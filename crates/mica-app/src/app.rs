//! The M0 shell: one window, one terminal, ConPTY + PowerShell.
//!
//! Input/render run on the main thread; pty output arrives on the reader
//! thread and is drained each loop iteration. M1 replaces this 8ms polling
//! loop with an event-driven wake-up (reader thread posts to the queue).

// 本模块整体是 Win32 FFI 区,再套细粒度 unsafe 块只是噪音(edition 2024 默认告警)
#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::num::NonZeroIsize;
use std::time::Duration;

use mica_core::input::{self, Key, Mods};
use mica_core::pty::{PtyReader, PtySession, default_shell_command};
use mica_core::surface::{CELL_HEIGHT, CELL_WIDTH, ScreenSize, Surface};
use mica_render::frame::build_instances;
use mica_render::pipeline::{Renderer, create_context};
use windows::Win32::Foundation::{HBRUSH, HICON, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VIRTUAL_KEY, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_HOME, VK_LEFT, VK_MENU,
    VK_NEXT, VK_PRIOR, VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, GetClientRect, LoadCursorW, MSG, PM_REMOVE, PeekMessageW, PostQuitMessage,
    RegisterClassExW, SetWindowTextW, TranslateMessage, WINDOW_EX_STYLE, WM_CHAR, WM_DESTROY,
    WM_KEYDOWN, WM_PAINT, WM_QUIT, WM_SIZE, WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::core::{HSTRING, PCWSTR, w};

const COLS: u16 = 80;
const ROWS: u16 = 24;

thread_local! {
    static STATE: RefCell<Option<Terminal>> = const { RefCell::new(None) };
}

struct Terminal {
    term: Surface,
    session: PtySession,
    reader: PtyReader,
    ctx: mica_render::pipeline::GpuContext,
    renderer: Renderer,
    wgpu_surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    cols: u16,
    rows: u16,
}

pub fn run() {
    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None).expect("GetModuleHandleW").0);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: HICON::default(),
            hCursor: LoadCursorW(None, windows::Win32::UI::WindowsAndMessaging::IDC_ARROW)
                .expect("no arrow cursor"),
            hbrBackground: HBRUSH::default(), // 客户区由 wgpu 每帧清屏
            lpszMenuName: PCWSTR::null(),
            lpszClassName: w!("mica_app_class"),
            hIconSm: HICON::default(),
        };
        assert_ne!(RegisterClassExW(&wc), 0, "RegisterClassExW failed");

        // 客户区 80x24 格,反推窗口外框
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: i32::from(COLS) * i32::from(CELL_WIDTH),
            bottom: i32::from(ROWS) * i32::from(CELL_HEIGHT),
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false).expect("AdjustWindowRect");
        let class_name = HSTRING::from("mica_app_class"); // Param<PCWSTR> 的已证实实现
        let title = HSTRING::from("Mica");
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &class_name,
            &title,
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(hinstance),
            None,
        )
        .expect("CreateWindowExW failed");

        // 暗色标题栏;Win10 1809 前忽略失败
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );

        init_terminal(hwnd);
        message_loop(hwnd);
    }
}

unsafe fn init_terminal(hwnd: HWND) {
    // wgpu 30:display handle 挂在 Instance 上,窗口路线用无显示构造
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    // SAFETY: hwnd 活到进程结束,长于 wgpu_surface
    // (注:display=Some(Windows) 与 None 两种组合均未实测;若此处报错,
    //  先把 raw_display_handle 改成 None 再试——两种都符合 rwh 0.6 类型)
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
    let config = wgpu_surface
        .get_default_config(&ctx.adapter, width, height)
        .expect("surface unsupported by adapter");
    wgpu_surface.configure(&ctx.device, &config);
    let renderer = Renderer::new(&ctx, config.format);

    let cols = (width / u32::from(CELL_WIDTH)).max(1) as u16;
    let rows = (height / u32::from(CELL_HEIGHT)).max(1) as u16;
    let term = Surface::new(ScreenSize::new(cols as usize, rows as usize));
    let (session, reader) =
        PtySession::spawn(default_shell_command(), cols, rows).expect("spawn shell");

    STATE.with(|cell| {
        *cell.borrow_mut() = Some(Terminal {
            term,
            session,
            reader,
            ctx,
            renderer,
            wgpu_surface,
            config,
            cols,
            rows,
        });
    });
}

unsafe fn message_loop(hwnd: HWND) {
    let mut msg = MSG::default();
    loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).0 != 0 {
            if msg.message == WM_QUIT {
                return; // Terminal 的 Drop 会杀掉子 shell
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let mut needs_redraw = false;
        STATE.with(|cell| {
            let Some(t) = cell.borrow_mut().as_mut() else {
                return;
            };
            let bytes = t.reader.drain();
            if bytes.is_empty() {
                return;
            }
            t.term.feed(&bytes);
            // 终端对查询的应答(DSR/OSC)必须写回 pty,否则 shell 会卡在等待
            for reply in t.term.take_pty_writes() {
                let _ = t.session.write(reply.as_bytes());
            }
            if let Some(title) = t.term.take_title() {
                let _ = SetWindowTextW(hwnd, &HSTRING::from(title));
            }
            needs_redraw = true;
        });

        if needs_redraw {
            draw_frame();
        }
        // M0 轮询节流;M1 改为 reader 线程向队列投递消息唤醒
        std::thread::sleep(Duration::from_millis(8));
    }
}

fn draw_frame() {
    STATE.with(|cell| {
        let Some(t) = cell.borrow_mut().as_mut() else {
            return;
        };
        let instances = build_instances(&t.term);
        t.renderer.draw(&t.wgpu_surface, &t.config, &instances);
    });
}

/// 输入约定(见 mica_core::input 文档):Enter/Tab/Esc/Backspace/普通字符走
/// WM_CHAR;方向/编辑键没有字符事件,走 WM_KEYDOWN 的 VK 映射。
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CHAR => {
            STATE.with(|cell| {
                let Some(t) = cell.borrow_mut().as_mut() else {
                    return;
                };
                // Windows 把退格发成 0x08,终端世界统一 DEL(0x7f)
                let bytes: Vec<u8> = match char::from_u32(wparam.0 as u32) {
                    Some('\u{8}') => vec![0x7f],
                    Some(c) => c.to_string().into_bytes(), // UTF-8,中文/控制字符原样
                    None => return,
                };
                let _ = t.session.write(&bytes);
            });
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let bytes = vkey_bytes(wparam.0 as u32, current_mods());
            if let Some(bytes) = bytes {
                STATE.with(|cell| {
                    if let Some(t) = cell.borrow_mut().as_mut() {
                        let _ = t.session.write(&bytes);
                    }
                });
            }
            LRESULT(0)
        }
        WM_SIZE => {
            // lparam 低位 = 客户区宽,高位 = 客户区高(像素)
            let width = (lparam.0 & 0xffff) as u32;
            let height = ((lparam.0 >> 16) & 0xffff) as u32;
            if width == 0 || height == 0 {
                return LRESULT(0); // 最小化
            }
            let mut resized = false;
            STATE.with(|cell| {
                let Some(t) = cell.borrow_mut().as_mut() else {
                    return;
                };
                let cols = (width / u32::from(CELL_WIDTH)).max(1) as u16;
                let rows = (height / u32::from(CELL_HEIGHT)).max(1) as u16;
                if cols == t.cols && rows == t.rows {
                    return;
                }
                t.cols = cols;
                t.rows = rows;
                t.term.resize(ScreenSize::new(cols as usize, rows as usize));
                let _ = t.session.resize(cols, rows);
                t.config.width = width;
                t.config.height = height;
                t.wgpu_surface.configure(&t.ctx.device, &t.config);
                resized = true;
            });
            // draw_frame 自己也要借 STATE,必须在 with 之外调用
            if resized {
                draw_frame();
            }
            LRESULT(0)
        }
        WM_PAINT => {
            // 绘制节奏由消息循环控制;这里只清掉无效区积压
            let _ = ValidateRect(Some(hwnd), None);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn vkey_bytes(vk: u32, mods: Mods) -> Option<Vec<u8>> {
    let key = match VIRTUAL_KEY(vk as u16) {
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
    input::encode(key, mods)
}

fn current_mods() -> Mods {
    unsafe {
        let down = |vk: VIRTUAL_KEY| GetKeyState(vk.0 as i32) < 0;
        Mods {
            shift: down(VK_SHIFT),
            alt: down(VK_MENU),
            ctrl: down(VK_CONTROL),
        }
    }
}
