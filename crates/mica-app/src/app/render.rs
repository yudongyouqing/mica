//! GPU surface/device initialization, device-lost recovery, frame coalescing
//! scheduling, and batched pane/strip/overlay frame rendering.
#![allow(unsafe_op_in_unsafe_fn)]

use super::*;
thread_local! {
    static STRIP_CACHE: RefCell<mica_render::strip_cache::StripCache> = RefCell::new(Default::default());
}

/// 窗口级 GPU 一次建:TAB_SEED 供 NewTab 复用启动设置。
pub(super) unsafe fn init_window_gpu(hwnd: HWND) -> anyhow::Result<WindowGpu> {
    // wgpu 30:display handle 挂在 Instance 上,窗口路线用无显示构造
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    // SAFETY: hwnd 活到进程结束,长于 wgpu_surface
    let wgpu_surface: wgpu::Surface<'static> = instance
        .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(raw_window_handle::RawDisplayHandle::Windows(
                raw_window_handle::WindowsDisplayHandle::new(),
            )),
            raw_window_handle: raw_window_handle::RawWindowHandle::Win32(
                raw_window_handle::Win32WindowHandle::new(
                    NonZeroIsize::new(hwnd.0 as isize).expect("hwnd is not null"),
                ),
            ),
        })
        .expect("create wgpu surface from HWND");

    let mut rect = RECT::default();
    GetClientRect(hwnd, &mut rect).expect("GetClientRect");
    let width = rect.right.max(1) as u32;
    let height = rect.bottom.max(1) as u32;

    let ctx = pollster::block_on(create_context(&instance, Some(&wgpu_surface)))
        .expect("no usable GPU adapter (need DX12/WARP)");
    let mut config = wgpu_surface
        .get_default_config(&ctx.adapter, width, height)
        .expect("surface unsupported by adapter");
    if config.format.is_srgb() {
        let caps = wgpu_surface.get_capabilities(&ctx.adapter);
        config.format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .expect("no non-srgb surface format");
    }
    wgpu_surface.configure(&ctx.device, &config);
    // device lost 双钩子(M5b/T2):丢失置静态标志(回调可能在 wgpu 内部
    // 线程),draw_frame 入口重建;未捕获错误记档(验证层报错有日志可查)
    ctx.device.set_device_lost_callback(|reason, msg| {
        GPU_LOST.store(true, std::sync::atomic::Ordering::SeqCst);
        eprintln!("[gpu] device lost: {reason:?} {msg}");
    });
    let err_handler: std::sync::Arc<dyn wgpu::UncapturedErrorHandler> =
        std::sync::Arc::new(|e: wgpu::Error| eprintln!("[wgpu] uncaptured error: {e}"));
    ctx.device.on_uncaptured_error(err_handler);
    let renderer = Renderer::new(&ctx, config.format)?;
    Ok(WindowGpu {
        ctx,
        renderer,
        wgpu_surface,
        config,
    })
}

/// device lost 后整套重建 GPU 资源(M5b/T2)。旧 WindowGpu 先 drop(旧
/// surface 归旧设备);新链按当前客户区尺寸重配。tab 侧不动——图集重传
/// 由 renderer_atlas_revision 重置驱动(与热重载同机制)。
fn rebuild_gpu_after_loss() {
    eprintln!("[gpu] device lost —— 重建渲染链");
    let Some(hwnd_raw) = SHARED_HWND.with(|s| s.borrow().clone()) else {
        return;
    };
    let Some(raw) = hwnd_raw.lock().ok().and_then(|g| *g) else {
        return;
    };
    // SAFETY: 哨兵非空即窗口存活(run() 退出序先置 None)
    unsafe {
        let mut gpu = match init_window_gpu(HWND(raw as _)) {
            Ok(gpu) => gpu,
            Err(error) => {
                eprintln!("GPU atlas rebuild deferred: {error}");
                GPU_LOST.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        };
        let palette = CURRENT_PALETTE.with(|p| *p.borrow());
        gpu.renderer.set_clear_color(&palette);
        if BG_IMG_PATH.with(|b| b.borrow().is_some()) {
            sync_background_image(&mut gpu.renderer);
        }
        GPU.with(|g| *g.borrow_mut() = Some(gpu));
    }
    TABS.with(|tabs| {
        for tab in tabs.borrow_mut().iter_mut() {
            for pane in tab.panes.iter_mut() {
                pane.terminal.force_full = true;
            }
        }
    });
}

// Scissors use attachment coordinates. Restore the fixed header offset when
// converting content height to the full client viewport extent.
pub(super) fn pane_scissor(
    rect: mica_core::layout::Rect,
    content_size: (f32, f32),
    content_top: f32,
) -> (u32, u32, u32, u32) {
    let sx = rect.x.max(0.0).round() as u32;
    let sy = rect.y.max(0.0).round() as u32;
    let sw = ((rect.x + rect.w).round() as u32)
        .min(content_size.0 as u32)
        .saturating_sub(sx);
    let sh_r = ((rect.y + rect.h).round() as u32)
        .min((content_size.1 + content_top) as u32)
        .saturating_sub(sy);
    (sx, sy, sw, sh_r)
}

/// T05 一次绘制批:pane id + scissor 矩形(px)+ 实例(该 pane 的图集批)
struct PaneBatch {
    pane_id: u64,
    scissor: (u32, u32, u32, u32),
    insts: Vec<mica_render::frame::CellInstance>,
    opacity: f32,
}

/// 合帧窗:数据到 → 5ms 内的同批输出并成一帧(打字回显无感,突发
/// 重绘整数量级合并;2026-10-07 定档)
pub(super) const COALESCE_FRAME_MS: u32 = 5;
/// 最大绘帧间隔:距上帧超此值即直绘,不让合帧窗在持续输出下变成
/// 无限推迟(WM_TIMER 低优先级,消息队列不空不投递)
pub(super) const MAX_FRAME_WAIT: std::time::Duration = std::time::Duration::from_millis(16);

/// 合帧调度决策(纯函数,表测见 sched_tests):输出到达时三选一。
/// 旧版每批 KillTimer+SetTimer **重置**计时——持续输出快于 5ms/批
/// 时绘制被无限顺延;改为"只武装一次",截止时间 = 首批到达 + 5ms,
/// 延迟有界;距上帧超 MAX_FRAME_WAIT 则绕过定时器直绘(饥饿兜底)。
#[derive(PartialEq, Eq, Debug)]
pub(super) enum FrameSched {
    /// 立即绘制(调用方撤定时器 + 清武装位)
    DrawNow,
    /// 武装 5ms 合帧定时器(仅未武装时)
    Arm,
    /// 已武装,本批并入待绘帧
    Pending,
}

pub(super) fn frame_sched(
    armed: bool,
    since_last_frame: Option<std::time::Duration>,
) -> FrameSched {
    match since_last_frame {
        Some(d) if d >= MAX_FRAME_WAIT => FrameSched::DrawNow,
        _ if armed => FrameSched::Pending,
        _ => FrameSched::Arm,
    }
}

pub(super) fn draw_frame() {
    // 帧节拍锚点:合帧兜底与 PerfDebug 都以"上次绘帧"为基准
    LAST_FRAME.with(|f| f.set(Some(std::time::Instant::now())));
    // device lost 重建(M5b/T2):下一帧发现丢失即整套重造(实例、适配
    // 器、surface、renderer),全部 tab 图集修订重置强制重传 + force_full。
    if GPU_LOST.swap(false, std::sync::atomic::Ordering::SeqCst) {
        rebuild_gpu_after_loss();
    }
    // 条高先取(动态:单标签隐藏第二行;取值含 TABS 借用,须在 guard 外)
    let _ = tab_scroll_adjust(0);
    let sh = strip_h();
    // T05 批次容器:TABS 闭包填充,GPU 阶段消费(闭包早退 = 空批,
    // GPU 阶段退化为纯清屏帧)
    let mut pane_batches: Vec<PaneBatch> = Vec::new();
    let mut strip_batch: (u64, Vec<mica_render::frame::CellInstance>) = (0, Vec::new());
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        // titles 先行收集(不可变借用),再取活跃标签可变借用
        let lock = TITLE_LOCK.with(|t| t.borrow().clone());
        let titles: Vec<(String, bool)> = guard
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                (
                    lock.clone().unwrap_or_else(|| tab.title.clone()),
                    i == ACTIVE.get(),
                )
            })
            .collect();
        let tab_colors = guard.iter().map(|tab| tab.color).collect();
        // pane 退出态(M5b/T3):strip 右侧标签(标题后缀会被长标题截断吃掉)
        let pane_exit = guard
            .get(ACTIVE.get())
            .and_then(|tab| tab.active_pane_ref())
            .and_then(|t| t.exited);
        let Some(active) = guard.get_mut(ACTIVE.get()) else {
            return;
        };
        active.dirty = false;
        // UIA Name 快照(M5c/T2):每帧刷新,Narrator 随时可读
        if let Ok(mut name) = uia::UIA_NAME.lock() {
            let summary = titles
                .get(ACTIVE.get())
                .map(|(t, _)| t.as_str())
                .unwrap_or("");
            *name = format!("Mica,{} 个标签,{}", titles.len(), summary);
        }

        // ---- T7 分屏渲染:客户区(减 strip)按布局树切给各 pane,逐 pane
        // build(各自己的 damage/router/光标)+ 实例偏移 pane 矩形;strip 文字走焦点 pane 的图集。
        let area_w = GPU
            .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.width as f32))
            .unwrap_or(0.0);
        let area_h = GPU
            .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.height as f32))
            .unwrap_or(0.0)
            - sh;
        use mica_core::layout::Rect;
        let area = terminal_area(area_w, area_h + sh, sh);
        let divider_width = DPI_SCALE.with(std::cell::Cell::get).round().max(1.0);
        let (rects, dividers) = active.layout.rects_and_dividers(area, divider_width);
        let focused_pane_id = active.active_pane_id().unwrap_or(0);
        let unfocused_opacity = UNFOCUSED_SPLIT_OPACITY.with(std::cell::Cell::get);

        // T05 分批绘制:每 pane 一批(自己的图集 + scissor 到 pane 矩形),
        // strip/搜索 overlay 是焦点图集的独立批(pane 区外,无 scissor)。
        for pane in active.panes.iter_mut() {
            let t = &mut pane.terminal;
            let rect = rects
                .iter()
                .find(|(id, _)| *id == pane.id)
                .map(|(_, r)| *r)
                .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0));
            if rect.w < 1.0 || rect.h < 1.0 {
                continue;
            }
            // 布局变了 → 网格按 pane 矩形重算(只在几何不匹配时)
            resize_terminal(t, rect);
            // Reclaim only before building any grid/strip/overlay instances.
            let max_dimension = GPU.with(|g| {
                g.borrow()
                    .as_ref()
                    .map_or(8192, |gpu| gpu.renderer.max_atlas_dimension())
            });
            let fingerprint = if t.router.needs_resource_recovery() {
                let shared_ui = (pane.id == focused_pane_id).then(|| {
                    (
                        &titles,
                        shortcuts_hidden(),
                        SEARCH_OPEN.with(std::cell::Cell::get),
                        SEARCH_QUERY.with(|q| q.borrow().clone()),
                        HOVER_URL.with(|h| h.borrow().as_ref().map(|(url, _, _)| url.clone())),
                        RENAME_OPEN.with(std::cell::Cell::get),
                        RENAME_QUERY.with(|q| q.borrow().clone()),
                        t.exited,
                    )
                });
                mica_render::frame::resource_fingerprint(&t.term, &shared_ui)
            } else {
                0
            };
            t.router.prepare_frame(fingerprint, max_dimension);
            let ver = t.router.atlas_version();
            if ver != t.last_atlas_ver {
                t.last_atlas_ver = ver;
                t.force_full = true;
            }
            let damage = if std::mem::take(&mut t.force_full) {
                let _ = t.term.take_damage(); // last_cursor 旋转契约
                Damage::Full
            } else {
                t.term.take_damage()
            };
            let display_offset = t.term.display_offset();
            let selection = t.term.selection_range();
            let cursor = t.term.cursor_shape();
            let cursor = if !CURSOR_BLINK.with(std::cell::Cell::get) {
                Some(cursor)
            } else {
                BLINK_PHASE.with(std::cell::Cell::get).then_some(cursor)
            };
            build_rows(
                &t.term,
                &mut t.router,
                &t.metrics,
                &t.palette,
                selection.as_ref(),
                display_offset,
                cursor,
                &damage,
                &mut t.row_insts,
            );
            let mut insts = repack(&t.row_insts);
            for inst in &mut insts {
                inst.pos_uv[0] += rect.x;
                inst.pos_uv[1] += rect.y;
            }
            if let Some(hover) = HOVER_PATH.with(|h| *h.borrow())
                && let Some(hl) = path_hover_rect(
                    hover,
                    pane.id,
                    rect,
                    &t.metrics,
                    t.term.display_offset(),
                    t.rows,
                )
            {
                let c = t.palette.colors[12];
                let color = [
                    c.r as f32 / 255.0,
                    c.g as f32 / 255.0,
                    c.b as f32 / 255.0,
                    0.0,
                ];
                let h = hl.h.min(2.0);
                insts.push(mica_render::frame::CellInstance {
                    pos_uv: [hl.x, hl.y + hl.h - h, 0.0, 0.0],
                    size_uv: [hl.w, h, 0.0, 0.0],
                    fg: color,
                    bg: color,
                });
            }
            let (sx, sy, sw, sh_r) = pane_scissor(rect, (area_w, area_h), sh);
            pane_batches.push(PaneBatch {
                pane_id: pane.id,
                scissor: (sx, sy, sw, sh_r),
                insts,
                opacity: if pane.id == focused_pane_id {
                    1.0
                } else {
                    unfocused_opacity
                },
            });
        }

        // strip:焦点 pane 的 router 路由标题(活跃 tab 列表)
        let Some(active) = guard.get_mut(ACTIVE.get()) else {
            return;
        };
        let Some(t) = active.active_pane() else {
            return;
        };
        // D37 状态点:活跃标签最近命令的 exit(B 清除/D 无参灰)
        let exit_dot = t.term.last_exit_code().map(|code| match code {
            Some(0) => mica_render::frame::ExitDot::Green,
            Some(_) => mica_render::frame::ExitDot::Red,
            None => mica_render::frame::ExitDot::Gray,
        });
        let sm = strip_metrics();
        if t.strip_router.is_none() {
            STRIP_CACHE.with(|c| c.borrow_mut().clear());
            let cfg = STRIP_CFG.with(|c| c.borrow().clone());
            let mut fams: Vec<String> = match &cfg.font_family {
                Some(f) => vec![f.clone()],
                None => TAB_SEED.with(|s| s.borrow().clone()).0,
            };
            for fb in [
                "Segoe UI",
                "Microsoft YaHei UI",
                "Segoe UI Symbol",
                "Segoe UI Emoji",
                "Segoe Fluent Icons",
            ] {
                if !fams.iter().any(|f| f.eq_ignore_ascii_case(fb)) {
                    fams.push(fb.to_string());
                }
            }
            let fams: Vec<&str> = fams.iter().map(String::as_str).collect();
            t.strip_router = DwriteRouter::new_ui(
                scaled_pt(cfg.font_size_pt),
                &fams,
                cfg.font_weight,
                Some(t.router.shared_atlas()),
            )
            .ok();
        }
        let Some(strip_r) = t.strip_router.as_mut() else {
            return; // 惰性建失败:本帧放弃,下帧重试
        };
        let ui_m = strip_r.metrics();
        let channel = |bg: u8, fg: u8| (f32::from(bg) * 0.86 + f32::from(fg) * 0.14) / 255.0;
        let (bg, fg) = (t.palette.bg, t.palette.fg);
        let line = [
            channel(bg.r, fg.r),
            channel(bg.g, fg.g),
            channel(bg.b, fg.b),
            0.0,
        ];
        let mut strip: Vec<_> = dividers
            .into_iter()
            .map(|r| mica_render::frame::CellInstance {
                pos_uv: [r.x, r.y, 0.0, 0.0],
                size_uv: [r.w, r.h, 0.0, 0.0],
                fg: line,
                bg: line,
            })
            .collect();
        if rects.len() > 1
            && let Some((_, rect)) = rects.iter().find(|(id, _)| *id == focused_pane_id)
        {
            let ink = [
                f32::from(fg.r) / 255.0,
                f32::from(fg.g) / 255.0,
                f32::from(fg.b) / 255.0,
                0.0,
            ];
            strip.push(mica_render::frame::CellInstance {
                pos_uv: [rect.x, rect.y, 0.0, 0.0],
                size_uv: [
                    (2.0 * sm.k).min(rect.w),
                    t.metrics.line_height.min(rect.h),
                    0.0,
                    0.0,
                ],
                fg: ink,
                bg: ink,
            });
        }
        let atlas_key = {
            let a = strip_r.atlas();
            (a.identity(), a.version())
        };
        STRIP_CACHE.with(|cache| {
            strip.extend_from_slice(
                cache.borrow_mut().get(
                    &titles,
                    strip_r,
                    &ui_m,
                    &t.palette,
                    &sm,
                    mica_render::frame::StripExtras {
                        drop_slot: tab_drag::drop_slot(),
                        window_title: lock.clone().or_else(|| t.term.cwd().map(str::to_owned)),
                        tab_colors,
                        tab_shortcuts: tab_shortcuts(titles.len()),
                        exit_dot,
                        caption_x: Some(area_w - 3.0 * sm.caption_w),
                        layout: mica_render::frame::StripLayout {
                            tab_scroll: TAB_SCROLL.with(std::cell::Cell::get),
                        },
                        pane_exit,
                        hover: HOVER_STRIP.with(std::cell::Cell::get),
                        zoomed: SHARED_HWND
                            .with(|h| h.borrow().as_ref().and_then(|s| *s.lock().unwrap()))
                            .is_some_and(|v| unsafe {
                                windows::Win32::UI::WindowsAndMessaging::IsZoomed(
                                    windows::Win32::Foundation::HWND(v as _),
                                )
                                .as_bool()
                            }),
                        focus: FOCUS_PHASE.with(std::cell::Cell::get),
                        capsule_dx: capsule_dx_now(),
                        drag: DRAG_TAB.with(std::cell::Cell::get).map(|i| {
                            (
                                i,
                                DRAG_SLOT.with(std::cell::Cell::get),
                                DRAG_X.with(std::cell::Cell::get),
                            )
                        }),
                        tab_dx: TAB_DX.with(|d| d.borrow().clone()),
                    },
                    atlas_key,
                ),
            )
        });
        // D28 hover 气泡:悬停链接的 URI overlay(锚点在 WM_MOUSEHOVER 记录)
        if let Some((url, hx, hy)) = HOVER_URL.with(|h| h.borrow().clone()) {
            strip.extend(mica_render::frame::tooltip_quads(
                &url,
                hx,
                hy,
                &mut t.router,
                &t.metrics,
                &t.palette,
            ));
        }
        // #57 搜索:命中高亮 + 底部搜索条,收集进独立 overlay
        let mut search_overlay: Vec<mica_render::frame::CellInstance> = Vec::new();
        if SEARCH_PANE.get() != Some(focused_pane_id) {
            SEARCH_OPEN.set(false);
        }
        let search_open = SEARCH_OPEN.with(std::cell::Cell::get);
        let rename_open = RENAME_OPEN.with(std::cell::Cell::get);
        if search_open || rename_open {
            let offset = t.term.display_offset() as i32;
            let rows = t.rows as i32;
            let cur = SEARCH_IDX.with(std::cell::Cell::get);
            let matches = if search_open {
                SEARCH_MATCHES.with(|m| m.borrow().clone())
            } else {
                Vec::new()
            };
            let yellow = [
                t.palette.colors[3].r,
                t.palette.colors[3].g,
                t.palette.colors[3].b,
            ];
            let mk_quad =
                |x: f32, y: f32, w: f32, h: f32, c: [u8; 3]| mica_render::frame::CellInstance {
                    pos_uv: [x, y, 0.0, 0.0],
                    size_uv: [w, h, 0.0, 0.0],
                    fg: [
                        c[0] as f32 / 255.0,
                        c[1] as f32 / 255.0,
                        c[2] as f32 / 255.0,
                        0.0,
                    ],
                    bg: [
                        c[0] as f32 / 255.0,
                        c[1] as f32 / 255.0,
                        c[2] as f32 / 255.0,
                        0.0,
                    ],
                };
            let pane_rect = rects
                .iter()
                .find(|(id, _)| *id == focused_pane_id)
                .map(|(_, r)| *r);
            if let Some(pr) = pane_rect {
                if search_open {
                    let mut hit_rects: Vec<(f32, f32, f32, f32, bool)> = Vec::new();
                    for (mi, (line, col, len)) in matches.iter().enumerate() {
                        let view = line + offset;
                        if !(0..rows).contains(&view) {
                            continue;
                        }
                        let (x, y, w) = (
                            pr.x + *col as f32 * t.metrics.cell_width,
                            pr.y + view as f32 * t.metrics.line_height,
                            *len as f32 * t.metrics.cell_width,
                        );
                        if mi == cur {
                            hit_rects.push((x, y, w, t.metrics.line_height, true));
                        } else {
                            let lh = t.metrics.line_height;
                            let b = 2.0;
                            search_overlay.push(mk_quad(x, y, w, b, yellow));
                            search_overlay.push(mk_quad(x, y + lh - b, w, b, yellow));
                            search_overlay.push(mk_quad(x, y, b, lh, yellow));
                            search_overlay.push(mk_quad(x + w - b, y, b, lh, yellow));
                        }
                    }
                    let hl_cur = [
                        yellow[0] as f32 / 255.0,
                        yellow[1] as f32 / 255.0,
                        yellow[2] as f32 / 255.0,
                        0.0,
                    ];
                    let ink_dark = [0.05, 0.05, 0.05, 0.0];
                    for batch in pane_batches.iter_mut() {
                        let batch_insts = &mut batch.insts;
                        for inst in batch_insts.iter_mut() {
                            let (ix, iy, iw, ih) = (
                                inst.pos_uv[0],
                                inst.pos_uv[1],
                                inst.size_uv[0],
                                inst.size_uv[1],
                            );
                            for &(hx, hy, hw, hh, _is_cur) in hit_rects.iter() {
                                if ix < hx + hw && ix + iw > hx && iy < hy + hh && iy + ih > hy {
                                    if inst.size_uv[2] > 0.0 {
                                        inst.fg = ink_dark;
                                        inst.bg = hl_cur;
                                    } else {
                                        inst.fg = hl_cur;
                                        inst.bg = hl_cur;
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
                let (bar, button) = search_bar_geometry(pr, &t.metrics);
                let blend = |bg: u8, fg: u8| ((u16::from(bg) * 7 + u16::from(fg)) / 8) as u8;
                let (bg, fg) = (t.palette.bg, t.palette.fg);
                let bar_bg = [blend(bg.r, fg.r), blend(bg.g, fg.g), blend(bg.b, fg.b)];
                search_overlay.push(mk_quad(bar.x, bar.y, bar.w, bar.h, bar_bg));
                let q = if rename_open {
                    RENAME_QUERY.with(|q| q.borrow().clone())
                } else {
                    SEARCH_QUERY.with(|q| q.borrow().clone())
                };
                let label = if rename_open {
                    if q.is_empty() {
                        "rename: (输入新标题)".into()
                    } else {
                        format!("rename: {q}")
                    }
                } else if q.is_empty() {
                    "搜索当前分屏".into()
                } else if matches.is_empty() {
                    format!("本分屏 · 无结果 · {q}")
                } else {
                    format!("本分屏 · {}/{} · {q}", cur + 1, matches.len())
                };
                let end = bar.x + bar.w;
                let labels = if rename_open {
                    vec![(label.as_str(), bar.x, end)]
                } else {
                    vec![
                        (label.as_str(), bar.x, end - 3.0 * button),
                        ("↑", end - 3.0 * button, end - 2.0 * button),
                        ("↓", end - 2.0 * button, end - button),
                        ("×", end - button, end),
                    ]
                };
                let ty = bar.y + 2.0;
                for (label, left, right) in labels {
                    let mut tx = left + 2.0;
                    for ch in label.chars() {
                        let g = <DwriteRouter as mica_render::font::router::GlyphRouter>::route(
                            &mut t.router,
                            ch,
                            mica_render::font::GlyphStyle::PLAIN,
                        );
                        if tx + g.offset_px[0] + g.size_px[0] > right
                            || ty + g.offset_px[1] + g.size_px[1] > bar.y + bar.h
                        {
                            break;
                        }
                        if g.size_px[0] > 0.0 {
                            search_overlay.push(mica_render::frame::CellInstance {
                                pos_uv: [
                                    tx + g.offset_px[0],
                                    ty + g.offset_px[1],
                                    g.uv[0],
                                    g.uv[1],
                                ],
                                size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                                fg: mica_render::frame::glyph_tint(
                                    [
                                        t.palette.fg.r as f32 / 255.0,
                                        t.palette.fg.g as f32 / 255.0,
                                        t.palette.fg.b as f32 / 255.0,
                                        0.0,
                                    ],
                                    g.color,
                                ),
                                bg: [
                                    bar_bg[0] as f32 / 255.0,
                                    bar_bg[1] as f32 / 255.0,
                                    bar_bg[2] as f32 / 255.0,
                                    0.0,
                                ],
                            });
                        }
                        tx += g.size_px[0].max(t.metrics.cell_width);
                    }
                }
            }
        }
        strip.extend(search_overlay);
        strip_batch = (focused_pane_id, strip);
    });
    // ---- T05 分批绘制:一次取帧、一次 clear、每批各绑各的图集、一次 present。
    GPU.with(|g| {
        let mut gpu_guard = g.borrow_mut();
        let Some(gpu) = gpu_guard.as_mut() else {
            return;
        };
        if !gpu.renderer.begin_frame(&gpu.wgpu_surface, &gpu.config) {
            return;
        }
        for batch in &pane_batches {
            let (pane_id, scissor, insts) = (&batch.pane_id, &batch.scissor, &batch.insts);
            if insts.is_empty() || scissor.2 == 0 || scissor.3 == 0 {
                continue;
            }
            TABS.with(|tabs| {
                let guard = tabs.borrow();
                let Some(p) = guard
                    .iter()
                    .find(|tab| tab.panes.iter().any(|p| p.id == *pane_id))
                    .and_then(|tab| tab.panes.iter().find(|p| p.id == *pane_id))
                else {
                    return; // 批构建后 pane 已关:整批丢弃
                };
                gpu.renderer.set_atlas(&p.terminal.router.atlas());
                gpu.renderer
                    .draw_batch_with_opacity(insts, Some(*scissor), batch.opacity);
            });
        }
        // strip + 搜索 overlay:焦点图集,无 scissor(strip 在 pane 区外)
        let (strip_pane, strip_insts) = &strip_batch;
        if !strip_insts.is_empty() {
            TABS.with(|tabs| {
                let guard = tabs.borrow();
                let Some(p) = guard
                    .iter()
                    .find(|tab| tab.panes.iter().any(|p| p.id == *strip_pane))
                    .and_then(|tab| tab.panes.iter().find(|p| p.id == *strip_pane))
                else {
                    return;
                };
                gpu.renderer.set_atlas(&p.terminal.router.atlas());
                gpu.renderer.draw_batch(strip_insts, None);
            });
        }
        gpu.renderer.end_frame();
    });
}

#[cfg(test)]
mod titlebar_perf {
    use super::*;
    use mica_render::frame::{StripExtras, StripMetrics};
    #[test]
    #[ignore = "CPU titlebar layout benchmark using DirectWrite"]
    fn titlebar_build_time() {
        let mut router =
            DwriteRouter::new_ui(11.0, &["Segoe UI", "Microsoft YaHei UI"], 600, None).unwrap();
        let sm = StripMetrics::default();
        let metrics = router.metrics();
        let palette = Palette::DEFAULT;
        for (name, title) in [
            ("short", "PowerShell".to_string()),
            ("long", "构建日志/Application/".repeat(256)),
        ] {
            let titles = vec![
                (title, true),
                ("tests".to_string(), false),
                ("docs".to_string(), false),
            ];
            for animated in [false, true] {
                let mut cache = mica_render::strip_cache::StripCache::default();
                let mut samples = Vec::new();
                for _ in 0..6 {
                    let start = std::time::Instant::now();
                    for i in 0..150 {
                        let extras = StripExtras {
                            caption_x: Some(840.0),
                            focus: if animated {
                                (i % 20) as f32 / 20.0
                            } else {
                                1.0
                            },
                            ..Default::default()
                        };
                        let owned = titles.clone();
                        let mut out = Vec::new();
                        let key = {
                            let a = router.atlas();
                            (a.identity(), a.version())
                        };
                        out.extend_from_slice(cache.get(
                            &owned,
                            &mut router,
                            &metrics,
                            &palette,
                            &sm,
                            extras,
                            key,
                        ));
                        std::hint::black_box(out);
                    }
                    samples.push(start.elapsed().as_nanos() / 150);
                }
                samples.remove(0);
                samples.sort_unstable();
                eprintln!(
                    "TITLEBAR {name} animated={animated} median_ns={}",
                    samples[2]
                );
            }
        }
    }
}
