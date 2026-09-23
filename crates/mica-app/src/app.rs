//! The M1a shell: one window, one terminal, ConPTY + PowerShell,
//! DirectWrite-rasterized glyphs (DwriteRouter owns metrics and the atlas).
//!
//! Input/render run on the main thread; pty output arrives on the reader
//! thread and is drained each loop iteration. M1 replaces this 8ms polling
//! loop with an event-driven wake-up (reader thread posts to the queue).

// 本模块整体是 Win32 FFI 区,再套细粒度 unsafe 块只是噪音(edition 2024 默认告警)
#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::num::NonZeroIsize;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{Event, RecursiveMode, Watcher};

use mica_core::config::palette::Palette;
use mica_core::config::settings::{self, ConfigError, DEFAULT_FAMILIES, Settings};
use mica_core::input::{self, Key, Mods};
use mica_core::pty::{PtyReader, PtySession, default_shell_command};
use mica_core::surface::{ScreenSize, Surface};
use mica_render::font::dwrite::DwriteRouter;
use mica_render::font::metrics::FontMetrics;
use mica_render::frame::build_instances;
use mica_render::pipeline::{Renderer, create_context};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VIRTUAL_KEY, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_HOME, VK_LEFT, VK_MENU,
    VK_NEXT, VK_PRIOR, VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::HICON;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, GetClientRect, LoadCursorW, MSG, MessageBoxW, PM_REMOVE, PeekMessageW,
    PostMessageW, PostQuitMessage, RegisterClassExW, SetWindowTextW, TranslateMessage,
    WINDOW_EX_STYLE, WM_CHAR, WM_DESTROY, WM_KEYDOWN, WM_PAINT, WM_QUIT, WM_SIZE, WNDCLASSEXW,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK};
use windows::core::{HSTRING, PCWSTR, w};

const COLS: u16 = 100;
const ROWS: u16 = 30;

/// WM_APP 用户消息区(0x8000 起)。+1 预留给 Task 7 的 WM_APP_RENDER
/// (pty 读线程唤醒渲染),勿占用
const WM_APP_CONFIG: u32 = 0x8000 + 2;

/// watch 线程持有的窗口句柄哨兵:退出流程先置 None 再停线程,保证不再向
/// (可能已销毁的)窗口投递消息;Post 到死句柄本就无害,纪律照守(T7 同序)。
/// 存裸 isize 而非 HWND——windows-rs 的句柄包着 *mut c_void,不是 Send,
/// 投递侧再包回 HWND。
type SharedHwnd = Arc<Mutex<Option<isize>>>;

/// 热重载组件打包:退出时按 哨兵置 None → drop watcher → join 线程 拆除
/// (序在 message_loop 的 WM_QUIT 分支,先于既有 teardown)。
struct ReloadHandle {
    hwnd_slot: SharedHwnd,
    watcher: notify::RecommendedWatcher,
    thread: std::thread::JoinHandle<()>,
}

thread_local! {
    static STATE: RefCell<Option<Terminal>> = const { RefCell::new(None) };
    /// 非 BMP 字符(emoji、扩展区汉字)以 UTF-16 代理对各发一次 WM_CHAR,
    /// 高代理暂存于此,低代理到达时重组成码点
    static PENDING_SURROGATE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

struct Terminal {
    term: Surface,
    session: PtySession,
    reader: PtyReader,
    router: DwriteRouter,
    metrics: FontMetrics,
    ctx: mica_render::pipeline::GpuContext,
    renderer: Renderer,
    /// 配置合成出的调色板:渲染实例着色与 OSC 4/10/11/12 应答同源(T2 Settings)
    palette: Palette,
    wgpu_surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    cols: u16,
    rows: u16,
    /// 已传给 Renderer 的图集修订号(C1:普通 insert 也递增);u64::MAX
    /// 起步保证空图集(修订 0)也完成首次上传,不踩 set_atlas 契约
    renderer_atlas_revision: u64,
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

        // T4:字号与字体链来自配置(%APPDATA%\mica\config;缺失 = 全默认)
        let settings = load_settings();
        let router = {
            let families: Vec<&str> = if settings.font_families.is_empty() {
                DEFAULT_FAMILIES.to_vec() // resolve 恒填默认链,此分支纯防御
            } else {
                settings.font_families.iter().map(String::as_str).collect()
            };
            DwriteRouter::new(settings.font_size_pt, &families).expect("no fonts resolved")
        };
        eprintln!("font families in use: {:?}", router.families_in_use()); // 冒烟期观察回退链
        let metrics = router.metrics();

        // 客户区 COLSxROWS 格,反推窗口外框
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: (f32::from(COLS) * metrics.cell_width).round() as i32,
            bottom: (f32::from(ROWS) * metrics.line_height).round() as i32,
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

        // T5:热重载 watcher 在窗口创建后启动(投递 WM_APP_CONFIG 需要 hwnd);
        // config 文件缺失(全默认启动)则不监听——首次创建配置需重启生效
        let reload = spawn_config_watcher(hwnd);
        init_terminal(hwnd, router, metrics, settings);
        message_loop(hwnd, reload);
    }
}

/// 配置文件路径:`%APPDATA%\mica\config`;APPDATA 未设(理论上不存在,
/// 保留兜底)落当前目录 `.mica\config`。
fn config_path() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(dir) if !dir.is_empty() => Path::new(&dir).join("mica").join("config"),
        _ => PathBuf::from(".mica").join("config"),
    }
}

/// 读配置并合成 Settings。错误哲学(T2 同款):终端绝不因配置拒绝启动——
/// 文件缺失 = 全默认静默;读失败或 resolve 失败 = MessageBox 警告后回落默认。
fn load_settings() -> Settings {
    let path = config_path();
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
        Err(e) => {
            warn_box(&format!(
                "无法读取配置文件 {}:\n{e}\n\n继续使用默认设置。",
                path.display()
            ));
            return Settings::default();
        }
    };
    match settings::resolve(&source) {
        Ok(s) => s,
        Err(err) => {
            warn_box(&format_config_error(&path, &err));
            Settings::default()
        }
    }
}

/// 错误弹窗文案:解析行错误带行号在前,值语义错误随后(与 ConfigError 同序)。
fn format_config_error(path: &Path, err: &ConfigError) -> String {
    let mut msg = format!("配置文件 {} 有误,已回退默认设置:", path.display());
    for e in &err.parse {
        msg.push_str(&format!("\n  第 {} 行: {}", e.line, e.reason));
    }
    for v in &err.values {
        msg.push_str(&format!("\n  {v}"));
    }
    msg
}

/// 警告框:不挂窗口(启动期 hwnd 尚未创建),自带消息泵,OK 后继续初始化。
fn warn_box(text: &str) {
    unsafe {
        let _ = MessageBoxW(
            None,
            &HSTRING::from(text),
            w!("Mica"),
            MB_OK | MB_ICONWARNING,
        );
    }
}

/// 配置热重载 watcher(最佳努力,绝不致命——T4 错误哲学同款):
/// notify 推荐后端(Windows = ReadDirectoryChangesW;对文件路径它实际监视
/// 父目录并按路径过滤,原子替换/重建不失联)监听 config 文件,事件经 mpsc
/// 交给去抖线程。None = 未监听(文件缺失或 watch 失败,热重载降级关闭)。
fn spawn_config_watcher(hwnd: HWND) -> Option<ReloadHandle> {
    let path = config_path();
    if !path.exists() {
        eprintln!("config watcher: {} 不存在,热重载未启用", path.display());
        return None;
    }
    // notify 的 EventHandler 直接支持 std mpsc Sender<Result<Event>>;
    // watcher 持有发送端,Drop 时后端停止、通道断开,线程随之退出
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx).expect("create config watcher");
    if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
        eprintln!(
            "config watcher: 无法监听 {}: {e};热重载未启用",
            path.display()
        );
        return None;
    }
    let hwnd_slot: SharedHwnd = Arc::new(Mutex::new(Some(hwnd.0 as isize)));
    let thread_slot = Arc::clone(&hwnd_slot);
    let thread = std::thread::Builder::new()
        .name("config-watcher".into())
        .spawn(move || debounce_loop(path, rx, thread_slot))
        .expect("spawn config watcher thread");
    Some(ReloadHandle {
        hwnd_slot,
        watcher,
        thread,
    })
}

/// 去抖线程:notify 后端已按被监视文件过滤事件,收到的必是 config 的动静。
/// 编辑器原子保存 = Delete+Create+Write 连发,不去抖会触发多轮重载并撞上
/// 瞬时缺失窗——故收首个事件后持续吞事件,直到 300ms 静默窗走完才投递一次;
/// 路径在静默结束时仍不存在(原子替换途中)则跳过本轮。通道断开(watcher
/// 已 Drop)即退出。
fn debounce_loop(path: PathBuf, rx: mpsc::Receiver<notify::Result<Event>>, hwnd_slot: SharedHwnd) {
    const QUIET: Duration = Duration::from_millis(300);
    loop {
        if rx.recv().is_err() {
            return; // watcher 已停,通道断开
        }
        loop {
            match rx.recv_timeout(QUIET) {
                Ok(_) => {} // 风暴未息,续等
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        if !path.exists() {
            continue;
        }
        let hwnd = *hwnd_slot.lock().expect("hwnd slot poisoned");
        if let Some(hwnd) = hwnd {
            // 失败(队列满/窗口已死)忽略即可;Post 到死句柄无害
            let hwnd = HWND(hwnd as *mut std::ffi::c_void);
            let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_CONFIG, WPARAM(0), LPARAM(0)) };
        }
    }
}

/// 热重载主路径(WM_APP_CONFIG,主线程):重读+合成;失败弹窗保旧、现状态
/// 不动;成功则重建 DwriteRouter(旧 router Drop 释放图集,新字形随绘制自然
/// 重光栅化,无需显式清缓存)、按**不变的窗口像素尺寸**重算网格、resize 终端
/// 与会话、换 palette 与清屏色,最后全量重绘。
unsafe fn reload_config(hwnd: HWND) {
    let path = config_path();
    let Ok(source) = std::fs::read_to_string(&path) else {
        // 去抖后仍读不到:文件被删而非瞬时缺失,提示后保旧
        warn_box(&format!(
            "无法读取配置文件 {}:\n热重载跳过,继续使用当前设置。",
            path.display()
        ));
        return;
    };
    let settings = match settings::resolve(&source) {
        Ok(s) => s,
        Err(err) => {
            warn_box(&format_config_error(&path, &err));
            return; // 保旧
        }
    };
    let families: Vec<&str> = settings.font_families.iter().map(String::as_str).collect();
    let Ok(router) = DwriteRouter::new(settings.font_size_pt, &families) else {
        warn_box("重建字体链失败,继续使用当前设置。");
        return; // 保旧
    };
    eprintln!("font families in use: {:?}", router.families_in_use()); // 冒烟期观察回退链
    let metrics = router.metrics();

    let mut reloaded = false;
    STATE.with(|cell| {
        let mut t_guard = cell.borrow_mut();
        let Some(t) = t_guard.as_mut() else {
            return;
        };
        // 客户区像素尺寸不变(不碰窗口尺寸),按新度量重算网格
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect).expect("GetClientRect");
        let width = rect.right.max(1) as u32;
        let height = rect.bottom.max(1) as u32;
        let cols = ((width as f32 / metrics.cell_width).max(1.0)) as u16;
        let rows = ((height as f32 / metrics.line_height).max(1.0)) as u16;

        t.router = router;
        t.metrics = metrics;
        // 修订号镜像回到 u64::MAX:新路由的空图集(修订 0)也保证完成首次
        // 上传,不会拿旧图集渲染新字形(与 init 同款契约)
        t.renderer_atlas_revision = u64::MAX;
        t.cols = cols;
        t.rows = rows;
        t.term.resize(ScreenSize::new(cols as usize, rows as usize));
        // 应答值取整与布局换算 f32 的分工同 init(I3)
        t.term.set_cell_metrics(
            metrics.cell_width.round() as u16,
            metrics.line_height.round() as u16,
        );
        let _ = t.session.resize(cols, rows);
        t.term.set_palette(&settings.palette);
        t.renderer.set_clear_color(&settings.palette);
        t.palette = settings.palette;
        reloaded = true;
    });
    // draw_frame 自己也要借 STATE,必须在 with 之外调用
    if reloaded {
        draw_frame();
    }
}

unsafe fn init_terminal(
    hwnd: HWND,
    router: DwriteRouter,
    metrics: FontMetrics,
    settings: Settings,
) {
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
    // T4:Globals 不再携带 cell/shader v2 纯矩形化,Renderer::new 退掉 cell 参
    let mut renderer = Renderer::new(&ctx, config.format);

    let cols = ((width as f32 / metrics.cell_width).max(1.0)) as u16;
    let rows = ((height as f32 / metrics.line_height).max(1.0)) as u16;
    let mut term = Surface::new(ScreenSize::new(cols as usize, rows as usize));
    // 查询应答(DSR/OSC 尺寸)用格子度量;应答值取整不截断(I3:真度量如
    // 8.53px 时 as u16 会答 8,应答与像素网格漂移;布局换算保持 f32)
    term.set_cell_metrics(
        metrics.cell_width.round() as u16,
        metrics.line_height.round() as u16,
    );
    // 调色板接线:OSC 4/10/11/12 应答与渲染/清屏同源,单一来源是 Settings
    // 解析出的 Palette(主题片段 < 用户 config 覆盖后合成)。
    term.set_palette(&settings.palette);
    renderer.set_clear_color(&settings.palette);
    let (session, reader) =
        PtySession::spawn(default_shell_command(), cols, rows).expect("spawn shell");

    STATE.with(|cell| {
        *cell.borrow_mut() = Some(Terminal {
            term,
            session,
            reader,
            router,
            metrics,
            ctx,
            renderer,
            palette: settings.palette,
            wgpu_surface,
            config,
            cols,
            rows,
            renderer_atlas_revision: u64::MAX,
        });
    });
}

unsafe fn message_loop(hwnd: HWND, reload: Option<ReloadHandle>) {
    let mut msg = MSG::default();
    loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).0 != 0 {
            if msg.message == WM_QUIT {
                // 退出序(参照 T7 纪律):先落 None 哨兵(watch 线程此后不再
                // Post)→ drop watcher(其 Drop 停止后端并断开发送端,线程随之
                // 退出)→ join 兜底;完事才做既有 teardown(Terminal 的 Drop
                // 杀 pty)。残余 Post 到已销毁窗口只是失败返回,无害
                if let Some(reload) = reload {
                    *reload.hwnd_slot.lock().expect("hwnd slot poisoned") = None;
                    drop(reload.watcher);
                    let _ = reload.thread.join();
                }
                STATE.with(|cell| cell.borrow_mut().take());
                return; // Terminal 的 Drop 在 TLS 存活时显式执行,退出期回调不再摸已销毁的 STATE
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let mut needs_redraw = false;
        STATE.with(|cell| {
            let mut t_guard = cell.borrow_mut();
            let Some(t) = t_guard.as_mut() else {
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
        let mut t_guard = cell.borrow_mut();
        let Some(t) = t_guard.as_mut() else {
            return;
        };
        // 先 build(路由新字形、改图集)再比修订号:同帧新增字形同帧上传
        let instances = build_instances(&t.term, &mut t.router, &t.metrics, &t.palette);
        let revision = t.router.atlas_revision();
        if t.renderer_atlas_revision != revision {
            t.renderer.set_atlas(t.router.atlas());
            t.renderer_atlas_revision = revision;
        }
        t.renderer.draw(&t.wgpu_surface, &t.config, &instances);
    });
}

/// 输入约定(见 mica_core::input 文档):Enter/Tab/Esc/Backspace/普通字符走
/// WM_CHAR;方向/编辑键没有字符事件,走 WM_KEYDOWN 的 VK 映射。
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CHAR => {
            // 代理对重组:Rust 的 char 不含代理码位,不重组的话 emoji
            // 两个 WM_CHAR 都在 from_u32 处变 None 被丢弃,永远发不出去
            let code = wparam.0 as u32;
            let ch = match code {
                0xD800..=0xDBFF => {
                    PENDING_SURROGATE.with(|s| s.set(code));
                    return LRESULT(0); // 等低代理
                }
                0xDC00..=0xDFFF => {
                    let hi = PENDING_SURROGATE.with(|s| s.replace(0));
                    if hi == 0 {
                        return LRESULT(0); // 裸低代理:无效序列丢弃
                    }
                    // 合并码点域恒为 0x10000..=0x10FFFF,标量值必合法
                    char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (code - 0xDC00))
                        .expect("surrogate pair maps to valid scalar")
                }
                _ => {
                    PENDING_SURROGATE.with(|s| s.set(0)); // 防异常序列残留污染下一对
                    match char::from_u32(code) {
                        Some(c) => c,
                        None => return LRESULT(0),
                    }
                }
            };
            // Windows 把退格发成 0x08,终端世界统一 DEL(0x7f)
            let bytes: Vec<u8> = if ch == '\u{8}' {
                vec![0x7f]
            } else {
                ch.to_string().into_bytes() // UTF-8,中文/emoji 原样
            };
            STATE.with(|cell| {
                if let Some(t) = cell.borrow_mut().as_mut() {
                    let _ = t.session.write(&bytes);
                }
            });
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // 编码要读 term 的 DECCKM 模式,索性连同写回共用一次借用
            //(RefCell 内不嵌套第二借用,app_cursor_mode 只借走 &t.term)
            STATE.with(|cell| {
                if let Some(t) = cell.borrow_mut().as_mut()
                    && let Some(bytes) =
                        vkey_bytes(wparam.0 as u32, current_mods(), t.term.app_cursor_mode())
                {
                    let _ = t.session.write(&bytes);
                }
            });
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
                let mut t_guard = cell.borrow_mut();
                let Some(t) = t_guard.as_mut() else {
                    return;
                };
                let cols = ((width as f32 / t.metrics.cell_width).max(1.0)) as u16;
                let rows = ((height as f32 / t.metrics.line_height).max(1.0)) as u16;
                if cols == t.cols && rows == t.rows {
                    return;
                }
                t.cols = cols;
                t.rows = rows;
                t.term.resize(ScreenSize::new(cols as usize, rows as usize));
                t.term.set_cell_metrics(
                    t.metrics.cell_width.round() as u16,
                    t.metrics.line_height.round() as u16,
                );
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
        WM_APP_CONFIG => {
            // 配置热重载(watch 线程去抖后投递);主线程执行,失败保旧
            reload_config(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn vkey_bytes(vk: u32, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
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
    Some(input::encode(key, mods, app_cursor))
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
