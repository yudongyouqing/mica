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
    PostQuitMessage, RegisterClassExW, SW_HIDE, ShowWindow, WINDOW_EX_STYLE, WM_ACTIVATE, WM_APP,
    WM_CHAR, WM_DESTROY, WM_ERASEBKGND, WM_HOTKEY, WM_KEYDOWN, WM_PAINT, WM_SIZE, WNDCLASSEXW,
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
pub unsafe fn init(hwnd_main: HWND) -> u32 {
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
    QT_HWND.with(|c| c.set(hwnd_qt.0 as usize));

    // 热键:Win+`(可配;失败记日志,QT 仍可用 IPC 激活留 M3c)
    // id=1 与主窗不冲突(RegisterHotKey 按线程+id 域)
    let hotkey_ok = RegisterHotKey(Some(hwnd_qt), 1, MOD_WIN | MOD_NOREPEAT, VK_OEM_3.0 as u32);
    if hotkey_ok.is_err() {
        eprintln!("quickterm: Win+` 注册失败(被占用?)——QT 需重配 quick-terminal-key");
    }

    // QT 终端状态:复用 TAB_SEED/CURRENT_PALETTE 建字体与 pty
    let (families, size_pt) = crate::app::tab_seed();
    let fam_ref: Vec<&str> = families.iter().map(String::as_str).collect();
    let Ok(router) = DwriteRouter::new(size_pt, &fam_ref) else {
        eprintln!("quickterm: 字体链失败,QT 禁用");
        return 0;
    };
    let metrics = router.metrics();
    let palette = crate::app::current_palette();

    // wgpu surface + renderer(独立于主窗)
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let wgpu_surface: wgpu::Surface<'static> = instance
        .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: None,
            raw_window_handle: raw_window_handle::RawWindowHandle::Win32(
                raw_window_handle::Win32WindowHandle::new(
                    std::num::NonZeroIsize::new(hwnd_qt.0 as isize).expect("hwnd"),
                ),
            ),
        })
        .expect("QT surface");
    let ctx =
        pollster::block_on(create_context(&instance, Some(&wgpu_surface))).expect("QT GPU context");
    let mut config = wgpu_surface
        .get_default_config(&ctx.adapter, w_px as u32, h_px as u32)
        .expect("QT config");
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
    let mut renderer = Renderer::new(&ctx, config.format);
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
    let (session, reader) = mica_core::pty::PtySession::spawn(
        mica_core::pty::command_from_str("powershell.exe -NoLogo"),
        cols,
        rows,
    )
    .expect("QT pty");
    let pty_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    term.set_clipboard_provider(Arc::new(|| {
        crate::app::clipboard::get_text().unwrap_or_default()
    }));

    let terminal = Terminal {
        term,
        session,
        pty_buf: Arc::clone(&pty_buf),
        router,
        metrics,
        palette,
        cols,
        rows,
        renderer_atlas_revision: u64::MAX,
        row_insts: Vec::new(),
        force_full: true,
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
    if let Some(slot) = crate::app::shared_hwnd_for_quickterm() {
        crate::app::spawn_qt_forwarder(reader, pty_buf, slot);
    }
    let _ = class_name; // 注册已用 w! 字面量
    1
}

/// 切换:可见→隐藏;隐藏→显示+置顶激活。
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
        WM_APP => {
            // QT 数据到达(id 0xFF00):drain → feed → 应答 → draw
            QT.with(|q| {
                let mut guard = q.borrow_mut();
                let Some(st) = guard.as_mut() else { return };
                let bytes =
                    std::mem::take(&mut *st.terminal.pty_buf.lock().expect("qt buf poisoned"));
                if bytes.is_empty() {
                    return;
                }
                st.terminal.term.feed(&bytes);
                for reply in st.terminal.term.take_pty_writes() {
                    let _ = st.terminal.session.write(reply.as_bytes());
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
                let bytes: Vec<u8> = if ch == '\u{8}' {
                    vec![0x7f]
                } else {
                    ch.to_string().into_bytes()
                };
                let _ = st.terminal.session.write(&bytes);
            });
            LRESULT(0)
        }
        WM_KEYDOWN => {
            use mica_core::input::{self, Key, Mods};
            // QT 无 keymap 拦截需求(单 pane);直接 pty 编码
            let vk = wparam.0 as u32;
            let mods = Mods::NONE; // 修饰读取省略:M3b 打磨票(vk->xterm 主键位已够用)
            let key = match vk {
                0x26 => Key::Up,
                0x28 => Key::Down,
                0x25 => Key::Left,
                0x27 => Key::Right,
                0x24 => Key::Home,
                0x23 => Key::End,
                0x2E => Key::Delete,
                0x21 => Key::PageUp,
                0x22 => Key::PageDown,
                0x0D => Key::Enter,
                0x08 => Key::Backspace,
                0x09 => Key::Tab,
                _ => {
                    return LRESULT(0); // 可打印字符走 WM_CHAR
                }
            };
            let bytes = input::encode(key, mods, false);
            QT.with(|q| {
                if let Some(st) = q.borrow_mut().as_mut() {
                    let _ = st.terminal.session.write(&bytes);
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
            let _ = unregister_hotkey();
            PostQuitMessage(0);
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
    let revision = t.router.atlas_revision();
    let atlas = t.router.atlas();
    if t.renderer_atlas_revision != revision {
        st.renderer.set_atlas(atlas);
        t.renderer_atlas_revision = revision;
    }
    let instances = repack(&t.row_insts);
    st.renderer.draw(&st.wgpu_surface, &st.config, &instances);
}
