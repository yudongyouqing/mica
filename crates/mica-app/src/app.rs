//! The M1 shell: one window, one terminal, ConPTY + PowerShell,
//! DirectWrite-rasterized glyphs (DwriteRouter owns metrics and the atlas).
//!
//! Event-driven (T7): the main thread parks in `GetMessageW` — zero polling.
//! A pty forwarder thread (app-owned) moves output into a shared buffer and
//! posts `WM_APP_RENDER`; the handler drains the buffer, feeds the terminal,
//! writes query replies back to the pty and redraws. Input paths only write
//! to the pty; echo comes back through the same render wake-up.

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
use mica_core::keymap::{Action, Keymap, TriggerKey};
use mica_core::pty::{PtyReader, PtySession, default_shell_command};
use mica_core::surface::{
    Column, Damage, Line, Point, ScreenSize, ScrollCommand, SelectionType, Side, Surface,
};
use mica_render::font::dwrite::DwriteRouter;
use mica_render::font::metrics::FontMetrics;
use mica_render::frame::{RowInst, build_rows, repack};
use mica_render::pipeline::{Renderer, create_context};
use windows::Win32::Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VIRTUAL_KEY, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END,
    VK_HOME, VK_INSERT, VK_LEFT, VK_MENU, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::HICON;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW,
    DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, LoadCursorW, MSG, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassExW, SetWindowTextW, TranslateMessage,
    WINDOW_EX_STYLE, WM_CHAR, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SIZE, WM_SYSCHAR,
    WM_SYSKEYDOWN, WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK};
use windows::core::{HSTRING, PCWSTR, w};

const COLS: u16 = 100;
const ROWS: u16 = 30;

/// WM_APP 用户消息区(0x8000 起):
/// +1 = WM_APP_RENDER(T7,pty 转发线程唤醒渲染);+2 = WM_APP_CONFIG(T5,热重载)
const WM_APP_RENDER: u32 = 0x8000 + 1;
const WM_APP_CONFIG: u32 = 0x8000 + 2;

/// watch/转发线程共持的窗口句柄哨兵,唯一语义是"窗口是否存活":退出序第一
/// 步统一落 None,两个后台线程自此不再向窗口投递消息;Post 到死句柄本就无害,
/// 纪律照守。存裸 isize 而非 HWND——windows-rs 的句柄包着 *mut c_void,不是
/// Send,投递侧再包回 HWND。
type SharedHwnd = Arc<Mutex<Option<isize>>>;

/// 热重载组件打包:退出时按 哨兵置 None(共用,见 SharedHwnd)→ drop watcher
/// → join 线程 拆除(序在 message_loop 的退出分支,先于既有 teardown)。
struct ReloadHandle {
    watcher: notify::RecommendedWatcher,
    thread: std::thread::JoinHandle<()>,
}

mod clipboard;

thread_local! {
    static STATE: RefCell<Option<Terminal>> = const { RefCell::new(None) };
    /// 非 BMP 字符(emoji、扩展区汉字)以 UTF-16 代理对各发一次 WM_CHAR,
    /// 高代理暂存于此,低代理到达时重组成码点
    static PENDING_SURROGATE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// 窗口级键位表(Settings 合成;热重载时整表替换)
    static KEYMAP: RefCell<Keymap> = RefCell::new(Keymap::wt_default());
    /// 拖选中(WM_LBUTTONDOWN 起、WM_LBUTTONUP 止)
    static MOUSE_DOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 上次双击的 (时刻, x, y):三击 = 同位置 450ms 内的第二次双击
    static LAST_DBLCLK: std::cell::RefCell<Option<(std::time::Instant, i32, i32)>> =
        const { std::cell::RefCell::new(None) };
}

struct Terminal {
    term: Surface,
    session: PtySession,
    /// pty 输出落点:转发线程(独占 PtyReader 通道)往里追加,WM_APP_RENDER
    /// 在主线程取空。放 Terminal 里让 handler 借 STATE 一次取齐,退出时随
    /// Terminal 一起拆
    pty_buf: Arc<Mutex<Vec<u8>>>,
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
    /// 行级实例缓存(Task 8):draw_frame 按 damage 增量重建,容量恒等于
    /// 视口行数(结构守恒在 frame::build_rows 内兜底)
    row_insts: Vec<RowInst>,
    /// 结构性失配(resize、热重载换字体/主题)置位:下一帧无视 term 脏区
    /// 强制全量重建。Term::resize 自身会标 full,但那属于上游实现细节,
    /// 几何失配必须显式钉死在本层
    force_full: bool,
}

pub fn run() {
    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None).expect("GetModuleHandleW").0);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // CS_DBLCLKS:双击翻译成 WM_LBUTTONDBLCLK(选择词/行语义的地基)
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
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
        // 窗口级键位表与设置同源(用户 keybind 覆盖 WT 默认)
        KEYMAP.with(|k| *k.borrow_mut() = settings.keymap.clone());
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

        // T5/T7:后台线程共用的窗口哨兵,窗口创建后建立(两个线程都要投递)
        let hwnd_slot: SharedHwnd = Arc::new(Mutex::new(Some(hwnd.0 as isize)));

        // T5:热重载 watcher 在窗口创建后启动(投递 WM_APP_CONFIG 需要 hwnd);
        // config 文件缺失(全默认启动)则不监听——首次创建配置需重启生效
        let reload = spawn_config_watcher(Arc::clone(&hwnd_slot));
        // T7:pty 会话与转发线程(reader 通道由转发线程独占消费,数据落
        // Terminal.pty_buf 共享缓冲)
        let (reader, pty_buf) = init_terminal(hwnd, router, metrics, settings);
        let forwarder = spawn_render_forwarder(reader, pty_buf, Arc::clone(&hwnd_slot));

        // 首帧显式化:旧轮询循环里第一帧混在首轮 drain 中,事件化后没有输出
        // 就没人画——进循环前先铺一帧(底色+空网格),不等第一条 pty 输出
        draw_frame();
        message_loop(reload, hwnd_slot, forwarder);
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
    // PowerShell 5 `Out-File` 等工具写 UTF-8 会带 BOM:不剥则首键(常是
    // theme/font-family)被静默吞掉,配置"半生效"且无任何提示
    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
    match settings::resolve(source) {
        Ok(s) => s,
        Err(err) => {
            warn_box(&format_config_error(&path, &err, "已回退默认设置"));
            Settings::default()
        }
    }
}

/// 错误弹窗文案:解析行错误带行号在前,值语义错误随后(与 ConfigError 同序)。
/// 结尾措辞按调用点参数化:启动路径回退默认,热重载路径保留当前设置。
fn format_config_error(path: &Path, err: &ConfigError, outcome: &str) -> String {
    let mut msg = format!("配置文件 {} 有误,{outcome}:", path.display());
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
/// 哨兵由 run() 统一建立后传入——与转发线程共用"窗口存活"这一个事实。
fn spawn_config_watcher(hwnd_slot: SharedHwnd) -> Option<ReloadHandle> {
    let path = config_path();
    if !path.exists() {
        eprintln!("config watcher: {} 不存在,热重载未启用", path.display());
        return None;
    }
    // notify 的 EventHandler 直接支持 std mpsc Sender<Result<Event>>;
    // watcher 持有发送端,Drop 时后端停止、通道断开,线程随之退出
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    // 创建/监听/起线程三步都按"绝不致命"降级:失败只记日志、热重载关闭,
    // 绝不 expect 崩终端
    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("config watcher: 创建失败:{e};热重载未启用");
            return None;
        }
    };
    if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
        eprintln!(
            "config watcher: 无法监听 {}: {e};热重载未启用",
            path.display()
        );
        return None;
    }
    let thread_slot = Arc::clone(&hwnd_slot);
    let thread = match std::thread::Builder::new()
        .name("config-watcher".into())
        .spawn(move || debounce_loop(path, rx, thread_slot))
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("config watcher: 线程创建失败:{e};热重载未启用");
            return None; // watcher 随之 Drop,后端停止
        }
    };
    Some(ReloadHandle { watcher, thread })
}

/// pty → 渲染转发线程(T7,app 所有;pty.rs 零 GUI 纪律:core 不碰 win32)。
/// 泊在 `PtyReader::recv_block` 上独占消费 reader 通道(单消费者,顺序天然
/// 保序):数据到达先落共享缓冲,再向窗口 Post WM_APP_RENDER。消息队列天然
/// 帧合并:突发输出只多压几条消息,缓冲被第一条取空后,余下的唤醒在
/// handler 里空转一次即过。退出语义:哨兵落 None 后不再 Post;线程本体要
/// 等 pty 关闭(通道断开,recv_block 得 None)才退——所以退出序必须先杀
/// pty 再 join,见 message_loop。
fn spawn_render_forwarder(
    reader: PtyReader,
    buffer: Arc<Mutex<Vec<u8>>>,
    hwnd_slot: SharedHwnd,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("pty-forwarder".into())
        .spawn(move || {
            loop {
                match reader.recv_block() {
                    Some(chunk) => {
                        {
                            // 锁只罩 extend,绝不跨 PostMessageW 持锁
                            let mut pending = buffer.lock().expect("pty buffer poisoned");
                            pending.extend_from_slice(&chunk);
                        }
                        let hwnd = *hwnd_slot.lock().expect("hwnd slot poisoned");
                        if let Some(raw) = hwnd {
                            // 失败(队列满/窗口已死)忽略即可;Post 到死句柄无害
                            let hwnd = HWND(raw as *mut std::ffi::c_void);
                            let _ = unsafe {
                                PostMessageW(Some(hwnd), WM_APP_RENDER, WPARAM(0), LPARAM(0))
                            };
                        }
                    }
                    None => return, // pty 读线程挂断:通道断开,转发使命结束
                }
            }
        })
        .expect("spawn pty forwarder")
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
    // 与启动路径同一 BOM 剥离:热重载不该比首读更挑剔编码
    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
    let settings = match settings::resolve(source) {
        Ok(s) => s,
        Err(err) => {
            warn_box(&format_config_error(&path, &err, "已保留当前设置"));
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
        // 几何/字形全换:行缓存结构性失配,显式全量(Task 8)
        t.force_full = true;
        t.term.set_palette(&settings.palette);
        t.renderer.set_clear_color(&settings.palette);
        t.palette = settings.palette;
        // 键位表整表替换(热重载同源)
        KEYMAP.with(|k| *k.borrow_mut() = settings.keymap.clone());
        reloaded = true;
    });
    // draw_frame 自己也要借 STATE,必须在 with 之外调用
    if reloaded {
        draw_frame();
    }
}

/// 初始化终端状态,返回 (pty 读端, 共享输出缓冲):读端连同缓冲交给转发
/// 线程(T7),缓冲同时存进 Terminal 供 WM_APP_RENDER 取空。
unsafe fn init_terminal(
    hwnd: HWND,
    router: DwriteRouter,
    metrics: FontMetrics,
    settings: Settings,
) -> (PtyReader, Arc<Mutex<Vec<u8>>>) {
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
    let pty_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    STATE.with(|cell| {
        *cell.borrow_mut() = Some(Terminal {
            term,
            session,
            pty_buf: Arc::clone(&pty_buf),
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
            row_insts: Vec::new(), // 首建即空:build_rows 的长度守恒兜底 → 首帧全量
            force_full: true,      // 首帧显式全量,不依赖哨兵的先后
        });
    });
    (reader, pty_buf)
}

/// 主循环:GetMessageW 阻塞等消息,零轮询(T7)。返回 0 = 取到 WM_QUIT,
/// -1 = 错误,其余为有消息——不能按真值判(-1 也非零),先精确判 -1 再判 0。
/// 渲染唤醒(WM_APP_RENDER)、输入、尺寸、热重载全在 wndproc 侧处理。
unsafe fn message_loop(
    reload: Option<ReloadHandle>,
    hwnd_slot: SharedHwnd,
    forwarder: std::thread::JoinHandle<()>,
) {
    let mut msg = MSG::default();
    loop {
        let ret = GetMessageW(&mut msg, None, 0, 0);
        if ret.0 == -1 {
            // MSDN 明言别拿返回值当 bool:-1 是错误,消息内容未定义,只记录
            eprintln!("GetMessageW failed, GetLastError={}", GetLastError().0);
            break;
        }
        if ret.0 == 0 {
            break; // WM_QUIT:走退出序
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    // 退出序 = T5 序的推广(哨兵 → 断源 → join):
    // 1) 哨兵落 None,watch/转发线程自此不再向窗口 Post(残余 Post 到已销毁
    //    窗口只是失败返回,无害);
    // 2) drop watcher 断开 notify 通道,去抖线程随之退出,join 兜底;
    // 3) STATE.take() 显式 Drop Terminal:杀 pty → pty 读线程 EOF 退出 →
    //    通道断开 → 泊在 recv_block 上的转发线程拿到 None 退场;
    // 4) join 转发线程兜底。
    // 计划原文把 join 排在杀 pty 之前,前提是"转发线程至多阻塞在 PostMessageW
    // 上(异步不阻塞,join 安全)";本实现它泊在 recv_block,pty 不断开就
    // 不醒,先杀后 join 才不悬挂(偏差已记 task 报告)。
    *hwnd_slot.lock().expect("hwnd slot poisoned") = None;
    if let Some(reload) = reload {
        drop(reload.watcher);
        let _ = reload.thread.join();
    }
    STATE.with(|cell| cell.borrow_mut().take());
    let _ = forwarder.join();
    // Terminal 的 Drop 在 TLS 存活时显式执行,退出期回调不再摸已销毁的 STATE
}

fn draw_frame() {
    STATE.with(|cell| {
        let mut t_guard = cell.borrow_mut();
        let Some(t) = t_guard.as_mut() else {
            return;
        };
        // 脏区分路(Task 8):结构性失配(resize/热重载)显式 Full,其余交
        // 给 term 脏区——空 Lines 时 build_rows 原样保留行缓存,重建成本只
        // 落在真正变化的行(路由缓存兜住重复字形的光栅化)
        let damage = if std::mem::take(&mut t.force_full) {
            // 结构性重建也必须先消费一次脏区:take_damage 是上游 last_cursor
            // 旋转的唯一触发点,跳过它则下一帧 Partial 拿着过期的“上一光标
            // 位”(真 resize 后是 (0,0),同尺寸热重载后是更早的任意位)——
            // 本帧 Full 画下的反色光标块从此无人重绘,成为永久残影
            let _ = t.term.take_damage();
            Damage::Full
        } else {
            t.term.take_damage()
        };
        // 先 build(路由新字形、改图集)再比修订号:同帧新增字形同帧上传
        let display_offset = t.term.display_offset();
        let selection = t.term.selection_range();
        build_rows(
            &t.term,
            &mut t.router,
            &t.metrics,
            &t.palette,
            selection.as_ref(),
            display_offset,
            &damage,
            &mut t.row_insts,
        );
        let revision = t.router.atlas_revision();
        if t.renderer_atlas_revision != revision {
            t.renderer.set_atlas(t.router.atlas());
            t.renderer_atlas_revision = revision;
        }
        // repack 恢复全局两遍发射序后整缓冲上传(spec:GPU 侧仍整帧提交,
        // 收益在 CPU 侧的行级重建)
        let instances = repack(&t.row_insts);
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
            // 剪贴板组合(T5;T6 Keymap 落地后迁入 Action 表):
            // Ctrl+C 有选区 → 复制并清选区,无选区 → 放行给 WM_CHAR 的
            // (ETX 中断,WT 同款语义);Ctrl+V / Shift+Insert → 粘贴
            let vk = wparam.0 as u32;
            let mods = current_mods();
            // Keymap 终端外语义优先(D15):命中即消费,不再走 pty 编码
            if let Some(trigger_key) = vk_to_trigger(vk)
                && let Some(action) = KEYMAP.with(|k| k.borrow().lookup(mods, trigger_key))
                && execute_action(action)
            {
                return LRESULT(0);
            }
            if mods.ctrl && !mods.alt && !mods.shift && vk == 'C' as u32 {
                let mut copied = false;
                STATE.with(|cell| {
                    if let Some(t) = cell.borrow_mut().as_mut()
                        && let Some(text) = t.term.selection_text()
                    {
                        clipboard::set_text(&text);
                        t.term.selection_clear();
                        t.force_full = true;
                        copied = true;
                    }
                });
                if copied {
                    draw_frame();
                    return LRESULT(0);
                }
            }
            if (mods.ctrl && !mods.alt && !mods.shift && vk == 'V' as u32)
                || (mods.shift && !mods.ctrl && vk == VK_INSERT.0 as u32)
            {
                if let Some(text) = clipboard::get_text() {
                    let normalized = clipboard::normalize_paste(&text);
                    STATE.with(|cell| {
                        if let Some(t) = cell.borrow_mut().as_mut() {
                            let _ = t.session.write(normalized.as_bytes());
                        }
                    });
                }
                return LRESULT(0);
            }
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
        WM_SYSKEYDOWN => {
            // Alt 组合的窗口层路由(M1 已知缺口清账):Alt+方向/编辑键走与
            // WM_KEYDOWN 同一条 encode 路径(alt 位已在 Mods 里,encode 加 ESC
            // 前缀)。我们不认识的系统键(Alt+F4、Alt+Space)必须落回
            // DefWindowProc,吞掉 return 0 会废掉系统行为
            match vkey_bytes(wparam.0 as u32, current_mods(), false) {
                Some(bytes) => {
                    STATE.with(|cell| {
                        if let Some(t) = cell.borrow_mut().as_mut() {
                            let _ = t.session.write(&bytes);
                        }
                    });
                    LRESULT(0)
                }
                None => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
        WM_SYSCHAR => {
            // Alt+可打印字符 = xterm meta 编码(ESC + 字符)。Alt+Space 等
            // 系统助记符落回 DefWindowProc(菜单激活)
            let code = wparam.0 as u32;
            if code == 0x20 || (code < 0x20 && code != 0x0d && code != 0x08) {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            } else if let Some(c) = char::from_u32(code) {
                // 退格 0x08 同 WM_CHAR 语义归一为 DEL
                let mut bytes: Vec<u8> = if c == '\u{8}' {
                    vec![0x7f]
                } else {
                    c.to_string().into_bytes()
                };
                STATE.with(|cell| {
                    if let Some(t) = cell.borrow_mut().as_mut() {
                        // Alt 修饰由消息本身保证:前置 ESC 完成 meta 编码
                        let mut prefixed = vec![0x1b];
                        prefixed.append(&mut bytes);
                        let _ = t.session.write(&prefixed);
                    }
                });
                LRESULT(0)
            } else {
                LRESULT(0)
            }
        }
        WM_LBUTTONDOWN => {
            let (px, py) = mouse_xy(lparam);
            let mods = current_mods();
            STATE.with(|cell| {
                if let Some(t) = cell.borrow_mut().as_mut() {
                    let offset = t.term.display_offset();
                    let (point, side) = px_to_buffer_point(px, py, &t.metrics, offset);
                    if mods.shift && t.term.selection_range().is_some() {
                        t.term.selection_update(point, side);
                    } else {
                        t.term.selection_begin(SelectionType::Simple, point, side);
                    }
                    // 选区跨行覆盖且不入 term 脏区:全量兜底
                    //(拖动 30 行重建成本低,行级选区追踪不值)
                    t.force_full = true;
                }
            });
            MOUSE_DOWN.with(|m| m.set(true));
            let _ = SetCapture(hwnd);
            draw_frame();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if !MOUSE_DOWN.with(std::cell::Cell::get) {
                return LRESULT(0);
            }
            let (px, py) = mouse_xy(lparam);
            STATE.with(|cell| {
                if let Some(t) = cell.borrow_mut().as_mut() {
                    let offset = t.term.display_offset();
                    let (point, side) = px_to_buffer_point(px, py, &t.metrics, offset);
                    t.term.selection_update(point, side);
                    t.force_full = true;
                }
            });
            draw_frame();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            MOUSE_DOWN.with(|m| m.set(false));
            let _ = ReleaseCapture();
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let (px, py) = mouse_xy(lparam);
            // 三击 = 同位置 450ms 内的第二次双击,升格行选择
            let ty = {
                let last = LAST_DBLCLK.with_borrow_mut(|l| l.take());
                let triple = last.as_ref().is_some_and(|(t, lx, ly)| {
                    t.elapsed().as_millis() < 450 && *lx == px && *ly == py
                });
                LAST_DBLCLK.with_borrow_mut(|l| *l = last);
                if triple {
                    SelectionType::Lines
                } else {
                    SelectionType::Semantic
                }
            };
            STATE.with(|cell| {
                if let Some(t) = cell.borrow_mut().as_mut() {
                    let offset = t.term.display_offset();
                    let (point, side) = px_to_buffer_point(px, py, &t.metrics, offset);
                    t.term.selection_begin(ty, point, side);
                    t.force_full = true;
                }
            });
            MOUSE_DOWN.with(|m| m.set(true));
            let _ = SetCapture(hwnd);
            draw_frame();
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // 高位有符号 delta,120/格;WT 惯例 3 行/格(=delta/40)。
            // delta 正 = 滚轮向上 = 看历史(上游 Scroll::Delta 正值增 offset)
            let delta = ((wparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
            let mut scrolled = false;
            STATE.with(|cell| {
                if let Some(t) = cell.borrow_mut().as_mut() {
                    t.term.scroll_display(ScrollCommand::Delta(delta / 40));
                    // 视口几何变了(视口行 → buffer 行的映射整体位移):
                    // 行缓存的"行 i"语义失效,显式全量(与 resize 同性质)
                    t.force_full = true;
                    scrolled = true;
                }
            });
            if scrolled {
                draw_frame();
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // 客户区全由 wgpu 清屏:阻止系统擦背景——resize 时旧内容闪白
            // 的来源就是这擦除(D3D 未准备好前的一帧系统底色)
            LRESULT(1)
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
                // 行缓存与视口几何脱节:显式全量(Task 8)——resize 后 term
                // 脏区可能为空,增量路径会拿旧几何的行缓存绘制
                t.force_full = true;
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
        WM_APP_RENDER => {
            // pty 数据到了(转发线程 Post):取空共享缓冲 → 喂终端 → 查询
            // 应答写回(否则 shell 卡在等应答)→ 标题 → 重绘。输入路径不在
            // 此画:回显经 pty 回来,走的是同一个唤醒(见 WM_CHAR)
            let mut drew = false;
            STATE.with(|cell| {
                let mut t_guard = cell.borrow_mut();
                let Some(t) = t_guard.as_mut() else {
                    return;
                };
                let bytes = std::mem::take(&mut *t.pty_buf.lock().expect("pty buffer poisoned"));
                if bytes.is_empty() {
                    return; // 队列里积压的重复唤醒:缓冲已被上一条取空,免重绘
                }
                t.term.feed(&bytes);
                // 钉在历史区时仍有输出:缓存按屏幕锚更新了,显示的是历史区,
                // 光标/行序关系整体变化,保守全量(滚动期间通常无输出,量小)
                if t.term.display_offset() > 0 {
                    t.force_full = true;
                }
                // 终端对查询的应答(DSR/OSC)必须写回 pty,否则 shell 会卡在等待
                for reply in t.term.take_pty_writes() {
                    let _ = t.session.write(reply.as_bytes());
                }
                if let Some(title) = t.term.take_title() {
                    let _ = SetWindowTextW(hwnd, &HSTRING::from(title));
                }
                drew = true;
            });
            // draw_frame 自己也要借 STATE,必须在 with 之外调用
            if drew {
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

/// VK → Keymap 触发键(字母/数字/Insert/导航;其余键无终端外语义)。
fn vk_to_trigger(vk: u32) -> Option<TriggerKey> {
    match vk {
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

/// 执行终端外语义动作。返回 true = 已消费(不再走 pty 编码)。
/// 标签类动作待 T7 标签池落地(TODO 占位消费,防误发 pty 序列)。
fn execute_action(action: Action) -> bool {
    let mut need_draw = false;
    STATE.with(|cell| {
        if let Some(t) = cell.borrow_mut().as_mut() {
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
                        let normalized = clipboard::normalize_paste(&text);
                        let _ = t.session.write(normalized.as_bytes());
                    }
                }
                Action::ScrollLine(n) => {
                    t.term.scroll_display(ScrollCommand::Delta(n));
                    t.force_full = true;
                    need_draw = true;
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
                // T7 前占位:消费掉但不动作(标签池落地时接实现)
                Action::NewTab
                | Action::CloseTab
                | Action::NextTab
                | Action::PrevTab
                | Action::GotoTab(_)
                | Action::SplitRight
                | Action::SplitDown
                | Action::ClosePane => {}
            }
        }
    });
    if need_draw {
        draw_frame();
    }
    true
}

/// 客户区像素 → buffer 坐标点(视口行 + display_offset)与半格侧别。
/// 拖出上/左边缘夹到 0(捕获期间 WM_MOUSEMOVE 仍投递,坐标可为负)。
fn px_to_buffer_point(
    px: i32,
    py: i32,
    metrics: &FontMetrics,
    display_offset: usize,
) -> (Point, Side) {
    let col_f = (px.max(0) as f32 / metrics.cell_width).max(0.0);
    let col = col_f as usize;
    let side = if (col_f - col as f32) < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    let view_line = (py.max(0) as f32 / metrics.line_height).max(0.0) as usize;
    (
        Point::new(Line(view_line as i32 + display_offset as i32), Column(col)),
        side,
    )
}

/// 鼠标消息 lparam 的有符号客户区坐标。
fn mouse_xy(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xffff) as u16 as i16 as i32,
        ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32,
    )
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
