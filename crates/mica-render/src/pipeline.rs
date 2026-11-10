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

/// Cumulative counters from the actual wgpu upload/allocation path.
#[derive(Clone, Copy, Debug, Default)]
pub struct AtlasStats {
    pub uploads: usize,
    pub upload_bytes: u64,
    pub texture_creations: usize,
    pub bind_group_creations: usize,
    pub cache_hits: usize,
}
struct AtlasResource {
    texture: wgpu::Texture,
    bind_group: Option<wgpu::BindGroup>,
    revision: u64,
    generation: u64,
    touched: u64,
    life: std::sync::Weak<()>,
    _process_permit: crate::font::atlas::BytePermit,
    _window_permit: crate::font::atlas::BytePermit,
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
    /// Most recently bound content revision (diagnostic only).
    atlas_revision: u64,
    /// Actual queue.write_texture atlas uploads, excluding fallback setup.
    atlas_uploads: usize,
    /// Active aliases; stable resources live in the identity cache below.
    atlas_texture: Option<wgpu::Texture>,
    atlas_cache: std::collections::HashMap<u64, AtlasResource>,
    atlas_tick: u64,
    atlas_stats: AtlasStats,
    atlas_budget: std::sync::Arc<crate::font::atlas::ByteBudget>,
    atlas_window_budget: std::sync::Arc<crate::font::atlas::ByteBudget>,
    glyph_sampler: wgpu::Sampler,
    fallback_texture: wgpu::Texture,
    _fallback_permit: Option<crate::font::atlas::BytePermit>,
    _fallback_window_permit: Option<crate::font::atlas::BytePermit>,
    retirement_slot: Option<crate::font::atlas::BytePermit>,
    atlas_degraded: bool,
    last_limit_log: Option<std::time::Instant>,
    clear_color: wgpu::Color,
    /// #77 背景图纹理(持有供 bind group;None = 无图)
    bg_texture: Option<wgpu::Texture>,
    bg_view: Option<wgpu::TextureView>,
    bg_sampler: Option<wgpu::Sampler>,
    /// #77 当前背景参数(写 globals 的缓存)
    bg_param: [f32; 4],
    /// 帧目标(T05 分批绘制):begin_frame 建立,end_frame 呈现并取空。
    /// 分批期间持视图与呈现句柄;每批独立 encoder+submit(顶点缓冲逐批
    /// 覆写,同批内引用一致——合并 submit 会让先批画到后批内容)。
    frame: Option<FrameTarget>,
    /// 帧视口尺寸(scissor 钳制用)
    frame_extent: (u32, u32),
}

/// One process-wide nonblocking retirement pump also services GPU shells which
/// the Win32 driver workaround deliberately forgets at window close.
const MAX_RETIREMENT_CONTEXTS: usize = 32;

struct PendingRetirement {
    device: wgpu::Device,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    // Acquired before constructing a Renderer, transferred exactly once at
    // close. Active + queued + polling contexts share this practical bound.
    _slot: crate::font::atlas::BytePermit,
}

fn retirement_slots() -> std::sync::Arc<crate::font::atlas::ByteBudget> {
    static SLOTS: std::sync::OnceLock<std::sync::Arc<crate::font::atlas::ByteBudget>> =
        std::sync::OnceLock::new();
    SLOTS
        .get_or_init(|| crate::font::atlas::ByteBudget::new(MAX_RETIREMENT_CONTEXTS))
        .clone()
}

fn retirement_sender() -> anyhow::Result<&'static std::sync::mpsc::SyncSender<PendingRetirement>> {
    static PUMP: std::sync::OnceLock<Option<std::sync::mpsc::SyncSender<PendingRetirement>>> =
        std::sync::OnceLock::new();
    PUMP.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<PendingRetirement>(MAX_RETIREMENT_CONTEXTS);
        let worker = std::thread::Builder::new()
            .name("atlas-retirement".into())
            .spawn(move || {
                let mut pending = Vec::with_capacity(MAX_RETIREMENT_CONTEXTS);
                loop {
                    if pending.is_empty() {
                        match rx.recv() {
                            Ok(item) => pending.push(item),
                            Err(_) => return,
                        }
                    }
                    pending.extend(rx.try_iter());
                    pending.retain(|item| {
                        let _ = item.device.poll(wgpu::PollType::Poll);
                        !item.done.load(std::sync::atomic::Ordering::Acquire)
                    });
                    if !pending.is_empty() {
                        match rx.recv_timeout(std::time::Duration::from_millis(20)) {
                            Ok(item) => pending.push(item),
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        }
                    }
                }
            });
        match worker {
            Ok(_) => Some(tx),
            Err(error) => {
                eprintln!("Mica: cannot start atlas retirement worker: {error}");
                None
            }
        }
    })
    .as_ref()
    .ok_or_else(|| anyhow::anyhow!("atlas retirement service unavailable"))
}

fn pump_retirement(item: PendingRetirement) {
    let sender = match retirement_sender() {
        Ok(sender) => sender,
        Err(_) => {
            std::mem::forget(item);
            return;
        }
    };
    if let Err(error) = sender.try_send(item) {
        // Every item owns one of the 32 constructor slots, so a failed pump
        // can retain at most 32 items. Never release its slot early, spin,
        // launch a replacement thread, or block the window thread.
        eprintln!("Mica: retirement service unavailable; retaining bounded GPU context");
        let (std::sync::mpsc::TrySendError::Full(item)
        | std::sync::mpsc::TrySendError::Disconnected(item)) = error;
        std::mem::forget(item);
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        self.release_atlas_resources();
    }
}

/// 一帧的渲染目标:swapchain 纹理视图 + 呈现句柄(离屏测试为 None)。
struct FrameTarget {
    view: wgpu::TextureView,
    present: Option<wgpu::SurfaceTexture>,
    cleared: bool,
}

impl Renderer {
    pub fn new(ctx: &GpuContext, format: wgpu::TextureFormat) -> anyhow::Result<Self> {
        Self::with_atlas_budgets(
            ctx,
            format,
            crate::font::atlas::gpu_atlas_budget(),
            crate::font::atlas::WINDOW_GPU_ATLAS_BYTES,
        )
    }
    fn with_atlas_budgets(
        ctx: &GpuContext,
        format: wgpu::TextureFormat,
        atlas_budget: std::sync::Arc<crate::font::atlas::ByteBudget>,
        window_limit: usize,
    ) -> anyhow::Result<Self> {
        Self::with_resource_budgets(ctx, format, atlas_budget, window_limit, retirement_slots())
    }

    fn with_resource_budgets(
        ctx: &GpuContext,
        format: wgpu::TextureFormat,
        atlas_budget: std::sync::Arc<crate::font::atlas::ByteBudget>,
        window_limit: usize,
        slots: std::sync::Arc<crate::font::atlas::ByteBudget>,
    ) -> anyhow::Result<Self> {
        retirement_sender()?;
        let retirement_slot = slots
            .reserve(1)
            .ok_or_else(|| anyhow::anyhow!("active/retiring GPU context limit reached"))?;
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

        let atlas_window_budget = crate::font::atlas::ByteBudget::new(window_limit);
        let fallback_window_permit = atlas_window_budget
            .reserve(4)
            .expect("fallback window quota");
        let fallback_permit = atlas_budget
            .reserve(4)
            .ok_or_else(|| anyhow::anyhow!("process GPU atlas budget exhausted"))?;
        let fallback_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas budget replacement"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        ctx.queue.write_texture(
            fallback_texture.as_image_copy(),
            &[255, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let glyph_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyphs"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        Ok(Self {
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
            atlas_cache: std::collections::HashMap::new(),
            atlas_tick: 0,
            atlas_stats: AtlasStats::default(),
            atlas_budget,
            atlas_window_budget,
            glyph_sampler,
            fallback_texture,
            _fallback_permit: Some(fallback_permit),
            _fallback_window_permit: Some(fallback_window_permit),
            retirement_slot: Some(retirement_slot),
            atlas_degraded: false,
            last_limit_log: None,
            clear_color: bg_clear_color(&Palette::DEFAULT),
            bg_texture: None,
            bg_view: None,
            bg_sampler: None,
            bg_param: [0.0, 1.0, 1.0, 0.0],
            frame: None,
            frame_extent: (0, 0),
        })
    }

    /// 换肤时同步清屏色(app 在 `surface.set_palette` 后调用);
    /// 与格子背景同源,边缘残余像素才不会露出旧主题色。
    pub fn set_clear_color(&mut self, palette: &Palette) {
        self.clear_color = bg_clear_color(palette);
    }

    /// Release byte leases even when the app must deliberately forget the driver
    /// shell. Submitted textures and the fallback remain charged until work_done.
    pub fn release_atlas_resources(&mut self) {
        self.atlas_texture = None;
        self.bind_group = None;
        for (_, resource) in std::mem::take(&mut self.atlas_cache) {
            self.retire_atlas(resource);
        }
        if let Some(permit) = self._fallback_permit.take() {
            let window_permit = self._fallback_window_permit.take();
            let texture = self.fallback_texture.clone();
            self.queue.submit([]);
            self.queue.on_submitted_work_done(move || {
                texture.destroy();
                drop((permit, window_permit));
            });
        } else {
            return;
        }
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = done.clone();
        self.queue.on_submitted_work_done(move || {
            completed.store(true, std::sync::atomic::Ordering::Release)
        });
        if let Some(slot) = self.retirement_slot.take() {
            pump_retirement(PendingRetirement {
                device: self.device.clone(),
                done,
                _slot: slot,
            });
        }
    }
    pub fn atlas_owned_bytes(&self) -> usize {
        self.atlas_window_budget.used()
    }
    pub fn atlas_stats(&self) -> AtlasStats {
        self.atlas_stats
    }
    pub fn max_atlas_dimension(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }
    fn retire_atlas(&self, resource: AtlasResource) {
        // Pending draw/copy commands may still own this texture. Hold both byte
        // leases until its last submission completes; pressure cannot allocate
        // unbounded retired textures during the same frame.
        self.queue.submit([]);
        self.queue.on_submitted_work_done(move || {
            resource.texture.destroy();
            drop(resource);
        });
    }
    fn evict_atlas(&mut self) -> bool {
        let oldest = self
            .atlas_cache
            .iter()
            .min_by_key(|(_, r)| r.touched)
            .map(|(&id, _)| id);
        if let Some(resource) = oldest.and_then(|id| self.atlas_cache.remove(&id)) {
            self.retire_atlas(resource);
            true
        } else {
            false
        }
    }
    fn reserve_atlas(
        &self,
        bytes: usize,
    ) -> Option<(
        crate::font::atlas::BytePermit,
        crate::font::atlas::BytePermit,
    )> {
        let window = self.atlas_window_budget.reserve(bytes)?;
        let process = self.atlas_budget.reserve(bytes)?;
        Some((process, window))
    }
    /// True atlas identity, generation and committed revision are separate keys.
    /// Cached textures survive pane switches; append-only growth copies old GPU
    /// texels, then uploads only changes after that texture's committed revision.
    pub fn set_atlas(&mut self, atlas: &GlyphAtlas) {
        let _ = self.device.poll(wgpu::PollType::Poll);
        self.atlas_tick += 1;
        self.atlas_revision = atlas.revision();
        // Drop active clones before evicting so permits track live ownership.
        self.atlas_texture = None;
        self.bind_group = None;
        let dead: Vec<_> = self
            .atlas_cache
            .iter()
            .filter(|(_, r)| r.life.strong_count() == 0)
            .map(|(&id, _)| id)
            .collect();
        for id in dead {
            if let Some(resource) = self.atlas_cache.remove(&id) {
                self.retire_atlas(resource);
            }
        }
        if self.bg_sampler.is_none() {
            self.bg_sampler = Some(
                self.device
                    .create_sampler(&wgpu::SamplerDescriptor::default()),
            );
        }
        let id = atlas.identity();
        let mut old = self.atlas_cache.remove(&id);
        let size = wgpu::Extent3d {
            width: atlas.width(),
            height: atlas.height(),
            depth_or_array_layers: 1,
        };
        let same_size = old
            .as_ref()
            .is_some_and(|r| r.texture.size() == size && r.generation == atlas.version());
        let mut resource = if same_size {
            old.take().expect("same size requires resource")
        } else {
            let bytes = atlas.texture().len();
            let valid_size = atlas.width() <= self.max_atlas_dimension()
                && atlas.height() <= self.max_atlas_dimension();
            let mut permits = valid_size.then(|| self.reserve_atlas(bytes)).flatten();
            while permits.is_none() && self.evict_atlas() {
                permits = valid_size.then(|| self.reserve_atlas(bytes)).flatten();
            }
            // Only quota pressure waits; warm pane switches never synchronise.
            // All retired submissions are already flushed by retire_atlas.
            if permits.is_none() {
                let _ = self.device.poll(wgpu::PollType::Poll);
                permits = valid_size.then(|| self.reserve_atlas(bytes)).flatten();
            }
            if permits.is_none() {
                if let Some(previous) = old.take() {
                    self.retire_atlas(previous);
                }
                let _ = self.device.poll(wgpu::PollType::Poll);
                permits = valid_size.then(|| self.reserve_atlas(bytes)).flatten();
            }
            let Some((permit, window_permit)) = permits else {
                if !self.atlas_degraded
                    && self
                        .last_limit_log
                        .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(5))
                {
                    self.last_limit_log = Some(std::time::Instant::now());
                    eprintln!(
                        "Mica: GPU atlas resource limit; visible foreground replacement blocks until quota is released"
                    );
                }
                self.atlas_degraded = true;
                let view = self
                    .fallback_texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                self.bind_group = Some(self.rebuild_bind_group(&view, &self.glyph_sampler));
                self.atlas_texture = Some(self.fallback_texture.clone());
                return;
            };
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("glyph atlas"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            self.atlas_stats.texture_creations += 1;
            let mut revision = 0;
            if let Some(previous) = old.as_ref().filter(|r| {
                r.generation == atlas.version()
                    && r.texture.width() == atlas.width()
                    && r.texture.height() <= atlas.height()
            }) {
                let mut encoder = self
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                encoder.copy_texture_to_texture(
                    previous.texture.as_image_copy(),
                    texture.as_image_copy(),
                    previous.texture.size(),
                );
                self.queue.submit([encoder.finish()]);
                revision = previous.revision;
            }
            // New images need the pinned fallback even if revision == 0.
            if revision == 0 {
                self.upload_atlas_rect(
                    &texture,
                    atlas,
                    crate::font::atlas::Rect {
                        u: 0,
                        v: 0,
                        w: atlas.width(),
                        h: atlas.height(),
                    },
                );
                revision = atlas.revision();
            }
            AtlasResource {
                texture,
                bind_group: None,
                revision,
                generation: atlas.version(),
                touched: self.atlas_tick,
                life: atlas.lifetime(),
                _process_permit: permit,
                _window_permit: window_permit,
            }
        };
        if let Some(previous) = old.take() {
            self.retire_atlas(previous);
        }
        self.atlas_degraded = false;
        if resource.revision != atlas.revision() {
            if let Some(rect) = atlas.dirty_since(resource.revision) {
                self.upload_atlas_rect(&resource.texture, atlas, rect);
            }
            // write_texture validation succeeded; each renderer commits independently.
            resource.revision = atlas.revision();
        } else {
            self.atlas_stats.cache_hits += usize::from(same_size);
        }
        if resource.bind_group.is_none() {
            let view = resource
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            resource.bind_group = Some(self.rebuild_bind_group(&view, &self.glyph_sampler));
            self.atlas_stats.bind_group_creations += 1;
        }
        resource.touched = self.atlas_tick;
        self.atlas_texture = Some(resource.texture.clone());
        self.bind_group = resource.bind_group.clone();
        self.atlas_cache.insert(id, resource);
    }
    fn upload_atlas_rect(
        &mut self,
        texture: &wgpu::Texture,
        atlas: &GlyphAtlas,
        rect: crate::font::atlas::Rect,
    ) {
        if rect.w == 0 || rect.h == 0 {
            return;
        }
        let offset = ((rect.v * atlas.width() + rect.u) * 4) as usize;
        // Queue::write_texture permits an unaligned row stride (wgpu 30); unlike
        // encoder buffer copies it does not require 256-byte row alignment.
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.u,
                    y: rect.v,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &atlas.texture()[offset..],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atlas.width() * 4),
                rows_per_image: Some(rect.h),
            },
            wgpu::Extent3d {
                width: rect.w,
                height: rect.h,
                depth_or_array_layers: 1,
            },
        );
        self.atlas_uploads += 1;
        self.atlas_stats.uploads += 1;
        self.atlas_stats.upload_bytes += u64::from(rect.w) * u64::from(rect.h) * 4;
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
        let rgba = crate::background::load(p)?;
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
        for resource in self.atlas_cache.values_mut() {
            resource.bind_group = None;
        }
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

    /// 全量重绘一帧(M0 策略,spec Global Constraints)。单批便捷封装:
    /// begin_frame → draw_batch → end_frame;QT 与单 pane 路径用它。
    pub fn draw(
        &mut self,
        surface: &wgpu::Surface<'_>,
        config: &wgpu::SurfaceConfiguration,
        instances: &[CellInstance],
    ) {
        if instances.is_empty() {
            return;
        }
        if !self.begin_frame(surface, config) {
            return;
        }
        self.draw_batch(instances, None);
        self.end_frame();
    }

    /// 开始一帧(T05 分批绘制):取 swapchain 纹理,建立帧目标。
    /// 返回 false = 本帧跳过(表面过期会顺带重新 configure,下帧恢复)。
    /// begin/end 之间可用 draw_batch 画多批,每批各自绑定图集;
    /// **禁止嵌套**(上一帧未 end_frame 时 debug_assert)。
    pub fn begin_frame(
        &mut self,
        surface: &wgpu::Surface<'_>,
        config: &wgpu::SurfaceConfiguration,
    ) -> bool {
        debug_assert!(self.frame.is_none(), "上一帧未 end_frame(禁止嵌套帧)");
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture)
            | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            // 表面过期/丢失:重新 configure,下一帧恢复
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                surface.configure(&self.device, config);
                return false;
            }
            _ => return false, // Timeout/Occluded/Validation:跳过本帧
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.frame_extent = (config.width, config.height);
        self.frame = Some(FrameTarget {
            view,
            present: Some(frame),
            cleared: false,
        });
        true
    }

    /// 离屏帧入口(测试/无窗口渲染):直接给目标视图与视口,不呈现。
    pub fn begin_frame_offscreen(&mut self, view: &wgpu::TextureView, extent: (u32, u32)) {
        debug_assert!(self.frame.is_none(), "上一帧未 end_frame(禁止嵌套帧)");
        self.frame_extent = extent;
        self.frame = Some(FrameTarget {
            view: view.clone(),
            present: None,
            cleared: false,
        });
    }

    /// 画一批实例(T05):使用**当前绑定**的图集(set_atlas 已在批间切换),
    /// scissor 钳制到帧视口(None = 全视口)。首批 clear,后续 load——
    /// 分屏各 pane 依次入帧,最后一次 present。
    pub fn draw_batch(
        &mut self,
        instances: &[CellInstance],
        scissor: Option<(u32, u32, u32, u32)>,
    ) {
        if self.frame.is_none() {
            return; // 未 begin_frame:静默丢批(与旧 draw 空帧同哲学)
        }
        // set_atlas 之前画是接线错误(T7 起先 set_atlas 再画)
        let bind_group = self
            .bind_group
            .clone()
            .expect("set_atlas() must be called before draw_batch()");
        // globals 随批更新:atlas_size 是像素 UV 的归一化分母,分屏各
        // pane 图集尺寸可以不同,不能用帧首一次写入
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
        let (vp_w, vp_h) = self.frame_extent;
        self.queue.write_buffer(
            &self.globals_buf,
            0,
            cast_slice(&[Globals {
                viewport: [vp_w as f32, vp_h as f32],
                atlas_size: [atlas_w, atlas_h],
                bg: self.bg_param,
            }]),
        );
        self.upload_instances(instances);
        let cleared = self.frame.as_ref().map(|f| f.cleared).unwrap_or(false);
        let load = if cleared {
            wgpu::LoadOp::Load
        } else {
            // 全量重绘,但客户区非 8/16 整数倍时右/下边缘不足一格的像素
            // 露清屏色 —— 背景色使其融入
            wgpu::LoadOp::Clear(self.clear_color)
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame batch"),
            });
        {
            let view = self.frame.as_ref().map(|f| &f.view).unwrap();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cells"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load,
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
            pass.set_bind_group(0, &bind_group, &[]);
            pass.set_vertex_buffer(0, self.instance_buf.slice(..));
            if let Some((sx, sy, sw, sh)) = scissor {
                // 钳制到视口(wgpu 校验要求 scissor 完整落在附件内)
                let x = sx.min(vp_w.saturating_sub(1));
                let y = sy.min(vp_h.saturating_sub(1));
                let w = sw.min(vp_w - x);
                let h = sh.min(vp_h - y);
                if w > 0 && h > 0 {
                    pass.set_scissor_rect(x, y, w, h);
                }
            }
            pass.draw(0..6, 0..instances.len() as u32);
        }
        self.queue.submit([encoder.finish()]);
        if let Some(f) = self.frame.as_mut() {
            f.cleared = true;
        }
    }

    /// 结束一帧:全空帧也要保证清屏(否则呈现陈旧内容),随后呈现。
    pub fn end_frame(&mut self) {
        let Some(mut target) = self.frame.take() else {
            return;
        };
        if !target.cleared {
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("frame clear"),
                });
            {
                let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("clear only"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target.view,
                        resolve_target: None,
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
            }
            self.queue.submit([encoder.finish()]);
        }
        if let Some(frame) = target.present.take() {
            // wgpu 30:present 移到了 Queue 上
            self.queue.present(frame);
        }
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
    use crate::font::atlas::{GlyphAtlas, GlyphBitmap, GlyphFormat};
    use crate::frame::CellInstance;
    use mica_core::config::palette::Palette;
    use wgpu::TextureFormat;

    #[test]
    fn retiring_context_count_is_bounded_until_completion() {
        use crate::font::atlas::ByteBudget;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(ctx) = pollster::block_on(super::create_context(&instance, None)) else {
            eprintln!("no GPU adapter; retirement resource regression skipped");
            return;
        };
        let slots = ByteBudget::new(1);
        let done = Arc::new(AtomicBool::new(false));
        // Keep a real retirement job incomplete: new Renderers must not grow
        // a queue of device clones while an earlier GPU remains unresponsive.
        super::pump_retirement(super::PendingRetirement {
            device: ctx.device.clone(),
            done: done.clone(),
            _slot: slots.reserve(1).unwrap(),
        });
        let rejected = Renderer::with_resource_budgets(
            &ctx,
            TextureFormat::Rgba8Unorm,
            ByteBudget::new(1024 * 1024),
            1024 * 1024,
            slots.clone(),
        )
        .is_err();
        done.store(true, Ordering::Release);
        wait_quota(&ctx.device, &slots, 0);
        assert!(
            rejected,
            "active + retiring contexts must share the same capacity"
        );

        let mut renderer = Renderer::with_resource_budgets(
            &ctx,
            TextureFormat::Rgba8Unorm,
            ByteBudget::new(1024 * 1024),
            1024 * 1024,
            slots.clone(),
        )
        .unwrap();
        assert_eq!(slots.used(), 1, "released quota must permit a new renderer");
        renderer.release_atlas_resources();
        std::mem::forget(renderer); // same driver-shell workaround as Win32 close
        wait_quota(&ctx.device, &slots, 0);
    }

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

    fn wait_quota(device: &wgpu::Device, budget: &crate::font::atlas::ByteBudget, expected: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while budget.used() != expected && std::time::Instant::now() < deadline {
            device.poll(wgpu::PollType::Poll).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(budget.used(), expected, "GPU retirement callback quota");
    }

    fn read_pixels(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::Texture,
        size: wgpu::Extent3d,
    ) -> Vec<u8> {
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (256 * 64 * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("copy"),
        });
        enc.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256 * 4),
                    rows_per_image: Some(64),
                },
            },
            size,
        );
        queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            tx.send(r).unwrap();
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        rx.recv().unwrap().expect("map");
        let data = slice.get_mapped_range().expect("map range").to_vec();
        buf.unmap();
        data
    }

    /// T05 分屏图集归属的像素级回归:左右两批各绑自己的图集(相同 UV
    /// 槽、不同颜色),离屏渲染后回读——左半必须红(A 图集)、右半必须
    /// 蓝(B 图集)。修前形态(全部实例只绑焦点 pane 图集一次绘制)在
    /// 本测试的等价路径是"两批都绑 A":右半会呈红色,断言失败。
    #[test]
    fn split_batches_bind_own_atlas_pixels() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        else {
            return; // 无适配器:跳过,不当作失败(与既有设备测试同哲学)
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("mica-render-split-test"),
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
        let mut renderer = Renderer::new(&ctx, TextureFormat::Bgra8Unorm).unwrap();

        // 两个全新图集,首个插入位相同 → 相同 UV 槽;A 红 B 蓝
        let mk_atlas = |rgb: [u8; 3]| {
            let mut a = GlyphAtlas::new(256, 256);
            let px: Vec<u8> = (0..8 * 12)
                .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
                .collect();
            let bmp = GlyphBitmap {
                width: 8,
                height: 12,
                pixels: px,
                format: GlyphFormat::Coverage,
            };
            let rect = a.insert(&bmp);
            (a, rect)
        };
        let (atlas_a, rect_a) = mk_atlas([255, 0, 0]);
        let (atlas_b, rect_b) = mk_atlas([0, 0, 255]);
        assert_eq!(
            (rect_a.u, rect_a.v),
            (rect_b.u, rect_b.v),
            "两图集同槽(等价 UV)"
        );

        // 字形 quad:x 处 8×12 像素,采样整个字形槽;墨色白(rgb 直通)。
        // 注意 alpha 槽是圆角半径语义(0=方角),非 0 会进圆角路径跳过
        // 图集采样——真实字形实例同此约定(见 cells.wgsl 头注)
        let quad = |x: f32, rect: crate::font::atlas::Rect| CellInstance {
            pos_uv: [x, 26.0, rect.u as f32, rect.v as f32],
            size_uv: [8.0, 12.0, rect.w as f32, rect.h as f32],
            fg: [1.0, 1.0, 1.0, 0.0],
            bg: [0.0, 0.0, 0.0, 0.0],
        };

        // 离屏目标 256×64
        let size = wgpu::Extent3d {
            width: 256,
            height: 64,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // 批 1:绑 A,scissor 左半;批 2:绑 B,scissor 右半
        renderer.set_atlas(&atlas_a);
        renderer.begin_frame_offscreen(&view, (256, 64));
        renderer.draw_batch(&[quad(12.0, rect_a)], Some((0, 0, 128, 64)));
        renderer.set_atlas(&atlas_b);
        renderer.draw_batch(&[quad(140.0, rect_b)], Some((128, 0, 128, 64)));
        renderer.end_frame();

        let data = read_pixels(&device, &queue, &target, size);

        let px = |x: usize, y: usize| (data[(y * 256 + x) * 4], data[(y * 256 + x) * 4 + 2]); // (B, R)
        // 左半字形中心:红(B≈0, R≈255)
        let (lb, lr) = px(16, 32);
        assert!(
            lb < 32 && lr > 223,
            "左半必须是 A 图集的红色,得 B={lb} R={lr}"
        );
        // 右半字形中心:蓝(B≈255, R≈0)——修前错绑焦点图集时这里是红
        let (rb, rr) = px(144, 32);
        assert!(
            rb > 223 && rr < 32,
            "右半必须是 B 图集的蓝色,得 B={rb} R={rr}"
        );
        // A saturated process still renders ordinary backgrounds and a visible
        // replacement in *green* foreground (magenta*green used to turn black).
        let quota = crate::font::atlas::ByteBudget::new(4);
        let mut limited = Renderer::with_atlas_budgets(
            &ctx,
            TextureFormat::Bgra8Unorm,
            quota.clone(),
            1024 * 1024,
        )
        .unwrap();
        limited.set_atlas(&atlas_a);
        assert!(limited.atlas_degraded);
        let background = CellInstance {
            pos_uv: [0.0; 4],
            size_uv: [256.0, 64.0, 0.0, 0.0],
            fg: [0.0; 4],
            bg: [0.0; 4],
        };
        let mut foreground = quad(12.0, rect_a);
        foreground.fg = [0.0, 1.0, 0.0, 0.0];
        limited.begin_frame_offscreen(&view, (256, 64));
        limited.draw_batch(&[background, foreground], None);
        limited.end_frame();
        let fallback = read_pixels(&device, &queue, &target, size);
        let center = (32 * 256 + 16) * 4;
        assert_eq!(
            &fallback[center..center + 3],
            &[0, 255, 0],
            "fallback glyph must remain visible in green"
        );
        let blank = (32 * 256 + 100) * 4;
        assert_eq!(
            &fallback[blank..blank + 3],
            &[0, 0, 0],
            "blank background must not sample fallback ink"
        );
        assert_eq!(quota.used(), 4);
        limited.release_atlas_resources();
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        wait_quota(&device, &quota, 0);

        // Two windows share a hard logical resource budget, including fallback.
        let quota = crate::font::atlas::ByteBudget::new(256 * 256 * 4 + 8);
        let mut owner = Renderer::with_atlas_budgets(
            &ctx,
            TextureFormat::Bgra8Unorm,
            quota.clone(),
            1024 * 1024,
        )
        .unwrap();
        let mut peer = Renderer::with_atlas_budgets(
            &ctx,
            TextureFormat::Bgra8Unorm,
            quota.clone(),
            1024 * 1024,
        )
        .unwrap();
        owner.set_atlas(&atlas_a);
        peer.set_atlas(&atlas_b);
        assert!(peer.atlas_degraded);
        owner.release_atlas_resources();
        wait_quota(&device, &quota, 4);
        peer.set_atlas(&atlas_b);
        assert!(
            !peer.atlas_degraded,
            "unchanged revision retries after another window closes"
        );
        assert!(quota.used() <= 256 * 256 * 4 + 8);
        peer.release_atlas_resources();
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        wait_quota(&device, &quota, 0);

        // A single-window LRU repeatedly fills/evicts without escaping either
        // live or in-flight quota; retired leases remain counted until completion.
        let quota = crate::font::atlas::ByteBudget::new(2 * 256 * 256 * 4 + 4);
        let mut lru = Renderer::with_atlas_budgets(
            &ctx,
            TextureFormat::Bgra8Unorm,
            quota.clone(),
            256 * 256 * 4 + 4,
        )
        .unwrap();
        for _ in 0..10 {
            for atlas in [&atlas_a, &atlas_b] {
                lru.set_atlas(atlas);
                device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
                lru.set_atlas(atlas);
                assert!(!lru.atlas_degraded);
                assert!(lru.atlas_owned_bytes() <= 256 * 256 * 4 + 4);
                assert!(quota.used() <= 2 * 256 * 256 * 4 + 4);
            }
        }
        lru.release_atlas_resources();
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        wait_quota(&device, &quota, 0);
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
        let mut renderer = Renderer::new(&ctx, TextureFormat::Bgra8Unorm).unwrap();
        // 256 对齐宽度的图集:set_atlas 走完整纹理创建+直传路径
        let mut atlas = GlyphAtlas::new(256, 256);
        let bmp = GlyphBitmap::from_coverage(8, 12, vec![255; (8 * 12) as usize]);
        let rect = atlas.insert(&bmp);
        assert!(rect.u + rect.w <= atlas.width());
        renderer.set_atlas(&atlas);
        assert_eq!(renderer.atlas_uploads, 1);
        // 重复调用同一图集:无条件路径也必须正常(门控职责在 app 层)
        renderer.set_atlas(&atlas);
        assert_eq!(
            renderer.atlas_uploads, 1,
            "same identity and revision needs no upload"
        );
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
        assert_eq!(
            renderer.atlas_uploads, 2,
            "different identities upload once each"
        );
        // C1 回归锁:普通插入(不触发 grow)version 不动但 revision 递增,
        // 调用方据此触发 set_atlas——本函数无条件消费,永不漏传
        let plain = GlyphBitmap::from_coverage(6, 9, vec![200; (6 * 9) as usize]);
        let before_plain = renderer.atlas_stats();
        atlas.insert(&plain);
        assert_eq!(atlas.version(), 0, "此插入不应触发 grow");
        renderer.set_atlas(&atlas);
        // 直接读私有字段:诊断值随最近一次上传更新
        assert_eq!(renderer.atlas_revision, atlas.revision());
        assert_eq!(renderer.atlas_uploads, 3);
        assert_eq!(
            renderer.atlas_stats().upload_bytes - before_plain.upload_bytes,
            6 * 9 * 4
        );
        assert_eq!(
            renderer.atlas_stats().texture_creations,
            before_plain.texture_creations
        );
        // 高 250 的字形在剩余空间放不下 → 页追加(高度 +256,条目原位);
        // set_atlas 走"重建纹理+重传"路径(页追加式,version 恒 0)
        let big = GlyphBitmap::from_coverage(8, 250, vec![128; (8 * 250) as usize]);
        let before_ver = atlas.version();
        atlas.insert(&big);
        assert_eq!(atlas.version(), before_ver, "页追加式无重排信号");
        assert!(atlas.height() > 256, "高度追加");
        let before_growth = renderer.atlas_stats();
        renderer.set_atlas(&atlas);
        assert_eq!(
            renderer.atlas_stats().texture_creations,
            before_growth.texture_creations + 1
        );
        assert_eq!(
            renderer.atlas_stats().upload_bytes - before_growth.upload_bytes,
            8 * 250 * 4,
            "growth copies old GPU image; only new glyph pixels cross the CPU/GPU boundary"
        );
        let warm = renderer.atlas_stats();
        for _ in 0..20 {
            renderer.set_atlas(&other);
            renderer.set_atlas(&atlas);
        }
        assert_eq!(
            renderer.atlas_stats().texture_creations,
            warm.texture_creations
        );
        assert_eq!(renderer.atlas_stats().upload_bytes, warm.upload_bytes);
        assert_eq!(
            renderer.atlas_stats().bind_group_creations,
            warm.bind_group_creations
        );
        eprintln!(
            "atlas GPU probe: stats={:?}, owned={} bytes; plain 6x9=216 upload bytes, growth 8x250=8000 bytes, 40 alternating binds=0 bytes/0 allocations",
            renderer.atlas_stats(),
            renderer.atlas_owned_bytes()
        );
        // Swap the same atlas through a different renderer/device-consumer.
        let mut second = Renderer::new(&ctx, TextureFormat::Bgra8Unorm).unwrap();
        second.set_atlas(&atlas);
        let before_second = second.atlas_stats();
        let tiny = GlyphBitmap::from_coverage(2, 2, vec![255; 4]);
        atlas.insert(&tiny);
        renderer.set_atlas(&atlas);
        second.set_atlas(&atlas);
        assert_eq!(
            second.atlas_stats().upload_bytes - before_second.upload_bytes,
            16,
            "dirty state commits independently for each renderer"
        );
        // 换肤路径:构造后的 set_clear_color 更新内部清屏色(读私有字段验证)
        let mut themed = Palette::DEFAULT;
        themed.apply_pair("background", "#abcdef").unwrap();
        renderer.set_clear_color(&themed);
        assert_eq!(renderer.clear_color, bg_clear_color(&themed));
    }
}
