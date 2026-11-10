//! Quick Terminal(M3b,spec §6):全局热键呼出的顶部下拉终端。
//!
//! 独立窗口类(无标题栏、不占任务栏)、独立单 pane 状态(自己的
//! Surface/session/router/行缓存),**共享进程级 GPU TLS**(Renderer
//! 按窗口各有 bind group?不——wgpu Surface 是窗口级,QT 自建 surface,
//! Renderer 也各一份:GPU TLS 只借 GpuContext/adapter;为最简起见 QT
//! 自持 Renderer)。失焦自动收起;WM_HOTKEY 切换。

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use mica_core::surface::{Damage, ScreenSize, Surface};
use mica_render::font::dwrite::DwriteRouter;
use mica_render::frame::{build_rows, repack};
use mica_render::pipeline::{Renderer, create_context};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_NOREPEAT, MOD_WIN, RegisterHotKey, UnregisterHotKey, VK_OEM_3,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, IDC_ARROW, LoadCursorW,
    RegisterClassExW, SW_HIDE, ShowWindow, WINDOW_EX_STYLE, WM_ACTIVATE, WM_APP, WM_CHAR,
    WM_DESTROY, WM_ERASEBKGND, WM_HOTKEY, WM_KEYDOWN, WM_PAINT, WM_SIZE, WM_TIMER, WNDCLASSEXW,
    WS_POPUP, WS_VISIBLE,
};
use windows::core::{HSTRING, w};

use crate::app::Terminal;

/// QT 独立状态(单 pane)。
struct QtState {
    terminal: Terminal,
    renderer: Renderer,
    wgpu_surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    /// 输入辅助:QT 窗口自己的代理对暂存(与主窗共用 TLS 会串号,故本地)
    pending_surrogate: u32,
}

thread_local! {
    static QT: RefCell<Option<QtState>> = const { RefCell::new(None) };
    /// QT 的 hwnd(主线程持;热键处理用)
    static QT_HWND: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 创建 Quick Terminal(主窗 run() 尾调用;失败仅记日志——QT 是增强件)。
/// 返回热键 id(0 = 未注册)。
pub unsafe fn init(hwnd_main: HWND) -> Result<(), ()> {
    let class_name: HSTRING = "mica_quickterm_class".into();
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(qt_wndproc),
        hInstance: windows::Win32::Foundation::HINSTANCE(
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .expect("GetModuleHandleW")
                .0,
        ),
        hCursor: LoadCursorW(None, IDC_ARROW).expect("cursor"),
        lpszClassName: w!("mica_quickterm_class"),
        ..Default::default()
    };
    // 重复注册(热重载期)不致命
    let _ = RegisterClassExW(&wc);

    // 尺寸:主显示器宽 100% × 高 40%
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let mon = MonitorFromWindow(hwnd_main, MONITOR_DEFAULTTONEAREST);
    let _ = GetMonitorInfoW(mon, &mut mi);
    let w_px = mi.rcMonitor.right - mi.rcMonitor.left;
    let h_px = (mi.rcMonitor.bottom - mi.rcMonitor.top) * 40 / 100;

    let hwnd: HSTRING = "mica_quickterm_class".into();
    let hwnd_qt = CreateWindowExW(
        WINDOW_EX_STYLE::default(), // POPUP 已不占任务栏;TOOLWINDOW 再防 Alt+Tab
        &hwnd,
        w!("Mica Quick Terminal"),
        WS_POPUP, // 初始隐藏,热键时显示
        mi.rcMonitor.left,
        0,
        w_px,
        h_px,
        None,
        None,
        None,
        None,
    )
    .expect("CreateWindowExW (quick terminal)");
    // 圆角(Win11 DWM,与主窗同款;下拉面板圆角是 Ghostty QT 观感)
    {
        let corner: i32 = windows::Win32::Graphics::Dwm::DWMWCP_ROUND.0;
        // SAFETY: 属性设置,失败静默(老系统无此属性)
        unsafe {
            let _ = windows::Win32::Graphics::Dwm::DwmSetWindowAttribute(
                hwnd_qt,
                windows::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const i32 as *const std::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }
    }
    QT_HWND.with(|c| c.set(hwnd_qt.0 as usize));

    // 热键:Win+`(可配;失败记日志,QT 仍可用 IPC 激活留 M3c)
    // id=1 与主窗不冲突(RegisterHotKey 按线程+id 域)
    let hotkey_ok = RegisterHotKey(Some(hwnd_qt), 1, MOD_WIN | MOD_NOREPEAT, VK_OEM_3.0 as u32);
    if hotkey_ok.is_err() {
        eprintln!("quickterm: Win+` 注册失败(被占用?)——QT 需重配 quick-terminal-key");
    }

    // QT 终端状态:复用 TAB_SEED/CURRENT_PALETTE 建字体与 pty
    let (families, size_pt) = crate::app::tab_seed();
    let size_pt = crate::app::scaled_pt(size_pt); // 字号随 DPI(与主窗单源)
    let fam_ref: Vec<&str> = families.iter().map(String::as_str).collect();
    let Ok(router) = DwriteRouter::new(size_pt, &fam_ref) else {
        eprintln!("quickterm: 字体链失败,QT 禁用");
        return Err(());
    };
    let metrics = router.metrics();
    let palette = crate::app::current_palette();

    // wgpu surface + renderer(独立于主窗;display handle 与主窗同款——
    // wgpu 30 在 DX12 后端要求显式 display handle,None 会 MissingDisplayHandle)
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let wgpu_surface: wgpu::Surface<'static> = instance
        .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(raw_window_handle::RawDisplayHandle::Windows(
                raw_window_handle::WindowsDisplayHandle::new(),
            )),
            raw_window_handle: raw_window_handle::RawWindowHandle::Win32(
                raw_window_handle::Win32WindowHandle::new(
                    std::num::NonZeroIsize::new(hwnd_qt.0 as isize).expect("hwnd"),
                ),
            ),
        })
        .inspect_err(|e| eprintln!("quickterm: surface 失败 {e:?},QT 禁用"))
        .map_err(|_| ())?;
    let ctx = pollster::block_on(create_context(&instance, Some(&wgpu_surface)))
        .inspect_err(|e| eprintln!("quickterm: GPU 上下文失败 {e:?},QT 禁用"))
        .map_err(|_| ())?;
    let mut config = wgpu_surface
        .get_default_config(&ctx.adapter, w_px as u32, h_px as u32)
        .ok_or(())?; // Option:get_default_config 失败仅记日志走降级
    if config.format.is_srgb() {
        let caps = wgpu_surface.get_capabilities(&ctx.adapter);
        config.format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .expect("non-srgb");
    }
    wgpu_surface.configure(&ctx.device, &config);
    let mut renderer = Renderer::new(&ctx, config.format)
        .inspect_err(|e| eprintln!("quickterm: atlas quota failed {e}, QT disabled"))
        .map_err(|_| ())?;
    renderer.set_clear_color(&palette);

    let cols = ((w_px as f32 / metrics.cell_width).max(1.0)) as u16;
    let rows = ((h_px as f32 / metrics.line_height).max(1.0)) as u16;
    let mut term = Surface::new(ScreenSize::new(cols as usize, rows as usize));
    term.set_cell_metrics(
        metrics.cell_width.round() as u16,
        metrics.line_height.round() as u16,
    );
    term.set_palette(&palette);
    // QT shell 恒 PowerShell(下拉终端语义;profile 化 M4)
    if !crate::app::lifecycle::spawn_capacity_ok() {
        // 故障态门:拒绝新会话(有界泄漏);QT 是增强件,禁用即可
        eprintln!("quickterm: 会话回收通道故障态,QT 本轮禁用");
        return Err(());
    }
    let session_permit = crate::app::lifecycle::acquire_session().ok_or_else(|| {
        eprintln!("quickterm: 并发会话已达上限,QT 本轮禁用");
    })?;
    let probe = crate::app::orphan::before_spawn();
    let (session, reader) = mica_core::pty::PtySession::spawn(
        mica_core::pty::command_from_str("powershell.exe -NoLogo"),
        cols,
        rows,
    )
    .inspect_err(|e| eprintln!("quickterm: pty 失败 {e:?},QT 禁用"))
    .map_err(|_| ())?;
    if let Some(pid) = session.child_pid() {
        crate::app::orphan::track_session_since(&probe, pid); // 入注册表(T02,同主窗)
    }
    // T08:QT 输出积压/写队列(WPARAM 0xFF00 寻址 QT 窗口)
    let pty_buf = crate::app::pty_bridge::PtyBacklog::new(
        0xFF00,
        crate::app::pty_bridge::PANE_BUDGET_BYTES,
        Arc::new(crate::app::pty_bridge::WindowWake {
            slot: crate::app::shared_hwnd_for_quickterm()
                .unwrap_or_else(|| Arc::new(Mutex::new(None))),
            msg: WM_APP,
        }),
    );
    let writeq = crate::app::pty_bridge::WriteQueue::new();
    let writer = session.writer_handle(); // 锁分离(复核 3):只持写端
    {
        let wq = Arc::clone(&writeq);
        std::thread::Builder::new()
            .name("qt-pty-writer".into())
            .spawn(move || {
                crate::app::pty_bridge::writer_loop(
                    wq,
                    Arc::new(crate::app::pty_bridge::SessionSink(writer)),
                );
            })
            .expect("spawn qt writer");
    }
    pty_buf.spawn_monitor(); // 独立唤醒重试(不依赖转发线程)
    term.set_clipboard_provider(Arc::new(|| {
        crate::app::clipboard::get_text().unwrap_or_default()
    }));

    let terminal = Terminal {
        term,
        session,
        session_permit: Some(session_permit),
        pty_buf: Arc::clone(&pty_buf),
        writeq: Arc::clone(&writeq),
        input_stage: crate::app::pty_bridge::InputStage::new(),
        session_gen: 0,
        router,
        metrics,
        palette,
        cols,
        rows,
        row_insts: Vec::new(),
        force_full: true,
        shell: "powershell.exe -NoLogo".into(),
        exited: None,
        strip_router: None,
        last_atlas_ver: 0,
    };
    QT.with(|q| {
        *q.borrow_mut() = Some(QtState {
            terminal,
            renderer,
            wgpu_surface,
            config,
            pending_surrogate: 0,
        })
    });

    // 转发线程(与主窗同款;WPARAM 用 QT 专属 id 0xFF00——不与 pane id 冲突)
    crate::app::spawn_qt_forwarder(reader, pty_buf, 0xFF00, 0);
    // #37:会话退出轮询(1s;命中即停表)——QT 无 strip,主窗的
    // poll_exits 轮不到它
    let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd_qt), 6, 1000, None);
    let _ = class_name; // 注册已用 w! 字面量
    Ok(())
}

/// 重开 QT 会话(#37):同 shell 新 pty + 干净 Surface(复用 pane 槽),
/// 挂新转发线程 + 重启退出轮询。会话族(permit/写队列/回收)全走
/// `lifecycle::restart_session_fields` 共享流——失败留 Stopped 态可重试
unsafe fn qt_respawn(t: &mut crate::app::Terminal, hwnd: HWND) {
    let (cols, rows) = (t.cols, t.rows);
    // **r5-2 统一重启序**(同主窗):kill → Stopped 化 → 回收旧 PTY →
    // 放旧 permit → 取新 permit → spawn。修前 QT 反序(先 acquire 后
    // drop):峰值多占一槽,且 spawn 失败时新 permit 泄漏
    let restarted = crate::app::lifecycle::restart_session_fields(
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
    match restarted {
        Ok((reader, new_gen)) => {
            crate::app::spawn_qt_forwarder(reader, Arc::clone(&t.pty_buf), 0xFF00, new_gen);
            let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 6, 1000, None);
        }
        Err(e) => eprintln!("[qt] {e}——本轮不重开"),
    }
}

/// 退出序取走 QT 状态(T03 收窄绕行面):终端态走 close_terminal 统一
/// 回收(杀子/标 Closing/交 reaper);**只 forget renderer/wgpu_surface
/// (GPU 对象)**——坏驱动栈上 wgpu teardown 陷内核(1.0.2 实证),这是
/// 与主窗同款的最后绕行。
pub fn forget_state() {
    if let Some(st) = QT.with(|q| q.borrow_mut().take()) {
        let QtState {
            terminal,
            renderer,
            wgpu_surface,
            config,
            pending_surrogate: _,
        } = st;
        let mut renderer = renderer;
        renderer.release_atlas_resources();
        std::mem::forget(renderer);
        std::mem::forget(wgpu_surface);
        std::mem::forget(config);
        crate::app::lifecycle::close_terminal(terminal);
    }
}

/// 切换:可见→隐藏;隐藏→显示+置顶激活。
/// #44 send-text 的 QT 入口:向 QT pane 注入输入(bracketed paste 自动包)
/// QT 是否已建(供 IPC 区分 bad_target 与 pty_write)。
pub fn exists() -> bool {
    QT.with(|q| q.borrow().is_some())
}

/// 向 QT pane 注入输入(UI 快路径,非阻塞;当前 UI 侧 QT 输入经
/// wndproc 直呼 accept_qt_input,此入口留作热键/脚本路径的公共面)。
#[allow(dead_code)]
pub fn send_text(bytes: &[u8]) -> bool {
    QT.with(|q| {
        let mut guard = q.borrow_mut();
        let Some(st) = guard.as_mut() else {
            return false;
        };
        let t = &mut st.terminal;
        let payload = if t.term.bracketed_paste_active() {
            let mut w = b"[200~".to_vec();
            w.extend_from_slice(bytes);
            w.extend_from_slice(b"[201~");
            w
        } else {
            bytes.to_vec()
        };
        let flow = accept_qt_input(t, &payload);
        if matches!(flow, crate::app::pty_bridge::InputFlow::Debt(_)) {
            arm_qt_flush_timer();
        }
        matches!(
            flow,
            crate::app::pty_bridge::InputFlow::Accepted
                | crate::app::pty_bridge::InputFlow::Debt(_)
        )
    })
}

/// 武装 QT 续灌定时器(id 7,30ms;清欠自动撤销)。
#[allow(dead_code)]
fn arm_qt_flush_timer() {
    let raw = QT_HWND.with(|c| c.get());
    if raw == 0 {
        return;
    }
    // SAFETY: 本线程拥有的 QT 窗口
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
            Some(windows::Win32::Foundation::HWND(
                raw as *mut std::ffi::c_void,
            )),
            7,
            30,
            None,
        );
    }
}

fn accept_qt_input(
    t: &mut crate::app::Terminal,
    payload: &[u8],
) -> crate::app::pty_bridge::InputFlow {
    let flow = crate::app::pty_bridge::stage_into(&mut t.input_stage, &t.writeq, payload);
    if matches!(flow, crate::app::pty_bridge::InputFlow::Debt(_)) {
        arm_qt_flush_timer();
    }
    flow
}

/// QT 输入的 IPC 生产入口(send-text `--qt`):bracketed paste 包装 +
/// **非阻塞接收**(stage_into,零等待)+ 武装 QT 自己的续灌定时器
/// (id 7)——所属输入目标 → 所属续灌调度(不借主窗 stage_input,那会
/// 武装主窗 id 8,而 flush_all_panes 只遍历 TABS,QT 欠账无人推进)。
/// 按键与应答回写走 `accept_qt_input`(手持引用):WM_APP/WM_CHAR 处
/// 理中已持 QT.borrow_mut(),这里再进全局面即双重借用 panic(2026-11-10
/// 修复,回归见本模块 tests)。
pub fn accept_qt(bytes: &[u8]) -> crate::app::pty_bridge::InputFlow {
    QT.with(|q| {
        let mut guard = q.borrow_mut();
        let Some(st) = guard.as_mut() else {
            return crate::app::pty_bridge::InputFlow::RejectedClosed;
        };
        let t = &mut st.terminal;
        let payload = if t.term.bracketed_paste_active() {
            let mut w = b"\x1b[200~".to_vec();
            w.extend_from_slice(bytes);
            w.extend_from_slice(b"\x1b[201~");
            w
        } else {
            bytes.to_vec()
        };
        let flow = crate::app::pty_bridge::stage_into(&mut t.input_stage, &t.writeq, &payload);
        if matches!(flow, crate::app::pty_bridge::InputFlow::Debt(_)) {
            arm_qt_flush_timer();
        }
        flow
    })
}

/// QT 续灌心搏(WM_TIMER id 7):冲欠账;清空即撤定时器(空闲零开销)。
unsafe fn qt_flush_tick(hwnd: HWND) {
    flush_input();
    if !has_input_debt() {
        windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 7).ok();
    }
}

/// QT 欠账续灌(被 QT 自己的 wndproc 心搏调用)。
pub fn flush_input() {
    QT.with(|q| {
        if let Some(st) = q.borrow_mut().as_mut()
            && !st.terminal.input_stage.is_empty()
        {
            st.terminal.input_stage.flush_into(&st.terminal.writeq);
        }
    });
}

/// QT 是否有输入欠账(定时器保活判定)。
pub fn has_input_debt() -> bool {
    QT.with(|q| {
        q.borrow()
            .as_ref()
            .is_some_and(|st| !st.terminal.input_stage.is_empty())
    })
}

/// 测试面:QT 输出积压的注入把手。ConPTY 会剥离查询序列,端到端喂
/// 不进来;注入点在转发线程下游(backlog.push 之后),自 WM_APP 消费
/// 起仍是全真生产路径。
#[cfg(test)]
pub(crate) fn backlog_for_test() -> Option<Arc<crate::app::pty_bridge::PtyBacklog>> {
    QT.with(|q| {
        q.borrow()
            .as_ref()
            .map(|st| Arc::clone(&st.terminal.pty_buf))
    })
}

pub unsafe fn toggle() {
    let raw = QT_HWND.with(|c| c.get());
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut std::ffi::c_void);
    let visible = (windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
    ) as u32)
        & WS_VISIBLE.0
        != 0;
    if visible {
        let _ = ShowWindow(hwnd, SW_HIDE);
    } else {
        // #37:上次会话已退出 → 重开(同 shell 新会话)再显示
        QT.with(|q| {
            let mut guard = q.borrow_mut();
            let Some(st) = guard.as_mut() else { return };
            if st.terminal.exited.is_some() {
                st.terminal.session.kill_child();
                qt_respawn(&mut st.terminal, hwnd);
            }
        });
        let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
            hwnd,
            Some(windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST),
            0,
            0,
            0,
            0,
            windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE
                | windows::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW,
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
    }
}

unsafe extern "system" fn qt_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            unsafe { toggle() };
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // WA_INACTIVE(0)=失焦 → 收起(spec §6)
            if wparam.0 == 0 {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TIMER => {
            // id 6(#37):会话退出轮询——主窗 poll_exits 轮 TABS,QT 在此
            // 自轮;命中记码 + 横幅重绘 + 停表(重开时 toggle 再起)
            if wparam.0 == 7 {
                // T08 三次返工:QT 输入欠账续灌(真实接线;主窗 id 8
                // 同语义,覆盖后台标签——QT 是独立 wndproc 独立定时器)
                qt_flush_tick(hwnd);
                return LRESULT(0);
            }
            if wparam.0 == 6 {
                let mut hit = false;
                QT.with(|q| {
                    let mut guard = q.borrow_mut();
                    let Some(st) = guard.as_mut() else { return };
                    let exited_now = {
                        let t = &mut st.terminal;
                        if t.exited.is_none()
                            && let Ok(Some(s)) = t.session.try_wait()
                        {
                            t.exited = Some(s.exit_code());
                            true
                        } else {
                            false
                        }
                    };
                    if exited_now {
                        hit = true;
                        qt_draw(st);
                    }
                });
                if hit {
                    windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 6).ok();
                }
            }
            LRESULT(0)
        }
        WM_APP => {
            // QT 数据到达(id 0xFF00):drain → feed → 应答 → draw
            QT.with(|q| {
                let mut guard = q.borrow_mut();
                let Some(st) = guard.as_mut() else { return };
                // T08 批预算(同主窗):drain_batch 内含补通知(含续批)
                let Some(bytes) = st
                    .terminal
                    .pty_buf
                    .drain_batch(crate::app::pty_bridge::UI_BATCH_BYTES)
                else {
                    return;
                };
                st.terminal.term.feed(&bytes);
                // 应答必须写回 pty(否则 shell 卡在等应答)。走手持引用的
                // 内入口而非 accept_qt 全局面:WM_APP 此处已持 QT 的
                // borrow_mut,全局面再借即 panic(生产路径条件性崩溃,
                // 回归见本模块 tests);且应答是协议字节,不得包
                // bracketed paste——与主窗 stage_input 同语义
                for reply in st.terminal.term.take_pty_writes() {
                    let _ = accept_qt_input(&mut st.terminal, reply.as_bytes());
                }
                if let Some(t) = st.terminal.term.take_title() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowTextW(
                        hwnd,
                        &HSTRING::from(format!("QT: {t}")),
                    );
                }
                qt_draw(st);
            });
            LRESULT(0)
        }
        WM_CHAR => {
            // 主窗同款代理对重组,但暂存于 QT 状态本地
            let code = wparam.0 as u32;
            QT.with(|q| {
                let mut guard = q.borrow_mut();
                let Some(st) = guard.as_mut() else { return };
                let ch = match code {
                    0xD800..=0xDBFF => {
                        st.pending_surrogate = code;
                        return;
                    }
                    0xDC00..=0xDFFF => {
                        let hi = std::mem::take(&mut st.pending_surrogate);
                        if hi == 0 {
                            return;
                        }
                        char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (code - 0xDC00))
                            .expect("surrogate pair")
                    }
                    _ => char::from_u32(code).unwrap_or('\u{FFFD}'),
                };
                // kitty 模式下 C0 已由 WM_KEYDOWN 侧编码成 CSI-u,这里再发
                // 就是双发(主窗同款契约);可打印文本照旧走此处
                if st.terminal.term.kitty_flags().any() && (ch as u32) < 0x20 {
                    return;
                }
                // Windows 把退格发成 0x08,终端世界统一 DEL(0x7f)
                let bytes: Vec<u8> = if ch == '\u{8}' {
                    vec![0x7f]
                } else {
                    ch.to_string().into_bytes()
                };
                let _ = accept_qt_input(&mut st.terminal, &bytes);
            });
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // M5a/T8(M3b 打磨票清账):对齐主窗输入契约——Enter/Tab/Esc/
            // Backspace/可打印走 WM_CHAR(修双发:旧代码两臂都写);kitty 时
            // 功能键 CSI-u;legacy 时方向/编辑键经 vkey_bytes(补修饰键,
            // 修 Ctrl+方向跳词/Shift+翻页等)。QT 无 keymap 拦截(单 pane)
            let vk = wparam.0 as u32;
            let mods = super::current_mods();
            QT.with(|q| {
                let mut guard = q.borrow_mut();
                let Some(st) = guard.as_mut() else {
                    return;
                };
                let flags = st.terminal.term.kitty_flags();
                let kind = if lparam.0 & (1 << 30) != 0 {
                    mica_core::protocol::kitty::EventKind::Repeat
                } else {
                    mica_core::protocol::kitty::EventKind::Press
                };
                if flags.any()
                    && let Some(key) = super::vk_to_key(vk)
                    && let Some(bytes) =
                        mica_core::protocol::kitty::kitty_encode(key, mods, kind, flags)
                {
                    let _ = accept_qt_input(&mut st.terminal, &bytes);
                    return;
                }
                if let Some(bytes) = super::vkey_bytes(vk, mods, st.terminal.term.app_cursor_mode())
                {
                    let _ = accept_qt_input(&mut st.terminal, &bytes);
                }
            });
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let _ = windows::Win32::Graphics::Gdi::ValidateRect(Some(hwnd), None);
            LRESULT(0)
        }
        WM_SIZE => {
            // 高度变化重算网格(M3b:初始即定尺寸,此分支留 resize 期)
            LRESULT(0)
        }
        WM_DESTROY => {
            // T04:QT 与主窗同线程——QT 销毁绝不能 PostQuitMessage(会把
            // 主窗消息循环一并掐死,主窗沦为未销毁的假退出)。QT 随进程退出
            let _ = unregister_hotkey();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn unregister_hotkey() -> windows::core::Result<()> {
    let raw = QT_HWND.with(|c| c.take());
    if raw == 0 {
        return Ok(());
    }
    let hwnd = HWND(raw as *mut std::ffi::c_void);
    unsafe { UnregisterHotKey(Some(hwnd), 1) }
}

fn qt_draw(st: &mut QtState) {
    let t = &mut st.terminal;
    let fingerprint = if t.router.needs_resource_recovery() {
        mica_render::frame::resource_fingerprint(&t.term) ^ t.exited.unwrap_or(0) as u64
    } else {
        0
    };
    t.router
        .prepare_frame(fingerprint, st.renderer.max_atlas_dimension());
    let version = t.router.atlas_version();
    if version != t.last_atlas_ver {
        t.last_atlas_ver = version;
        t.force_full = true;
    }
    let damage = if std::mem::take(&mut t.force_full) {
        let _ = t.term.take_damage();
        Damage::Full
    } else {
        t.term.take_damage()
    };
    let display_offset = t.term.display_offset();
    let selection = t.term.selection_range();
    let cursor = t.term.cursor_shape();
    build_rows(
        &t.term,
        &mut t.router,
        &t.metrics,
        &t.palette,
        selection.as_ref(),
        display_offset,
        Some(cursor),
        &damage,
        &mut t.row_insts,
    );
    let mut instances = repack(&t.row_insts);
    // #37:退出横幅(顶行,后发先至盖在内容上):色板红/绿槽 + 白字,
    // 提示收起后重开。先于图集装载段(路由要可变借 router)
    if let Some(code) = t.exited {
        let slot = if code == 0 {
            t.palette.colors[2]
        } else {
            t.palette.colors[1]
        };
        let label = format!("[exit {code}] 会话已结束——按热键收起,再按即重开");
        let w = st.config.width as f32;
        let h = t.metrics.line_height;
        let c = |v: u8| v as f32 / 255.0;
        let bg = [c(slot.r), c(slot.g), c(slot.b), 0.0];
        let fg = [1.0, 1.0, 1.0, 0.0];
        instances.push(mica_render::frame::CellInstance {
            pos_uv: [0.0, 0.0, 0.0, 0.0],
            size_uv: [w, h, 0.0, 0.0],
            fg: bg,
            bg,
        });
        let mut x = 4.0;
        for ch in label.chars() {
            let g = <DwriteRouter as mica_render::font::router::GlyphRouter>::route(
                &mut t.router,
                ch,
                mica_render::font::GlyphStyle::PLAIN,
            );
            if g.size_px[0] > 0.0 {
                instances.push(mica_render::frame::CellInstance {
                    pos_uv: [x + g.offset_px[0], g.offset_px[1], g.uv[0], g.uv[1]],
                    size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                    fg,
                    bg,
                });
            }
            x += t.metrics.cell_width;
        }
    }
    // Always bind by true atlas identity; also retries GPU quota recovery when
    // the CPU revision has not changed. Renderer handles no-upload cache hits.
    st.renderer.set_atlas(&t.router.atlas());
    st.renderer.draw(&st.wgpu_surface, &st.config, &instances);
}

#[cfg(test)]
mod tests {
    //! WM_APP 应答回写回归(2026-11-10 审计 P1):应答写回曾走 accept_qt
    //! 全局面,而 WM_APP 已持 QT.borrow_mut()——双重借用 panic 于 wndproc,
    //! 进程 abort(生产路径条件性崩溃:shell 发查询 → 终端应答 → 炸)。
    //! 真 init(真窗口/真 GPU/真会话)后向 backlog 注入 DSR 查询、
    //! SendMessage 同步分发 WM_APP:修复前本测试以进程 abort 告红,存活
    //! 到断言即绿。注入理由:ConPTY 剥离查询序列,端到端喂不进来;注入
    //! 点在转发线程下游,自 WM_APP 消费起仍是全真生产路径。

    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_CLOSE};

    #[test]
    fn reply_writeback_survives_held_qt_borrow() {
        crate::app::seed_tls_for_tests(&mica_core::config::settings::Settings::default());
        // SAFETY: 本线程创建并拥有 QT 窗口;hwnd_main 仅用于显示器查询
        // (null → 默认显示器)。测试尾走 WM_CLOSE → DestroyWindow 收尾
        unsafe { init(HWND(std::ptr::null_mut())) }.expect("quickterm init");
        let raw = QT_HWND.with(|c| c.get());
        assert_ne!(raw, 0, "init 应登记 QT hwnd");
        let hwnd = HWND(raw as *mut std::ffi::c_void);
        let backlog = backlog_for_test().expect("QT 状态已装载");

        // 两条 DSR 查询一批到达:feed 产生两条应答,覆盖应答 for 循环
        backlog.push(0, b"\x1b[6n\x1b[6n").expect("注入查询");
        // SAFETY: 同线程同步分发;修复前 panic 于此后 abort,到不了断言
        unsafe { SendMessageW(hwnd, WM_APP, Some(WPARAM(0xFF00)), Some(LPARAM(0))) };
        assert!(backlog_for_test().is_some(), "应答循环后 QT 状态仍可借用");

        // 第二轮:路径可重复进入(连续查询场景)
        backlog.push(0, b"\x1b[6n").expect("再注入");
        unsafe { SendMessageW(hwnd, WM_APP, Some(WPARAM(0xFF00)), Some(LPARAM(0))) };

        // SAFETY: 创建线程同步关窗(WM_DESTROY 注销热键),forget_state
        // 杀子回收会话(真 powershell 子进程不留孤儿)
        unsafe { SendMessageW(hwnd, WM_CLOSE, Some(WPARAM(0)), Some(LPARAM(0))) };
        forget_state();
    }
}
