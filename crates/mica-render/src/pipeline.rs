//! wgpu 30 pipeline: adapter/device setup, glyph atlas upload, instanced
//! cell pass. Pure GPU plumbing; instance building lives in frame.rs.

use bytemuck::cast_slice;
use wgpu::util::DeviceExt;

use crate::atlas::{Atlas, CELL_HEIGHT, CELL_WIDTH};
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
    cell: [f32; 2],
    atlas: [f32; 2],
    _pad: [f32; 2],
}

const INSTANCE_CAPACITY: usize = 4096;

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    globals_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_capacity: usize,
    atlas_size: [f32; 2],
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
        let atlas = Atlas::ascii();

        // R8 图集上传:行距须 256 字节对齐(wgpu COPY_BYTES_PER_ROW_ALIGNMENT)
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        let bytes_per_row = (atlas.width as usize).div_ceil(align) * align;
        let mut padded = vec![0u8; bytes_per_row * atlas.height as usize];
        for row in 0..atlas.height as usize {
            let src = row * atlas.width as usize;
            let dst = row * bytes_per_row;
            padded[dst..dst + atlas.width as usize]
                .copy_from_slice(&atlas.data[src..src + atlas.width as usize]);
        }
        let atlas_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("atlas upload"),
            contents: &padded,
            usage: wgpu::BufferUsages::COPY_SRC,
        });
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d {
                width: atlas.width,
                height: atlas.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        // wgpu 30:纹理拷贝在 CommandEncoder 上,Queue 没有 copy 方法
        let mut upload = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("atlas upload"),
        });
        upload.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &atlas_buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row as u32),
                    rows_per_image: None,
                },
            },
            texture.as_image_copy(),
            wgpu::Extent3d {
                width: atlas.width,
                height: atlas.height,
                depth_or_array_layers: 1,
            },
        );
        ctx.queue.submit([upload.finish()]);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyphs"),
            mag_filter: wgpu::FilterMode::Nearest, // 位图字体:最近邻保持锐利
            min_filter: wgpu::FilterMode::Nearest,
            ..wgpu::SamplerDescriptor::default()
        });

        let globals_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals"),
            contents: cast_slice(&[Globals {
                viewport: [1.0, 1.0],
                cell: [CELL_WIDTH as f32, CELL_HEIGHT as f32],
                atlas: [atlas.width as f32, atlas.height as f32],
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
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cells bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
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
            bind_group,
            globals_buf,
            instance_buf,
            instance_capacity,
            atlas_size: [atlas.width as f32, atlas.height as f32],
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
                cell: [CELL_WIDTH as f32, CELL_HEIGHT as f32],
                atlas: self.atlas_size,
                _pad: [0.0; 2],
            }]),
        );
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
            pass.set_bind_group(0, &self.bind_group, &[]);
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
    use super::is_srgb;
    use wgpu::TextureFormat;

    #[test]
    fn detects_srgb_swapchain_variants() {
        assert!(is_srgb(TextureFormat::Bgra8UnormSrgb));
        assert!(is_srgb(TextureFormat::Rgba8UnormSrgb));
        assert!(!is_srgb(TextureFormat::Bgra8Unorm));
        assert!(!is_srgb(TextureFormat::Rgba8Unorm));
    }
}
