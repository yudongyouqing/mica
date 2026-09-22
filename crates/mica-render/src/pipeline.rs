//! wgpu 30 pipeline: adapter/device setup, dynamic glyph atlas upload,
//! instanced cell pass. Pure GPU plumbing; instance building lives in frame.rs.

use bytemuck::cast_slice;
use wgpu::util::DeviceExt;

use crate::font::atlas::GlyphAtlas;
use crate::frame::{CellInstance, DEFAULT_BG};

/// Adapter + device + queue, created once per window.
pub struct GpuContext {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

/// Pick an adapter (preferring one that can present to `compatible_surface`)
/// and open a device. Async; drive with `pollster` at the call site.
pub async fn create_context(
    instance: &wgpu::Instance,
    compatible_surface: Option<&wgpu::Surface<'_>>,
) -> anyhow::Result<GpuContext> {
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("mica-render"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        })
        .await?;
    Ok(GpuContext {
        adapter,
        device,
        queue,
    })
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

const INSTANCE_CAPACITY: usize = 4096;

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    /// `set_atlas` 首次调用前为 None;draw 遇到它视为接线错误
    bind_group: Option<wgpu::BindGroup>,
    globals_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_capacity: usize,
    /// 已上传图集的 version;变化才重传
    atlas_version: u64,
    /// 持有当前图集纹理供 bind group 引用(替换时旧纹理随之释放)
    atlas_texture: Option<wgpu::Texture>,
    clear_color: wgpu::Color,
}

impl Renderer {
    pub fn new(ctx: &GpuContext, format: wgpu::TextureFormat) -> Self {
        // shader 不做 gamma 转换:sRGB swapchain 会把颜色二次编码
        // (深底洗浅、全色偏色)。宁可启动即失败,不可静默偏色。
        assert!(
            !is_srgb(format),
            "surface 格式必须选非 sRGB 变体(见计划 Task 7 执行者必读)"
        );
        let device = &ctx.device;

        let globals_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals"),
            contents: cast_slice(&[Globals {
                viewport: [1.0, 1.0],
                _pad: [0.0; 2],
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let instance_capacity = INSTANCE_CAPACITY;
        let instance_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("instances"),
            contents: &vec![0u8; instance_capacity * std::mem::size_of::<CellInstance>()],
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cells bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cells pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::include_wgsl!("cells.wgsl"));
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cells pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<CellInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 16,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 32,
                            shader_location: 2,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 48,
                            shader_location: 3,
                        },
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            device: ctx.device.clone(),
            queue: ctx.queue.clone(),
            pipeline,
            bgl,
            bind_group: None,
            globals_buf,
            instance_buf,
            instance_capacity,
            atlas_version: 0,
            atlas_texture: None,
            // 清屏色 = 默认背景:客户区非 8/16 整数倍时,右/下残余的
            // 不足一格像素会露出清屏色,与背景同色才不显突兀
            clear_color: wgpu::Color {
                r: f64::from(DEFAULT_BG.r) / 255.0,
                g: f64::from(DEFAULT_BG.g) / 255.0,
                b: f64::from(DEFAULT_BG.b) / 255.0,
                a: 1.0,
            },
        }
    }

    /// 图集更新(version 变化才重传;首次调用建立纹理与 bind group)。
    pub fn set_atlas(&mut self, atlas: &GlyphAtlas) {
        if self.atlas_version == atlas.version() && self.atlas_texture.is_some() {
            return;
        }
        self.atlas_version = atlas.version();
        let size = wgpu::Extent3d {
            width: atlas.width(),
            height: atlas.height(),
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        // wgpu 对所有纹理拷贝(含 write_texture)要求 bytes_per_row 按
        // COPY_BYTES_PER_ROW_ALIGNMENT(256)对齐,1 字节纹素也不例外。
        // GlyphAtlas 的宽度约定为 ≥256 的 2 的幂(初始值与 grow 的
        // next_power_of_two 都满足),故 width 本身即对齐行距,可零填充直传。
        // 若未来出现非 256 倍数的宽度,这里 debug 构建立即报错;修复路径是
        // padded 暂存上传(div_ceil(256)*256 逐行拷贝,参考旧 Atlas 上传)。
        debug_assert_eq!(
            atlas.width() % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT,
            0,
            "atlas width {} 不满足 bytes_per_row 的 256 对齐,需 padded 上传",
            atlas.width()
        );
        self.queue.write_texture(
            texture.as_image_copy(),
            atlas.texture(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atlas.width()),
                rows_per_image: Some(atlas.height()),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyphs"),
            mag_filter: wgpu::FilterMode::Linear, // 灰度 AA 需要
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        // 重建 bind_group(新 view/sampler);texture 存字段供 drop
        self.bind_group = Some(self.rebuild_bind_group(&view, &sampler));
        self.atlas_texture = Some(texture);
    }

    /// 用当前 globals 槽 + 给定的图集 view/sampler 重建 bind group。
    fn rebuild_bind_group(
        &self,
        view: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cells bg"),
            layout: &self.bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.globals_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }

    /// 全量重绘一帧(M0 策略,spec Global Constraints)。
    pub fn draw(
        &mut self,
        surface: &wgpu::Surface<'_>,
        config: &wgpu::SurfaceConfiguration,
        instances: &[CellInstance],
    ) {
        if instances.is_empty() {
            return;
        }
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture)
            | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            // 表面过期/丢失:重新 configure,下一帧恢复
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                surface.configure(&self.device, config);
                return;
            }
            _ => return, // Timeout/Occluded/Validation:跳过本帧
        };
        self.upload_instances(instances);
        self.queue.write_buffer(
            &self.globals_buf,
            0,
            cast_slice(&[Globals {
                viewport: [config.width as f32, config.height as f32],
                _pad: [0.0; 2],
            }]),
        );
        // set_atlas 之前 draw 是接线错误(T7 起先 set_atlas 再 draw)
        let bind_group = self
            .bind_group
            .as_ref()
            .expect("set_atlas() must be called before draw()");
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cells"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    // 全量重绘,但客户区非 8/16 整数倍时右/下边缘
                    // 不足一格的像素露清屏色 —— 用背景色使其融入
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.set_vertex_buffer(0, self.instance_buf.slice(..));
            pass.draw(0..6, 0..instances.len() as u32);
        }
        self.queue.submit([encoder.finish()]);
        // wgpu 30:present 移到了 Queue 上
        self.queue.present(frame);
    }

    fn upload_instances(&mut self, instances: &[CellInstance]) {
        let bytes = cast_slice(instances);
        if bytes.len() > self.instance_capacity * std::mem::size_of::<CellInstance>() {
            // 几何扩容:拖拽放大时 cell 数单调递增,按精确大小重建会逐帧分配
            let new_capacity = instances.len().max(self.instance_capacity * 2);
            self.instance_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instances"),
                size: (new_capacity * std::mem::size_of::<CellInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = new_capacity;
        }
        self.queue.write_buffer(&self.instance_buf, 0, bytes);
    }
}

/// shader 的颜色路径不做 gamma 转换,surface 格式不得为 sRGB 变体
/// (否则硬件二次编码,深色背景被洗浅、全色偏色)。
fn is_srgb(format: wgpu::TextureFormat) -> bool {
    format.is_srgb()
}

#[cfg(test)]
mod tests {
    use super::{GpuContext, Renderer, is_srgb};
    use crate::font::atlas::{GlyphAtlas, GlyphBitmap};
    use wgpu::TextureFormat;

    #[test]
    fn detects_srgb_swapchain_variants() {
        assert!(is_srgb(TextureFormat::Bgra8UnormSrgb));
        assert!(is_srgb(TextureFormat::Rgba8UnormSrgb));
        assert!(!is_srgb(TextureFormat::Bgra8Unorm));
        assert!(!is_srgb(TextureFormat::Rgba8Unorm));
    }

    /// 无 GPU 也可跑的纯逻辑测试之外的设备冒烟:cells.wgsl 的 naga 校验、
    /// 顶点布局 vs shader location 的一致性、set_atlas 的上传合法性,都要
    /// 真适配器才能验证(wgpu 的校验错误在这里 panic)。无适配器的环境跳过。
    #[test]
    fn pipeline_and_atlas_upload_validate_on_device() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        else {
            return; // 无适配器(headless/CI 限制):跳过,不当作失败
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("mica-render-test"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .expect("test device");
        let ctx = GpuContext {
            adapter,
            device: device.clone(),
            queue: queue.clone(),
        };
        // 非面格式(测试无 surface),非 sRGB 满足构造断言;
        // 管线创建会校验 shader 与 4×Float32x4 布局的一致性
        let mut renderer = Renderer::new(&ctx, TextureFormat::Bgra8Unorm);
        // 256 对齐宽度的图集:set_atlas 走完整纹理创建+直传路径
        let mut atlas = GlyphAtlas::new(256, 256);
        let bmp = GlyphBitmap {
            width: 8,
            height: 12,
            pixels: vec![255; 8 * 12],
        };
        let rect = atlas.insert(&bmp);
        assert!(rect.u + rect.w <= atlas.width());
        renderer.set_atlas(&atlas);
        // 同 version 再次调用:不应重传也不应 panic(version 门早退)
        renderer.set_atlas(&atlas);
        // 高 250 的字形在剩余空间放不下(12+250>256)→ grow 后 version 递增;
        // set_atlas 走"重建纹理+重传"路径(新尺寸 256×512)
        let big = GlyphBitmap {
            width: 8,
            height: 250,
            pixels: vec![128; 8 * 250],
        };
        atlas.insert(&big);
        assert!(atlas.version() > 0, "高度不足应触发 grow+版本递增");
        renderer.set_atlas(&atlas);
    }
}
