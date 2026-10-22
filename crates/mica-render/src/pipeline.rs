//! wgpu 30 pipeline: adapter/device setup, dynamic RGBA glyph atlas upload,
//! instanced cell pass. Pure GPU plumbing; instance building lives in frame.rs.

use bytemuck::cast_slice;
use wgpu::util::DeviceExt;

use mica_core::config::palette::Palette;

use crate::font::atlas::GlyphAtlas;
use crate::frame::CellInstance;

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
    // 适配器择优(2026-10-06 真机事故定案):request_adapter(LowPower)
    // 在装了远控虚拟显卡的机器上会随机选中虚拟适配器——表面兼容但呈现
    // 落空,整窗停留在未初始化显存(症状:窗口内容为每次不同的杂色、
    // 残影错位,与渲染逻辑无关;基线构建同样复现)。改为显式枚举:
    // 离散 > 集成 > 其余(VirtualGpu/Cpu 仅兜底),并日志留痕——下次
    // 选卡异常一眼可辨。
    let adapter = {
        let candidates: Vec<wgpu::Adapter> = instance
            .enumerate_adapters(wgpu::Backends::all())
            .await
            .into_iter()
            .filter(|a| compatible_surface.is_none_or(|s| a.is_surface_supported(s)))
            .collect();
        let rank = |a: &wgpu::Adapter| match a.get_info().device_type {
            wgpu::DeviceType::DiscreteGpu => 0,
            wgpu::DeviceType::IntegratedGpu => 1,
            _ => 2,
        };
        let best = candidates
            .iter()
            .min_by_key(|a| (rank(a), a.get_info().name.clone()))
            .ok_or_else(|| anyhow::anyhow!("no surface-compatible adapter"))?;
        let info = best.get_info();
        eprintln!(
            "adapter: {} ({:?}, {} {}, backend {:?})",
            info.name, info.device_type, info.driver, info.driver_info, info.backend
        );
        best.clone()
    };
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
    /// 图集纹理尺寸(纹素):UV 归一移到 shader,实例存纹素坐标——
    /// 页追加式高度增长不改变既有条目采样位置(2026-10-08)
    atlas_size: [f32; 2],
    /// #77 背景图 (alpha, w, h, pad):alpha<0.5 = 无图(空 quad 直通);
    /// 有图时空白 quad 的底混采样图(cover 平铺,alpha 即混合强度
    /// = background-opacity 的图向部分)
    bg: [f32; 4],
}

const INSTANCE_CAPACITY: usize = 4096;

/// 清屏色 = 调色板背景:客户区非 8/16 整数倍时,右/下残余的
/// 不足一格像素会露出清屏色,与背景同色才不显突兀。
fn bg_clear_color(palette: &Palette) -> wgpu::Color {
    wgpu::Color {
        r: f64::from(palette.bg.r) / 255.0,
        g: f64::from(palette.bg.g) / 255.0,
        b: f64::from(palette.bg.b) / 255.0,
        a: 1.0,
    }
}

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
    /// 最近一次上传的 revision(诊断值,不做门控——见 set_atlas 注释)
    atlas_revision: u64,
    /// set_atlas 实际执行次数。回归锁的观测面:窗口级纹理被多个
    /// pane 的图集轮流占用,任何"按内容/数值跳过上传"的内部门都会
    /// 在此计数上现形(两次调用必须两次计数)
    atlas_uploads: usize,
    /// 持有当前图集纹理供 bind group 引用(替换时旧纹理随之释放)
    atlas_texture: Option<wgpu::Texture>,
    clear_color: wgpu::Color,
    /// #77 背景图纹理(持有供 bind group;None = 无图)
    bg_texture: Option<wgpu::Texture>,
    bg_view: Option<wgpu::TextureView>,
    bg_sampler: Option<wgpu::Sampler>,
    /// #77 当前背景参数(写 globals 的缓存)
    bg_param: [f32; 4],
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
                atlas_size: [1.0, 1.0],
                bg: [0.0, 1.0, 1.0, 0.0],
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
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
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
            atlas_revision: 0,
            atlas_uploads: 0,
            atlas_texture: None,
            clear_color: bg_clear_color(&Palette::DEFAULT),
            bg_texture: None,
            bg_view: None,
            bg_sampler: None,
            bg_param: [0.0, 1.0, 1.0, 0.0],
        }
    }

    /// 换肤时同步清屏色(app 在 `surface.set_palette` 后调用);
    /// 与格子背景同源,边缘残余像素才不会露出旧主题色。
    pub fn set_clear_color(&mut self, palette: &Palette) {
        self.clear_color = bg_clear_color(palette);
    }

    /// 图集上传(无条件执行;首次调用建立纹理与 bind group)。
    /// 1.0.10 教训:这里曾按"revision 数值相等"早退——但窗口级纹理会被
    /// 多个 pane 的图集轮流占用,字符集相似的两个标签 revision 计数完全
    /// 可能相同,数值门必撞车漏传(用户稳定复现:多标签点回第一个全
    /// 乱码;app 层 a883e6e 的身份门被这道内门废掉)。"GPU 里装的是谁
    /// 的图集"只有调用方知道(app 的 WindowGpu::atlas_owner 按 pane
    /// 身份判断),本函数只忠实上传。insert 改纹理内容、grow 改尺寸,
    /// 都由调用方的 revision 触发到这里,无内门则永不漏。
    pub fn set_atlas(&mut self, atlas: &GlyphAtlas) {
        self.atlas_revision = atlas.revision();
        self.atlas_uploads += 1;
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
            format: wgpu::TextureFormat::Rgba8Unorm, // RGBA 图集(D19):灰度 RGB 白 A=coverage,彩色原色
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        // wgpu 对所有纹理拷贝(含 write_texture)要求 bytes_per_row 按
        // COPY_BYTES_PER_ROW_ALIGNMENT(256)对齐,1 字节纹素也不例外。
        // GlyphAtlas 的宽度约定为 ≥256 的 2 的幂(初始值与 grow 的
        // next_power_of_two 都满足),故 width 本身即对齐行距,可零填充直传。
        // 若未来出现非 256 倍数的宽度,这里 debug 构建立即报错;修复路径是
        // padded 暂存上传(div_ceil(256)*256 逐行拷贝,参考旧 Atlas 上传)。
        // RGBA8:每行 = width*4 字节;宽度恒为 256 的幂 → ×4 仍 256 对齐
        debug_assert_eq!(
            (atlas.width() * 4) % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT,
            0,
            "atlas width*4 {} 不满足 bytes_per_row 的 256 对齐",
            atlas.width() * 4
        );
        self.queue.write_texture(
            texture.as_image_copy(),
            atlas.texture(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atlas.width() * 4),
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
        if self.bg_sampler.is_none() {
            self.bg_sampler = Some(self.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("bg"),
                ..wgpu::SamplerDescriptor::default()
            }));
        }
        self.bind_group = Some(self.rebuild_bind_group(&view, &sampler));
        self.atlas_texture = Some(texture);
    }

    /// 用当前 globals 槽 + 给定的图集 view/sampler 重建 bind group。
    /// #77:binding 3/4 = 背景图;无图时占位(bind group 布局静态)
    fn rebuild_bind_group(
        &self,
        view: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        let (bg_view, bg_sampler) = self.bg_resources();
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
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&bg_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&bg_sampler),
                },
            ],
        })
    }

    /// #77 背景 (view, sampler) 句柄:有图取真源,无图建 1x1 占位
    fn bg_resources(&self) -> (wgpu::TextureView, wgpu::Sampler) {
        if let (Some(v), Some(s)) = (self.bg_view.clone(), self.bg_sampler.clone()) {
            return (v, s);
        }
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bg placeholder"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = self
            .device
            .create_sampler(&wgpu::SamplerDescriptor::default());
        (view, sampler)
    }

    /// #77 设置背景图:解码(png/jpeg/bmp/webp)→ 上传 → 重绑。
    /// opacity 0..1 为图的混合强度;None 关图。长边钳 4096(显存折中)
    pub fn set_background_image(
        &mut self,
        path: Option<&std::path::Path>,
        opacity: f32,
    ) -> Result<(), String> {
        let Some(p) = path else {
            self.bg_texture = None;
            self.bg_view = None;
            self.bg_param = [0.0, 1.0, 1.0, 0.0];
            self.rebind_after_bg();
            return Ok(());
        };
        let img = image::open(p).map_err(|e| format!("背景图打开失败 {}: {e}", p.display()))?;
        let (iw, ih) = (img.width(), img.height());
        let scale = if iw.max(ih) > 4096 {
            4096.0 / iw.max(ih) as f32
        } else {
            1.0
        };
        let rgba = if scale < 1.0 {
            image::imageops::resize(
                &img,
                (iw as f32 * scale) as u32,
                (ih as f32 * scale) as u32,
                image::imageops::FilterType::Lanczos3,
            )
        } else {
            img.to_rgba8()
        };
        let (w, h) = (rgba.width(), rgba.height());
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bg image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        // w*4 不满足 256 对齐时逐行零垫(padded 布局)
        let stride = w * 4;
        let aligned = stride.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let mut buf = Vec::with_capacity((aligned * h) as usize);
        for y in 0..h {
            buf.extend_from_slice(&rgba.as_raw()[(y * w * 4) as usize..((y + 1) * w * 4) as usize]);
            buf.resize(buf.len() + (aligned - stride) as usize, 0);
        }
        self.queue.write_texture(
            texture.as_image_copy(),
            &buf,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(aligned),
                rows_per_image: Some(h),
            },
            size,
        );
        self.bg_texture = Some(texture.clone());
        self.bg_view = Some(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        self.bg_param = [opacity.clamp(0.0, 1.0), w as f32, h as f32, 0.0];
        self.rebind_after_bg();
        Ok(())
    }

    /// #77 bg 变更后重建 bind group(用已存图集 view 的克隆)
    fn rebind_after_bg(&mut self) {
        let Some(tex) = self.atlas_texture.clone() else {
            return;
        };
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyphs"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        self.bind_group = Some(self.rebuild_bind_group(&view, &sampler));
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
        let atlas_w = self
            .atlas_texture
            .as_ref()
            .map(|t| t.width() as f32)
            .unwrap_or(1.0);
        let atlas_h = self
            .atlas_texture
            .as_ref()
            .map(|t| t.height() as f32)
            .unwrap_or(1.0);
        self.queue.write_buffer(
            &self.globals_buf,
            0,
            cast_slice(&[Globals {
                viewport: [config.width as f32, config.height as f32],
                atlas_size: [atlas_w, atlas_h],
                bg: self.bg_param,
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
    use super::{GpuContext, Renderer, bg_clear_color, is_srgb};
    use crate::font::atlas::{GlyphAtlas, GlyphBitmap};
    use mica_core::config::palette::Palette;
    use wgpu::TextureFormat;

    #[test]
    fn detects_srgb_swapchain_variants() {
        assert!(is_srgb(TextureFormat::Bgra8UnormSrgb));
        assert!(is_srgb(TextureFormat::Rgba8UnormSrgb));
        assert!(!is_srgb(TextureFormat::Bgra8Unorm));
        assert!(!is_srgb(TextureFormat::Rgba8Unorm));
    }

    /// 清屏色数学(u8/255)随 palette.bg 走——换肤后边缘残余像素同色。
    #[test]
    fn clear_color_follows_palette_bg() {
        let c = bg_clear_color(&Palette::DEFAULT);
        assert_eq!(c.r, f64::from(0x28) / 255.0);
        assert_eq!(c.g, f64::from(0x2c) / 255.0);
        assert_eq!(c.b, f64::from(0x34) / 255.0);
        assert_eq!(c.a, 1.0);
        let mut p = Palette::DEFAULT;
        p.apply_pair("background", "#abcdef").unwrap();
        let c = bg_clear_color(&p);
        assert_eq!(c.r, f64::from(0xab) / 255.0);
        assert_eq!(c.g, f64::from(0xcd) / 255.0);
        assert_eq!(c.b, f64::from(0xef) / 255.0);
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
        let bmp = GlyphBitmap::from_coverage(8, 12, vec![255; (8 * 12) as usize]);
        let rect = atlas.insert(&bmp);
        assert!(rect.u + rect.w <= atlas.width());
        renderer.set_atlas(&atlas);
        assert_eq!(renderer.atlas_uploads, 1);
        // 重复调用同一图集:无条件路径也必须正常(门控职责在 app 层)
        renderer.set_atlas(&atlas);
        assert_eq!(renderer.atlas_uploads, 2);
        // 撞车回归锁(1.0.10 二段修复):两份内容不同、revision 计数相同
        // 的图集——旧的内门按数值相等早退,第二份静默漏传=切标签乱码
        // 的机制本体。现在必须两次都真实上传。
        let mut other = GlyphAtlas::new(256, 256);
        let other_bmp = GlyphBitmap::from_coverage(10, 10, vec![90; 10 * 10]);
        other.insert(&other_bmp);
        assert_eq!(
            other.revision(),
            atlas.revision(),
            "前提:两图集 revision 计数此刻相同"
        );
        renderer.set_atlas(&other);
        renderer.set_atlas(&atlas);
        assert_eq!(renderer.atlas_uploads, 4, "数值撞车不得跳过上传");
        // C1 回归锁:普通插入(不触发 grow)version 不动但 revision 递增,
        // 调用方据此触发 set_atlas——本函数无条件消费,永不漏传
        let plain = GlyphBitmap::from_coverage(6, 9, vec![200; (6 * 9) as usize]);
        atlas.insert(&plain);
        assert_eq!(atlas.version(), 0, "此插入不应触发 grow");
        renderer.set_atlas(&atlas);
        // 直接读私有字段:诊断值随最近一次上传更新
        assert_eq!(renderer.atlas_revision, atlas.revision());
        assert_eq!(renderer.atlas_uploads, 5);
        // 高 250 的字形在剩余空间放不下 → 页追加(高度 +256,条目原位);
        // set_atlas 走"重建纹理+重传"路径(页追加式,version 恒 0)
        let big = GlyphBitmap::from_coverage(8, 250, vec![128; (8 * 250) as usize]);
        let before_ver = atlas.version();
        atlas.insert(&big);
        assert_eq!(atlas.version(), before_ver, "页追加式无重排信号");
        assert!(atlas.height() > 256, "高度追加");
        renderer.set_atlas(&atlas);
        // 换肤路径:构造后的 set_clear_color 更新内部清屏色(读私有字段验证)
        let mut themed = Palette::DEFAULT;
        themed.apply_pair("background", "#abcdef").unwrap();
        renderer.set_clear_color(&themed);
        assert_eq!(renderer.clear_color, bg_clear_color(&themed));
    }
}
