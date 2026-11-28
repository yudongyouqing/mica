//! Input staging/flushing, clipboard paste, keyboard VK/modifier encoding,
//! mouse reporting & coordinate utilities, and keymap action execution.
#![allow(unsafe_op_in_unsafe_fn)]

use super::*;

pub(super) fn paste_input(t: &mut Terminal, bytes: &[u8]) {
    match stage_input(t, bytes) {
        pty_bridge::InputFlow::Accepted | pty_bridge::InputFlow::Debt(_) => {}
        pty_bridge::InputFlow::RejectedOverflow => {
            eprintln!(
                "[paste] 拒绝粘贴 {} 字节(当前可用 {} B):输入通道饱和,零副作用",
                bytes.len(),
                paste_capacity(t),
            );
            warn_box(&format!(
                "粘贴内容({} KB)超出当前输入通道容量({} KB)\n\nshell 正在消化未消费的输入,已接收内容不受影响。\n请稍后重试或分批粘贴。",
                bytes.len() / 1024,
                paste_capacity(t) / 1024,
            ));
        }
        pty_bridge::InputFlow::RejectedClosed => {}
    }
}

/// 粘贴文本统一入口:CRLF 归一 + DECSET 2004 bracketed paste 包裹 + 事务接收。
pub(super) fn paste_text(t: &mut Terminal, text: &str) {
    let normalized = clipboard::normalize_paste(text);
    if t.term.bracketed_paste_active() {
        let mut wrapped = Vec::with_capacity(normalized.len() + 12);
        wrapped.extend_from_slice(b"\x1b[200~");
        wrapped.extend_from_slice(normalized.as_bytes());
        wrapped.extend_from_slice(b"\x1b[201~");
        paste_input(t, &wrapped);
    } else {
        paste_input(t, normalized.as_bytes());
    }
}

/// 当前可整体接收的粘贴容量(与 stage_into 容量规则同源)。
fn paste_capacity(t: &Terminal) -> usize {
    if t.input_stage.is_empty() {
        t.writeq.free_bytes() + t.input_stage.free_room()
    } else {
        t.input_stage.free_room()
    }
}

/// T08(三次返工):统一 UI 输入入口(**非阻塞**)。核心顺序规则在
/// `pty_bridge::stage_into`(与测试共用的生产组件):暂存有旧欠账时
/// 新输入**只追加到暂存尾部**——绝不 try_push 绕过,否则 writer 腾出
/// 的空间会被新输入越过旧欠账(复核探针实测 NEW→OLD 乱序)。
/// 返回 InputFlow(结构化:Accepted/Debt/RejectedOverflow/Closed)。
/// 欠账存在时武装输入续灌定时器(WM_TIMER id 8,见下)。
pub(super) fn stage_input(t: &mut Terminal, bytes: &[u8]) -> pty_bridge::InputFlow {
    let flow = pty_bridge::stage_into(&mut t.input_stage, &t.writeq, bytes);
    match flow {
        pty_bridge::InputFlow::Accepted => {}
        pty_bridge::InputFlow::Debt(_) => arm_input_flush_timer(),
        pty_bridge::InputFlow::RejectedOverflow => {
            eprintln!(
                "[input] 输入暂存满,拒收 {} 字节(累计 {} )",
                bytes.len(),
                pty_bridge::overflow_drops()
            );
        }
        pty_bridge::InputFlow::RejectedClosed => {}
    }
    flow
}

// 输入续灌定时器(id 8,30ms):真实接线的欠账保底——shell 静默时
// WM_APP_RENDER 不来,WM_TIMER 是唯一推进者;空闲时定时器不存在
// (武装/撤销按需,零空闲开销)。TLS 防重复 SetTimer。
thread_local! {
    static INPUT_TIMER_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn arm_input_flush_timer() {
    INPUT_TIMER_ARMED.with(|armed| {
        if armed.get() {
            return;
        }
        if let Some(raw) = SHARED_HWND
            .with(|s| s.borrow().clone())
            .and_then(|slot| *slot.lock().unwrap_or_else(|e| e.into_inner()))
        {
            // SAFETY: 目标是本线程窗口;30ms 续灌节拍
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
                    Some(windows::Win32::Foundation::HWND(
                        raw as *mut std::ffi::c_void,
                    )),
                    8,
                    30,
                    None,
                );
            }
            armed.set(true);
            if std::env::var_os("MICA_DEBUG_INPUTFLOW").is_some() {
                eprintln!("[inputflow] timer armed (hwnd={raw})");
            }
        }
    });
}

/// 全部 pane 的暂存续灌(后台标签/非焦点 pane 都覆盖)。全部清空后
/// 撤定时器(空闲零开销)。
pub(super) fn flush_all_panes() {
    let debug = std::env::var_os("MICA_DEBUG_INPUTFLOW").is_some();
    let mut any_debt = false;
    let mut debt_panes = 0usize;
    TABS.with(|tabs| {
        for tab in tabs.borrow_mut().iter_mut() {
            for pane in tab.panes.iter_mut() {
                let t = &mut pane.terminal;
                if !t.input_stage.is_empty() {
                    t.input_stage.flush_into(&t.writeq);
                    if !t.input_stage.is_empty() {
                        any_debt = true;
                        debt_panes += 1;
                    }
                }
            }
        }
    });
    if debug {
        let (flushed, q, s, acc, infl, wr, fail, bal): (u64, u64, u64, u64, u64, u64, u64, bool) =
            TABS.with(|tabs| {
                let g = tabs.borrow();
                let (mut f, mut q, mut s2, mut a, mut i, mut w, mut fl) =
                    (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
                let mut bal = true;
                for tab in g.iter() {
                    for pane in tab.panes.iter() {
                        let t = &pane.terminal;
                        let acct = t.writeq.accounting();
                        bal &= acct.balanced();
                        f += t.input_stage.flushed_total();
                        q += acct.queued;
                        s2 += t.input_stage.len() as u64;
                        a += acct.accepted;
                        i += acct.inflight;
                        w += acct.written;
                        fl += acct.failed_unknown;
                    }
                }
                (f, q, s2, a, i, w, fl, bal)
            });
        eprintln!(
            "[inputflow] timer-tick: debt_panes={debt_panes} armed={any_debt} flushed={flushed} q={q} s={s} acc={acc} infl={infl} wr={wr} fail={fail} bal={bal}"
        );
    }
    if any_debt {
        arm_input_flush_timer();
    }
    if !any_debt && INPUT_TIMER_ARMED.with(std::cell::Cell::get) {
        if let Some(raw) = SHARED_HWND
            .with(|s| s.borrow().clone())
            .and_then(|slot| *slot.lock().unwrap_or_else(|e| e.into_inner()))
        {
            // SAFETY: 本线程窗口的定时器撤销
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::KillTimer(
                    Some(windows::Win32::Foundation::HWND(
                        raw as *mut std::ffi::c_void,
                    )),
                    8,
                )
                .ok();
            }
        }
        INPUT_TIMER_ARMED.with(|a| a.set(false));
    }
}

/// T08:续灌(消息循环心跳调用)。
pub(super) fn flush_input(t: &mut Terminal) {
    t.input_stage.flush_into(&t.writeq);
}

/// VK → 终端键(kitty 编码用;与 vk_to_trigger 同表源,不经 Keymap)。
/// match 模式不能用 VK 常量表达式(pattern 限制),数值注释留来源。
pub(super) fn vk_to_key(vk: u32) -> Option<mica_core::input::Key> {
    Some(match vk {
        0x0D => mica_core::input::Key::Enter,
        0x08 => mica_core::input::Key::Backspace,
        0x09 => mica_core::input::Key::Tab,
        0x1B => mica_core::input::Key::Escape,
        0x26 => mica_core::input::Key::Up,       // VK_UP
        0x28 => mica_core::input::Key::Down,     // VK_DOWN
        0x25 => mica_core::input::Key::Left,     // VK_LEFT
        0x27 => mica_core::input::Key::Right,    // VK_RIGHT
        0x24 => mica_core::input::Key::Home,     // VK_HOME
        0x23 => mica_core::input::Key::End,      // VK_END
        0x2E => mica_core::input::Key::Delete,   // VK_DELETE
        0x21 => mica_core::input::Key::PageUp,   // VK_PRIOR
        0x22 => mica_core::input::Key::PageDown, // VK_NEXT
        _ => return None,
    })
}

/// VK → Keymap 触发键(字母/数字/Insert/导航;其余键无终端外语义)。
pub(super) fn vk_to_trigger(vk: u32) -> Option<TriggerKey> {
    match vk {
        _ if vk == VK_TAB.0 as u32 => Some(TriggerKey::Key(Key::Tab)),
        // VK 常量是关联 const,进不了 pattern——守卫链对齐 vkey_bytes
        0x41..=0x5A => Some(TriggerKey::Letter(vk as u8 as char)),
        0x30..=0x39 => Some(TriggerKey::Digit((vk - 0x30) as u8)),
        _ if vk == VK_INSERT.0 as u32 => Some(TriggerKey::Insert),
        _ if vk == VK_UP.0 as u32 => Some(TriggerKey::Key(Key::Up)),
        _ if vk == VK_DOWN.0 as u32 => Some(TriggerKey::Key(Key::Down)),
        _ if vk == VK_LEFT.0 as u32 => Some(TriggerKey::Key(Key::Left)),
        _ if vk == VK_RIGHT.0 as u32 => Some(TriggerKey::Key(Key::Right)),
        _ if vk == VK_HOME.0 as u32 => Some(TriggerKey::Key(Key::Home)),
        _ if vk == VK_END.0 as u32 => Some(TriggerKey::Key(Key::End)),
        _ if vk == VK_DELETE.0 as u32 => Some(TriggerKey::Key(Key::Delete)),
        _ if vk == VK_PRIOR.0 as u32 => Some(TriggerKey::Key(Key::PageUp)),
        _ if vk == VK_NEXT.0 as u32 => Some(TriggerKey::Key(Key::PageDown)),
        _ => None,
    }
}

pub(super) fn vkey_bytes(vk: u32, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    // #79 key-remap:encode 前转换修饰(动作判定已在上游完成)
    let mods = remap_mods(mods);
    let key = match VIRTUAL_KEY(vk as u16) {
        VK_TAB => Key::Tab,
        VK_UP => Key::Up,
        VK_DOWN => Key::Down,
        VK_LEFT => Key::Left,
        VK_RIGHT => Key::Right,
        VK_HOME => Key::Home,
        VK_END => Key::End,
        VK_DELETE => Key::Delete,
        VK_PRIOR => Key::PageUp,
        VK_NEXT => Key::PageDown,
        _ => return None,
    };
    Some(mica_core::input::encode(key, mods, app_cursor))
}

/// #79 应用 key-remap 表(链式:逐条翻转修饰位)
pub(super) fn remap_mods(mods: Mods) -> Mods {
    KEY_REMAP.with(|r| r.borrow().iter().fold(mods, |m, rm| rm.apply(m)))
}

// All other paste shortcuts belong to the configurable keymap.
pub(super) fn contextual_paste(mods: Mods, vk: u32) -> bool {
    mods.ctrl && !mods.alt && !mods.shift && !mods.win && vk == 'V' as u32
}

pub(super) fn current_mods() -> Mods {
    unsafe {
        let down = |vk: VIRTUAL_KEY| GetKeyState(vk.0 as i32) < 0;
        Mods {
            shift: down(VK_SHIFT),
            alt: down(VK_MENU),
            ctrl: down(VK_CONTROL),
            win: false,
        }
    }
}

/// 鼠标上报路由(DECSET 1000/1002/1003 + 1006,2026-10-07):应用捕获
/// 鼠标时把点击/拖动/滚轮编码写回 pty,替代本地选择/滚回。Shift 按住
/// 强制本地(业界惯例——文本选择不被应用截走)。motion 事件仅在
/// Drag/Motion 模式发。返回 true = 事件已消费,调用方勿走本地交互。
pub(super) fn forward_mouse(px: i32, py: i32, button: u8, pressed: bool, motion: bool) -> bool {
    if current_mods().shift {
        return false;
    }
    let rects = active_pane_rects();
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        let Some(tab) = guard.get_mut(ACTIVE.get()) else {
            return false;
        };
        let Some((pid, r)) = rects.iter().find(|(_, r)| {
            (px as f32) >= r.x
                && (px as f32) < r.x + r.w
                && (py as f32) >= r.y
                && (py as f32) < r.y + r.h
        }) else {
            return false;
        };
        let Some(t) = tab.pane_by_id(*pid).map(|p| &mut p.terminal) else {
            return false;
        };
        let mode = t.term.mouse_mode();
        use mica_core::surface::MouseMode;
        if mode == MouseMode::Off {
            return false;
        }
        if motion && mode == MouseMode::Click {
            return false;
        }
        let col = (((px as f32 - r.x) / t.metrics.cell_width).floor() as u16)
            .saturating_add(1)
            .min(t.cols);
        let row = (((py as f32 - r.y) / t.metrics.line_height).floor() as u16)
            .saturating_add(1)
            .min(t.rows);
        let bytes = mica_core::input::mouse_report(
            t.term.mouse_sgr(),
            pressed,
            button,
            col,
            row,
            current_mods(),
        );
        let _ = stage_input(t, &bytes);
        true
    })
}

/// 客户区像素 → buffer 坐标点(视口行 + display_offset)与半格侧别。
/// 拖出上/左边缘夹到 0(捕获期间 WM_MOUSEMOVE 仍投递,坐标可为负)。
pub(super) fn px_to_buffer_point(
    px: f32,
    py: f32,
    metrics: &FontMetrics,
    display_offset: usize,
) -> (Point, Side) {
    let col_f = (px.max(0.0) / metrics.cell_width).max(0.0);
    let col = col_f as usize;
    let side = if (col_f - col as f32) < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    let view_line = (py.max(0.0) / metrics.line_height).floor() as i32;
    (
        Point::new(Line(view_line - display_offset as i32), Column(col)),
        side,
    )
}

/// 打字藏鼠标(Ghostty mouse-hide-while-typing):ShowCursor 计数式,
/// CURSOR_HIDDEN 独占配对——藏一次显一次;WM_MOUSEMOVE 处恢复
pub(super) fn hide_cursor_if_typing() {
    if MOUSE_HIDE_ON_TYPE.with(std::cell::Cell::get) && !CURSOR_HIDDEN.with(std::cell::Cell::get) {
        CURSOR_HIDDEN.with(|c| c.set(true));
        // SAFETY: 指针显示计数 -1;恢复在 WM_MOUSEMOVE(配对)
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowCursor(false);
        }
    }
}

/// 鼠标消息 lparam 的有符号客户区坐标。
pub(super) fn mouse_xy(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xffff) as u16 as i16 as i32,
        ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32,
    )
}

/// Win32:TrackMouseEvent 投递的 hover/leave(0x2A2/0x2A3;windows-rs 0.62
/// 只收在未启用的 Win32_UI_Controls feature,本地声明免开整包)。
pub(super) const WM_MOUSEHOVER: u32 = 673;
pub(super) const WM_MOUSELEAVE: u32 = 675;
/// TrackMouseEvent TME_NONCLIENT 投递(0x02A2;windows-rs 未收录)
pub(super) const WM_NCMOUSELEAVE: u32 = 674;

/// 武装 hover/leave 追踪(WM_MOUSEMOVE 非拖选路径与 WM_MOUSEHOVER 重武装共用)。
/// dwell 300ms:观感上"停一下才出提示",误触率与响应速度的折中。
pub(super) fn arm_hover(hwnd: HWND) {
    let mut tme = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_HOVER | TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 300,
    };
    // SAFETY: OS 追踪注册;失败(罕见)只是收不到 hover,无碍
    unsafe {
        let _ = TrackMouseEvent(&mut tme);
    }
    HOVER_ARMED.with(|a| a.set(true));
}

/// 执行终端外语义动作。返回 true = 已消费(不再走 pty 编码)。
pub(super) fn execute_action(action: Action, hwnd: HWND) -> bool {
    let mut need_draw = false;
    let mut restart = false; // 延迟执行(块内自借 TABS 会 panic——冒烟实证)
    let mut search_toggle = false; // #57:段外 toggle(不借 TABS)
    let mut write_scrollback = false; // #81:段外写文件+打开(不走 TABS 双借)
    let mut toggle_opacity = false; // #78:段外切档(需要 hwnd)
    let mut reopen_tab = false; // #88:段外重开(需要 hwnd)
    let mut rename_tab = false; // #87:段外开改名模式
    let mut new_tab = false;
    let mut new_window = false;
    let mut close_idx: Option<usize> = None;
    let mut next_idx: Option<usize> = None;
    let mut goto_idx: Option<usize> = None;
    let mut split: Option<mica_core::layout::SplitDir> = None;
    let mut close_pane = false;
    let mut focus_dir: Option<Key> = None;
    TABS.with(|cell| {
        let mut tabs = cell.borrow_mut();
        let tab_count = tabs.len();
        if let Some(t) = tabs.get_mut(ACTIVE.get()).and_then(|tab| tab.active_pane()) {
            match action {
                Action::Copy => {
                    if let Some(text) = t.term.selection_text() {
                        clipboard::set_text(&text);
                        t.term.selection_clear();
                        t.force_full = true;
                        need_draw = true;
                    }
                }
                Action::Paste => {
                    if let Some(text) = clipboard::get_text() {
                        paste_text(t, &text);
                    }
                }
                Action::ScrollLine(n) => {
                    t.term.scroll_display(ScrollCommand::Delta(n));
                    t.force_full = true;
                    need_draw = true;
                }
                Action::RestartPane => restart = true, // 延迟:with 块内再借 TABS 会双借 panic
                Action::SearchScrollback => search_toggle = !search_toggle,
                Action::ToggleBackgroundOpacity => toggle_opacity = true,
                Action::ReopenTab => reopen_tab = true,
                Action::RenameTab => rename_tab = true,
                Action::WriteScrollback => write_scrollback = true,
                Action::NewWindow => new_window = true,
                Action::JumpPrevPrompt | Action::JumpNextPrompt => {
                    if t.term.jump_prompt(matches!(action, Action::JumpNextPrompt)) {
                        t.force_full = true;
                        need_draw = true;
                    }
                }
                Action::ScrollPage(n) => {
                    let rows = t.rows as i32;
                    t.term.scroll_display(ScrollCommand::Delta(n * rows));
                    t.force_full = true;
                    need_draw = true;
                }
                Action::ScrollTop => {
                    t.term.scroll_display(ScrollCommand::Top);
                    t.force_full = true;
                    need_draw = true;
                }
                Action::ScrollBottom => {
                    t.term.scroll_display(ScrollCommand::Bottom);
                    t.force_full = true;
                    need_draw = true;
                }
                // 标签操作这里只标记,段外执行(避免嵌套借 TABS)
                Action::NewTab => new_tab = true,
                Action::CloseTab => close_idx = Some(ACTIVE.get()),
                Action::NextTab => {
                    let n = tab_count;
                    next_idx = Some((ACTIVE.get() + 1) % n.max(1));
                }
                Action::PrevTab => {
                    let n = tab_count;
                    next_idx = Some((ACTIVE.get() + n.saturating_sub(1)) % n.max(1));
                }
                Action::GotoTab(n) => goto_idx = Some((n as usize).saturating_sub(1)),
                // 分屏动作需要 tab 级(layout/panes)与新建 pty(段外做,
                // 避免 TABS 借用嵌套);方向信息带出去
                Action::SplitRight => split = Some(mica_core::layout::SplitDir::Horizontal),
                Action::SplitDown => split = Some(mica_core::layout::SplitDir::Vertical),
                Action::ClosePane => close_pane = true,
                Action::FocusNeighbor(dir) => focus_dir = Some(dir),
            }
        }
    });
    if need_draw {
        draw_frame();
    }
    // 标签操作段外执行:TABS 借用已还,switch/close/start 可自由再借
    if restart {
        restart_active_pane();
    }
    if reopen_tab {
        // #88 LIFO 重开:shell/cwd/标题恢复
        let snapshot = CLOSED_TABS.with(|h| h.borrow_mut().pop());
        let Some(ct) = snapshot else {
            return true;
        };
        // SAFETY: 段外(TABS 借用已还),create/forwarder 与既有路径同款
        unsafe {
            let (pane, reader) = match create_pane_with_options(
                hwnd,
                &ct.shell,
                None,
                ct.cwd.as_deref(),
                Some(&ct.extra_env),
            ) {
                Ok(pair) => pair,
                Err(e) => {
                    warn_box(&e);
                    return true;
                }
            };
            let pane_id = pane.id;
            let tab = TabState {
                title: ct.title,
                color: ct.color,
                dirty: false,
                panes: vec![pane],
                layout: mica_core::layout::Layout::leaf(pane_id),
                focused: 0,
            };
            let pty_buf = Arc::clone(&tab.panes[0].terminal.pty_buf);
            let fwd_epoch = tab.panes[0].terminal.session_gen;
            let tab_index = TABS.with(|tabs| tabs.borrow().len());
            window_ipc::pane_created(hwnd, tab_index, &tab.panes[0]);
            TABS.with(|tabs| tabs.borrow_mut().push(tab));
            ACTIVE.with(|a| a.set(TABS.with(|tabs| tabs.borrow().len() - 1)));
            TAB_COUNT.with(|c| c.set(TABS.with(|tabs| tabs.borrow().len())));
            tab_follow_active();
            if let Some(slot) = SHARED_HWND.with(|s| s.borrow().clone()) {
                let _ = &slot;
                spawn_render_forwarder(reader, pty_buf, pane_id, fwd_epoch);
            }
            draw_frame();
        }
    }
    if toggle_opacity {
        // #78 两档切换:apply_window_alpha 同步更新 WINDOW_TRANSLUCENT
        let to_translucent = !WINDOW_TRANSLUCENT.with(std::cell::Cell::get);
        apply_window_alpha(hwnd, to_translucent);
    }
    if rename_tab {
        let idx = ACTIVE.get();
        let cur_title = TABS.with(|tabs| {
            tabs.borrow()
                .get(idx)
                .map(|tab| tab.title.clone())
                .unwrap_or_default()
        });
        SEARCH_OPEN.with(|o| o.set(false));
        RENAME_IDX.with(|i| i.set(idx));
        RENAME_QUERY.with(|q| *q.borrow_mut() = cur_title);
        RENAME_OPEN.with(|o| o.set(true));
        draw_frame();
    }
    if write_scrollback {
        // #81:回看导出——%TEMP%/mica-scrollback-{标题}-{时间戳}.log,
        // UTF-8 写入 + 系统默认打开;失败只记日志
        let (text, title) = TABS.with(|tabs| {
            let guard = tabs.borrow();
            guard
                .get(ACTIVE.get())
                .and_then(|tab| tab.active_pane_ref())
                .map(|t| {
                    (
                        t.term.scrollback_text(),
                        guard
                            .get(ACTIVE.get())
                            .map(tab_title_safe)
                            .unwrap_or_default(),
                    )
                })
                .unwrap_or_default()
        });
        if !text.is_empty() {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let safe: String = title
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || matches!(c, '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .take(24)
                .collect();
            let dir = std::env::temp_dir();
            let path = dir.join(format!("mica-scrollback-{safe}-{ts}.log"));
            match std::fs::write(&path, &text) {
                Ok(()) => {
                    let _ = path.to_str().map(|p| {
                        unsafe {
                            ShellExecuteW(
                                None,
                                windows::core::w!("open"),
                                windows::core::PCWSTR(windows::core::HSTRING::from(p).as_ptr()),
                                None,
                                None,
                                SW_SHOWNORMAL,
                            )
                        };
                    });
                }
                Err(e) => eprintln!("[scrollback] 写出失败:{e}"),
            }
        }
    }
    if search_toggle {
        if SEARCH_OPEN.with(std::cell::Cell::take) {
            search_close();
        } else {
            search_open();
        }
    }
    if new_window {
        spawn_window(None);
    }
    if new_tab {
        unsafe { start_tab(hwnd) };
        draw_frame();
    }
    if let Some(dir) = split {
        unsafe { split_pane(hwnd, dir) };
        draw_frame();
    }
    if close_pane {
        close_pane_action();
    }
    if let Some(dir) = focus_dir {
        focus_neighbor(dir);
    }
    if let Some(idx) = close_idx {
        close_tab(idx);
    }
    if let Some(idx) = next_idx.or(goto_idx) {
        switch_tab(idx);
    }
    true
}
