//! Tab and split-pane lifecycle, PTY render forwarder, grid/DPI resizing,
//! IME composition positioning, and tab switching/closing.
#![allow(unsafe_op_in_unsafe_fn)]

use super::*;

/// 字号随 DPI:DWrite 光栅按 96dpi 像素出图,pt×scale 才与屏幕物理一致。
/// reload_dpi 一直这么乘,其余建 router 站点曾全漏(单 150% 屏无
/// WM_DPICHANGED,字体永远 ×1.0)——2026-10-06 统一单源
pub(crate) fn scaled_pt(base_pt: f32) -> f32 {
    (base_pt * DPI_SCALE.with(std::cell::Cell::get)).clamp(6.0, 72.0)
}

/// All layout, input hit testing and grid sizing share the configured padding.
pub(super) fn terminal_area(width: f32, height: f32, strip: f32) -> mica_core::layout::Rect {
    let scale = DPI_SCALE.with(std::cell::Cell::get);
    let (x, y) = CONTENT_PADDING.with(|p| *p.borrow());
    let (x, y) = (x * scale, y * scale);
    mica_core::layout::Rect::new(
        x,
        strip + y,
        (width - 2.0 * x).max(1.0),
        (height - strip - 2.0 * y).max(1.0),
    )
}

pub(super) fn grid_size(area: mica_core::layout::Rect, metrics: &FontMetrics) -> (u16, u16) {
    (
        ((area.w / metrics.cell_width).max(1.0)) as u16,
        ((area.h / metrics.line_height).max(1.0)) as u16,
    )
}

pub(super) fn resize_terminal(terminal: &mut Terminal, area: mica_core::layout::Rect) -> bool {
    let (cols, rows) = grid_size(area, &terminal.metrics);
    if (cols, rows) == (terminal.cols, terminal.rows) {
        return false;
    }
    terminal.cols = cols;
    terminal.rows = rows;
    terminal
        .term
        .resize(ScreenSize::new(cols as usize, rows as usize));
    terminal.term.set_cell_metrics(
        terminal.metrics.cell_width.round() as u16,
        terminal.metrics.line_height.round() as u16,
    );
    let _ = terminal.session.resize(cols, rows);
    terminal.force_full = true;
    true
}

/// Resize every tab using its own layout, including inactive split panes.
pub(super) fn resize_all_grids(width: f32, height: f32) -> bool {
    let area = terminal_area(width, height, strip_h());
    TABS.with(|tabs| {
        let mut changed = false;
        for tab in tabs.borrow_mut().iter_mut() {
            let rects = tab.layout.rects(area);
            for pane in &mut tab.panes {
                if let Some((_, rect)) = rects.iter().find(|(id, _)| *id == pane.id) {
                    changed |= resize_terminal(&mut pane.terminal, *rect);
                }
            }
        }
        changed
    })
}

/// pty → 渲染转发线程(T7,app 所有;pty.rs 零 GUI 纪律:core 不碰 win32)。
/// 泊在 `PtyReader::recv_event` 上独占消费 reader 通道(单消费者,顺序天然
/// 保序):数据到达先落共享缓冲,再向窗口 Post WM_APP_RENDER。
pub(super) fn spawn_render_forwarder(
    reader: PtyReader,
    backlog: Arc<PtyBacklog>,
    _pane_id: u64,
    epoch: u64,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("pty-forwarder".into())
        .spawn(move || {
            use mica_core::pty::RecvEvent;
            // T08:push 满则阻塞(背压链),close/换代 Err 退场
            loop {
                match reader.recv_event(Duration::from_millis(200)) {
                    RecvEvent::Data(chunk) => match backlog.push(epoch, &chunk) {
                        Ok(()) => {}
                        Err(_) => return, // 关闭或换代:本线程退场
                    },
                    RecvEvent::Timeout => {} /* 唤醒重试由 monitor 专职 */
                    RecvEvent::Closed => return, // pty 读线程挂断
                }
            }
        })
        .expect("spawn pty forwarder")
}

/// 轮询各 pane 子进程退出(M5b/T3):首次探得退出记 code。返回是否有新
/// 退出(调用方触发重绘)。EOF 后读线程自然静默,1s 轮询补上"退出"事实。
pub(super) fn poll_exits() -> bool {
    let mut changed = false;
    TABS.with(|tabs| {
        for tab in tabs.borrow_mut().iter_mut() {
            for pane in tab.panes.iter_mut() {
                let t = &mut pane.terminal;
                if t.exited.is_none()
                    && let Ok(Some(st)) = t.session.try_wait()
                {
                    t.exited = Some(st.exit_code());
                    changed = true;
                }
            }
        }
    });
    changed
}

/// 重开活跃 pane(M5b/T3):同 shell 新 pty,复用 pane 槽/id/图集/网格;
/// surface 换新(旧画面属旧会话,重开即干净起步),退出态清零。
/// 会话族(permit/写队列/回收)全走 `lifecycle::restart_session_fields`
/// 共享流——失败 pane 留 Stopped 态可重试,渲染侧接线只属本窗。
pub(super) fn restart_active_pane() -> bool {
    let Some((cols, rows)) = TABS.with(|tabs| {
        tabs.borrow_mut()
            .get_mut(ACTIVE.get())
            .and_then(|tab| tab.active_pane())
            .map(|t| (t.cols, t.rows))
    }) else {
        return false;
    };
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        let Some(tab) = guard.get_mut(ACTIVE.get()) else {
            return;
        };
        let Some(pid) = tab.active_pane_id() else {
            return;
        };
        let Some(pane) = tab.pane_by_id(pid) else {
            return;
        };
        let t = &mut pane.terminal;
        let restarted = lifecycle::restart_session_fields(
            &mut t.session,
            &mut t.session_permit,
            &mut t.writeq,
            &mut t.input_stage,
            &t.pty_buf,
            &t.shell,
            cols,
            rows,
            &mut t.exited,
        );
        let Ok((reader, new_gen)) = restarted else {
            eprintln!("[restart] {:?}", restarted.err().unwrap());
            return;
        };
        let mut term = Surface::new(ScreenSize::new(cols as usize, rows as usize));
        term.set_cell_metrics(
            t.metrics.cell_width.round() as u16,
            t.metrics.line_height.round() as u16,
        );
        term.set_palette(&t.palette);
        term.set_clipboard_provider(Arc::new(|| clipboard::get_text().unwrap_or_default()));
        t.term = term;
        t.row_insts.clear();
        t.force_full = true;
        t.session_gen = new_gen;
        let pty_buf = Arc::clone(&t.pty_buf);
        if let Some(slot) = SHARED_HWND.with(|s| s.borrow().clone()) {
            let _ = &slot;
            spawn_render_forwarder(reader, pty_buf, pid, new_gen);
        }
    });
    true
}

/// DPI 变更(M5b/T4):更新缩放系数,全部 pane 按新有效字号重建
/// router/度量/网格(热重载同机制);配置 pt 不动(可逆)。
pub(super) unsafe fn reload_dpi(hwnd: HWND, dpi: u32) {
    let scale = dpi as f32 / 96.0;
    if (DPI_SCALE.with(std::cell::Cell::get) - scale).abs() < f32::EPSILON {
        return;
    }
    DPI_SCALE.with(|d| d.set(scale));
    let (families, base_pt) = TAB_SEED.with(|s| s.borrow().clone());
    let families: Vec<&str> = if families.is_empty() {
        DEFAULT_FAMILIES.to_vec()
    } else {
        families.iter().map(String::as_str).collect()
    };
    let size_pt = scaled_pt(base_pt);
    let pane_count: usize = TABS.with(|tabs| tabs.borrow().iter().map(|tab| tab.panes.len()).sum());
    let mut routers = Vec::with_capacity(pane_count.max(1));
    for _ in 0..pane_count.max(1) {
        let Ok(router) = DwriteRouter::new(size_pt, &families) else {
            eprintln!("[dpi] 字体链重建失败,保持旧缩放观感");
            return;
        };
        routers.push(router);
    }
    let metrics = routers[0].metrics();
    let mut routers = routers.into_iter();
    let mut rect = RECT::default();
    GetClientRect(hwnd, &mut rect).expect("GetClientRect");
    eprintln!("[dpi] {dpi} (x{scale:.2}) → pt {base_pt}→{size_pt}");
    TABS.with(|tabs| {
        for tab in tabs.borrow_mut().iter_mut() {
            for pane in tab.panes.iter_mut() {
                let t = &mut pane.terminal;
                if let Some(router) = routers.next() {
                    t.router = router;
                }
                t.strip_router = None;
                t.metrics = metrics;
                t.term.set_cell_metrics(
                    metrics.cell_width.round() as u16,
                    metrics.line_height.round() as u16,
                );
                t.force_full = true;
            }
        }
    });
    resize_all_grids(rect.right.max(1) as f32, rect.bottom.max(1) as f32);
}

/// IME 组合窗/候选窗定位(M5b/T5):跟随焦点 pane 的终端光标格。
/// 坐标链:pane 原点 + 光标格(客户区)→ ClientToScreen → ImmSet*。
pub(super) fn position_ime(hwnd: HWND) {
    let Some((cx, cy)) = TABS.with(|tabs| {
        let guard = tabs.borrow();
        let tab = guard.get(ACTIVE.get())?;
        let pid = tab.active_pane_id()?;
        let t = tab.active_pane_ref()?;
        let rect = active_pane_rects()
            .into_iter()
            .find(|(id, _)| *id == pid)
            .map(|(_, r)| r)?;
        let grid = t.term.grid();
        let col = grid.cursor.point.column.0 as f32;
        let line = grid.cursor.point.line.0.max(0) as f32;
        Some((
            rect.x + (col + 0.5) * t.metrics.cell_width,
            rect.y + (line + 1.0) * t.metrics.line_height, // 候选框排在光标行下缘
        ))
    }) else {
        return;
    };
    // SAFETY: 坐标换算与 IMM 上下文,输出/句柄本侧承接
    unsafe {
        let mut pt = windows::Win32::Foundation::POINT {
            x: cx as i32,
            y: cy as i32,
        };
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut pt);
        let himc = windows::Win32::UI::Input::Ime::ImmGetContext(hwnd);
        if himc.is_invalid() {
            return;
        }
        let comp = windows::Win32::UI::Input::Ime::COMPOSITIONFORM {
            dwStyle: windows::Win32::UI::Input::Ime::CFS_POINT,
            ptCurrentPos: pt,
            rcArea: RECT::default(),
        };
        let _ = windows::Win32::UI::Input::Ime::ImmSetCompositionWindow(himc, &comp);
        let cand = windows::Win32::UI::Input::Ime::CANDIDATEFORM {
            dwIndex: 0,
            dwStyle: windows::Win32::UI::Input::Ime::CFS_EXCLUDE,
            ptCurrentPos: pt,
            rcArea: RECT {
                left: pt.x,
                top: pt.y,
                right: pt.x,
                bottom: pt.y,
            },
        };
        let _ = windows::Win32::UI::Input::Ime::ImmSetCandidateWindow(himc, &cand);
        let _ = windows::Win32::UI::Input::Ime::ImmReleaseContext(hwnd, himc);
    }
}

/// 活跃标签的 pane 矩形表(鼠标路由与 draw_frame 同源几何)。
pub(super) fn active_pane_rects() -> Vec<(u64, mica_core::layout::Rect)> {
    let (aw, ah) = GPU
        .with(|g| {
            g.borrow()
                .as_ref()
                .map(|gpu| (gpu.config.width as f32, gpu.config.height as f32))
        })
        .unwrap_or((1.0, 1.0));
    let area = terminal_area(aw, ah, strip_h());
    TABS.with(|tabs| {
        tabs.borrow()
            .get(ACTIVE.get())
            .map(|tab| tab.layout.rects(area))
            .unwrap_or_default()
    })
}

/// 焦点 pane 在布局树里的最近邻(方向几何:候选中心与当前中心的方向向量
/// 点积最大且分量同向)——Alt+方向跳焦点(spec §6)。
pub(super) fn focus_neighbor(dir: Key) {
    let sh = strip_h();
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        let Some(tab) = guard.get_mut(ACTIVE.get()) else {
            return;
        };
        let Some(cur_id) = tab.active_pane_id() else {
            return;
        };
        let (aw, ah) = GPU
            .with(|g| {
                g.borrow()
                    .as_ref()
                    .map(|gpu| (gpu.config.width as f32, gpu.config.height as f32))
            })
            .unwrap_or((1.0, 1.0));
        let area = terminal_area(aw, ah, sh);
        let rects = tab.layout.rects(area);
        let Some((_, cur)) = rects.iter().find(|(id, _)| *id == cur_id) else {
            return;
        };
        let cur_c = (cur.x + cur.w / 2.0, cur.y + cur.h / 2.0);
        let (dx, dy) = match dir {
            Key::Left => (-1.0, 0.0),
            Key::Right => (1.0, 0.0),
            Key::Up => (0.0, -1.0),
            Key::Down => (0.0, 1.0),
            _ => return,
        };
        let mut best: Option<(f32, usize)> = None;
        for (i, (id, r)) in rects.iter().enumerate() {
            if *id == cur_id {
                continue;
            }
            let c = (r.x + r.w / 2.0, r.y + r.h / 2.0);
            let (vx, vy) = (c.0 - cur_c.0, c.1 - cur_c.1);
            if dx * vx + dy * vy <= 0.0 {
                continue;
            }
            let dist_sq = vx * vx + vy * vy;
            if best.is_none_or(|(b, _)| dist_sq < b) {
                best = Some((dist_sq, i));
            }
        }
        if let Some((_, i)) = best
            && let Some(pane_pos) = tab.panes.iter().position(|p| p.id == rects[i].0)
        {
            tab.focused = pane_pos;
            for p in tab.panes.iter_mut() {
                p.terminal.force_full = true;
            }
        }
    });
    draw_frame();
}

/// 关闭焦点 pane:树塌缩 + pty Drop;最后一个 pane = 关标签。
pub(super) fn close_pane_action() {
    let mut close_tab_idx: Option<usize> = None;
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        let Some(tab) = guard.get_mut(ACTIVE.get()) else {
            return;
        };
        let Some(pid) = tab.active_pane_id() else {
            return;
        };
        if tab.panes.len() == 1 {
            close_tab_idx = Some(ACTIVE.get()); // 末 pane = 关标签
            return;
        }
        let removed = tab.layout.remove(pid);
        if !removed {
            return;
        }
        if let Some(pos) = tab.panes.iter().position(|p| p.id == pid) {
            let removed = tab.panes.remove(pos);
            lifecycle::close_terminal(removed.terminal);
        }
        tab.focused = tab.focused.min(tab.panes.len() - 1);
        for p in tab.panes.iter_mut() {
            p.terminal.force_full = true;
        }
    });
    if let Some(idx) = close_tab_idx {
        close_tab(idx);
    } else {
        draw_frame();
    }
}

/// 活跃标签分屏:布局 split + 新 pane(独立 pty/forwarder,WPARAM=pane id)。
pub(super) unsafe fn split_pane(hwnd: HWND, dir: mica_core::layout::SplitDir) {
    let Some(slot) = SHARED_HWND.with(|s| s.borrow().clone()) else {
        return;
    };
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        let Some(tab) = guard.get_mut(ACTIVE.get()) else {
            return;
        };
        let Some(target) = tab.active_pane_id() else {
            return;
        };
        let (pane, reader) = match create_pane(hwnd, &launch_command(None)) {
            Ok(pair) => pair,
            Err(e) => {
                warn_box(&e);
                return;
            }
        };
        let pane_id = pane.id;
        let pty_buf = Arc::clone(&pane.terminal.pty_buf);
        let fwd_epoch = pane.terminal.session_gen;
        tab.layout.split(target, dir, pane_id);
        tab.panes.push(pane);
        tab.focused = tab.panes.len() - 1; // 新 pane 焦点(WT 语义)
        for p in tab.panes.iter_mut() {
            p.terminal.force_full = true;
        }
        let _ = &slot;
        spawn_render_forwarder(reader, pty_buf, pane_id, fwd_epoch);
    });
}

pub(super) unsafe fn create_pane(
    hwnd: HWND,
    shell_command: &str,
) -> Result<(PaneState, PtyReader), String> {
    create_pane_with_size(hwnd, shell_command, None, None)
}

/// T9:font_size 覆盖 seed 字号(Some 时),其余同 [`create_pane`]。
/// cwd(OSC 7 继承,2026-10-07):Some = 新 pane 在此目录起 shell
pub(super) unsafe fn create_pane_with_size(
    hwnd: HWND,
    shell_command: &str,
    font_size: Option<f32>,
    cwd: Option<&str>,
) -> Result<(PaneState, PtyReader), String> {
    let (families, size_pt) = TAB_SEED.with(|s| s.borrow().clone());
    let size_pt = font_size
        .filter(|s| (6.0..=72.0).contains(s))
        .unwrap_or(size_pt);
    let size_pt = scaled_pt(size_pt);
    let families: Vec<&str> = if families.is_empty() {
        DEFAULT_FAMILIES.to_vec()
    } else {
        families.iter().map(String::as_str).collect()
    };
    let router = DwriteRouter::new(size_pt, &families)
        .map_err(|e| format!("Font/atlas resource unavailable: {e}"))?;
    let metrics = router.metrics();
    let palette = CURRENT_PALETTE.with(|p| *p.borrow());

    let mut rect = RECT::default();
    GetClientRect(hwnd, &mut rect).expect("GetClientRect");
    let area = terminal_area(
        rect.right.max(1) as f32,
        rect.bottom.max(1) as f32,
        strip_h(),
    );
    let (cols, rows) = grid_size(area, &metrics);
    let mut term = Surface::new(ScreenSize::new(cols as usize, rows as usize));
    term.set_cell_metrics(
        metrics.cell_width.round() as u16,
        metrics.line_height.round() as u16,
    );
    term.set_palette(&palette);
    term.set_clipboard_provider(std::sync::Arc::new(|| {
        clipboard::get_text().unwrap_or_default()
    }));
    let mut cmd = mica_core::pty::command_from_str(shell_command);
    if let Some(dir) = cwd {
        cmd.cwd(std::path::PathBuf::from(dir));
    }
    if !lifecycle::spawn_capacity_ok() {
        return Err("会话回收通道故障(通道满/断),暂不能开新标签——关闭部分标签后自动恢复".into());
    }
    let session_permit =
        lifecycle::acquire_session().ok_or("并发会话已达上限(16)——关闭部分标签/窗口后重试")?;
    let probe = orphan::before_spawn();
    let (session, reader) = PtySession::spawn(cmd, cols, rows)
        .map_err(|e| format!("无法启动 shell({shell_command}):{e}"))?;
    if let Some(pid) = session.child_pid() {
        orphan::track_session_since(&probe, pid);
    }
    static NEXT_PANE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let pane_id = NEXT_PANE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let slot = SHARED_HWND.with(|s| s.borrow().clone());
    let pty_buf = PtyBacklog::new(
        pane_id,
        pty_bridge::PANE_BUDGET_BYTES,
        Arc::new(pty_bridge::WindowWake {
            slot: slot.clone().unwrap_or_else(|| Arc::new(Mutex::new(None))),
            msg: WM_APP_RENDER,
        }),
    );
    pty_buf.spawn_monitor();
    let writeq = WriteQueue::new();
    let writer = session.writer_handle();
    {
        let wq = Arc::clone(&writeq);
        std::thread::Builder::new()
            .name("pty-writer".into())
            .spawn(move || pty_bridge::writer_loop(wq, Arc::new(pty_bridge::SessionSink(writer))))
            .expect("spawn pty writer");
    }

    let terminal = Terminal {
        term,
        session,
        session_permit: Some(session_permit),
        pty_buf: Arc::clone(&pty_buf),
        writeq: Arc::clone(&writeq),
        input_stage: pty_bridge::InputStage::new(),
        session_gen: 0,
        router,
        metrics,
        palette,
        cols,
        rows,
        row_insts: Vec::new(),
        force_full: true,
        shell: shell_command.to_string(),
        exited: None,
        strip_router: None,
        last_atlas_ver: 0,
    };
    Ok((
        PaneState {
            id: pane_id,
            terminal,
        },
        reader,
    ))
}

/// 建标签并接入渲染链(T7):create_pane 包成 Tab → 入池 → forwarder → 置为活跃。
pub(super) unsafe fn start_tab(hwnd: HWND) {
    start_tab_with_profile(hwnd, None, None);
}

/// 带 profile 的建标签(M3a):IPC new-tab / CLI 注入。
pub(super) unsafe fn start_tab_with_profile(
    hwnd: HWND,
    profile: Option<&str>,
    font_size: Option<f32>,
) -> bool {
    let profiles = mica_core::profile::cached_profiles();
    let picked = profile.and_then(|name| {
        let p = profiles
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
            .cloned();
        if p.is_none() {
            eprintln!("ipc/CLI: 未知 profile `{name}`,回落 PowerShell");
        }
        p
    });
    let shell = launch_command(picked.as_ref());
    let title = picked
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "PowerShell".to_string());
    let inherit = TABS.with(|tabs| {
        tabs.borrow()
            .get(ACTIVE.get())
            .and_then(|tab| tab.active_pane_ref())
            .and_then(|t| t.term.cwd().map(str::to_string))
    });
    start_tab_with_shell(hwnd, &shell, font_size, inherit.as_deref(), title)
}

/// 带 shell/cwd/标题的建标签(#35 tear-off 与 profile 路径的公共底):
pub(super) unsafe fn start_tab_with_shell(
    hwnd: HWND,
    shell: &str,
    font_size: Option<f32>,
    cwd: Option<&str>,
    title: String,
) -> bool {
    let old_active = ACTIVE.get();
    let (pane, reader) = match create_pane_with_size(hwnd, shell, font_size, cwd) {
        Ok(pair) => pair,
        Err(e) => {
            warn_box(&e);
            return false;
        }
    };
    let pane_id = pane.id;
    let tab = TabState {
        title,
        color: None,
        dirty: false,
        panes: vec![pane],
        layout: mica_core::layout::Layout::leaf(pane_id),
        focused: 0,
    };
    let id = tab.panes[0].id;
    let pty_buf = Arc::clone(&tab.panes[0].terminal.pty_buf);
    let fwd_epoch = tab.panes[0].terminal.session_gen;
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        guard.push(tab);
        TAB_COUNT.with(|c| c.set(guard.len()));
    });
    ACTIVE.with(|a| a.set(TABS.with(|tabs| tabs.borrow().len() - 1)));
    prime_capsule_slide(old_active, ACTIVE.get());
    ensure_anim_timer();
    tab_follow_active();
    if let Some(slot) = SHARED_HWND.with(|s| s.borrow().clone()) {
        let _ = &slot;
        spawn_render_forwarder(reader, pty_buf, id, fwd_epoch);
    }
    true
}

/// 切到指定下标的标签:置活跃 + force_full(strip 也要换高亮)+ 重绘。
pub(super) fn switch_tab(idx: usize) {
    let old = ACTIVE.get();
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        if idx >= guard.len() {
            return;
        }
        ACTIVE.with(|a| a.set(idx));
        if let Some(tab) = guard.get_mut(idx) {
            for p in tab.panes.iter_mut() {
                p.terminal.force_full = true;
            }
        }
    });
    prime_capsule_slide(old, idx);
    ensure_anim_timer();
    tab_follow_active();
    draw_frame();
}

/// 关闭指定标签:Drop 杀 pty(既有契约)→ 读线程 EOF → forwarder 自然退。
/// 关的是活跃标签则邻位顶上;池空 → 退出应用。
pub(super) fn close_tab(idx: usize) {
    let mut quit = false;
    let old_active = ACTIVE.get();
    let mut new_active: Option<usize> = None;
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        if idx >= guard.len() {
            return;
        }
        let removed = guard.remove(idx);
        {
            let first = removed.panes.first().map(|p| {
                (
                    p.terminal.shell.clone(),
                    p.terminal.term.cwd().map(str::to_string),
                )
            });
            if let Some((shell, cwd)) = first {
                CLOSED_TABS.with(|h| {
                    let mut hist = h.borrow_mut();
                    if hist.len() >= 8 {
                        hist.remove(0);
                    }
                    hist.push(ClosedTab {
                        shell,
                        cwd,
                        title: removed.title.clone(),
                        color: removed.color,
                    });
                });
            }
        }
        for pane in removed.panes {
            lifecycle::close_terminal(pane.terminal);
        }
        let len = guard.len();
        TAB_COUNT.with(|c| c.set(len));
        if len == 0 {
            quit = true;
            return;
        }
        let active = ACTIVE.get().min(len - 1);
        ACTIVE.with(|a| a.set(active));
        new_active = Some(active);
        if let Some(tab) = guard.get_mut(active) {
            for p in tab.panes.iter_mut() {
                p.terminal.force_full = true;
            }
        }
    });
    if quit {
        let hwnd = SHARED_HWND
            .with(|s| s.borrow().clone())
            .and_then(|slot| *slot.lock().unwrap_or_else(|e| e.into_inner()));
        if let Some(raw) = hwnd {
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(raw as *mut std::ffi::c_void)),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        } else {
            unsafe { PostQuitMessage(0) };
        }
        return;
    }
    if let Some(na) = new_active {
        prime_capsule_slide(old_active, na);
        ensure_anim_timer();
    }
    draw_frame();
}
