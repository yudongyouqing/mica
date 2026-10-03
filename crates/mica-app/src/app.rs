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
use mica_core::ipc::IpcMessage;
use mica_core::keymap::{Action, Keymap, TriggerKey};
use mica_core::pty::{PtyReader, PtySession};
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
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::HICON;
/// ShellExecuteW show cmd(打开链接用)
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW,
    DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, GetWindowRect, LoadCursorW, MSG,
    MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW, SetWindowTextW,
    TranslateMessage, WINDOW_EX_STYLE, WM_CHAR, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_PAINT, WM_SIZE,
    WM_SYSCHAR, WM_SYSCOMMAND, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTCLOSE, HTLEFT, HTMAXBUTTON,
    HTMINBUTTON, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, MB_ICONWARNING, MB_OK, SC_MAXIMIZE,
    SC_MINIMIZE,
};
use windows::core::{HSTRING, PCWSTR, w};

const COLS: u16 = 100;
const ROWS: u16 = 30;

/// 自绘顶栏(T8):右上三颗系统按钮各 46px 宽;resize 拖拽边宽
const CAPTION_BTN_W: i32 = 46;
const RESIZE_BORDER: i32 = 6;

/// WM_APP 用户消息区(0x8000 起):
/// +1 = WM_APP_RENDER(T7,pty 转发线程唤醒渲染);+2 = WM_APP_CONFIG(T5,热重载)
const WM_APP_RENDER: u32 = 0x8000 + 1;
const WM_APP_CONFIG: u32 = 0x8000 + 2;
const WM_APP_IPC: u32 = 0x8000 + 3;
/// WM_APP 基值(quickterm 转发线程投递用裸 WM_APP)
const WM_APP_MSG: u32 = 0x8000;

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
pub(crate) mod ipc;
mod jumplist;
pub(crate) mod quickterm;

thread_local! {
    static GPU: RefCell<Option<WindowGpu>> = const { RefCell::new(None) };
    static TABS: RefCell<Vec<TabState>> = const { RefCell::new(Vec::new()) };
    static ACTIVE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static NEXT_TAB_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
    /// SharedHwnd 的全局副本:NewTab 动作要起新 forwarder,而 hwnd 哨兵
    /// 只在 run() 手里有——建池时存一份进 TLS(与窗口同生命周期)
    static SHARED_HWND: RefCell<Option<SharedHwnd>> = const { RefCell::new(None) };
    /// 新建标签所需的字体/度量种子(load_settings 快照):热重载后建的
    /// 标签跟随当前设置,而不是启动时的
    static TAB_SEED: RefCell<(Vec<String>, f32)> = const { RefCell::new((Vec::new(), 12.0)) };
    /// 当前生效调色板(建新标签用;热重载时更新)
    static CURRENT_PALETTE: RefCell<Palette> = const { RefCell::new(Palette::DEFAULT) };
    /// 非 BMP 字符(emoji、扩展区汉字)以 UTF-16 代理对各发一次 WM_CHAR,
    /// 高代理暂存于此,低代理到达时重组成码点
    static PENDING_SURROGATE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// 窗口级键位表(Settings 合成;热重载时整表替换)
    static KEYMAP: RefCell<Keymap> = RefCell::new(Keymap::wt_default());
    /// IPC 服务端消息队列(主线程 WM_APP_IPC 时 try_recv)
    static IPC_RX: RefCell<Option<std::sync::mpsc::Receiver<IpcMessage>>> = const { RefCell::new(None) };
    /// 2026 同步持帧(D29):h 起置 true,l 或 150ms 超时放帧。
    /// 持帧只跳 draw(数据照 drain+feed,不丢)
    static SYNC_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 光标闪烁相位(true = 亮);开关由配置驱动(settings.cursor_blink)
    static BLINK_PHASE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static CURSOR_BLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 拖选中(WM_LBUTTONDOWN 起、WM_LBUTTONUP 止)
    static MOUSE_DOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 上次双击的 (时刻, x, y):三击 = 同位置 450ms 内的第二次双击
    static LAST_DBLCLK: std::cell::RefCell<Option<(std::time::Instant, i32, i32)>> =
        const { std::cell::RefCell::new(None) };
}

/// 窗口级 GPU 资源(T7 标签架构):surface/renderer 全标签共享,
/// 切标签只换绑"喂给渲染器的数据",不动 GPU。
struct WindowGpu {
    ctx: mica_render::pipeline::GpuContext,
    renderer: Renderer,
    wgpu_surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

/// 单个标签的终端态:GPU 以外的一切(M1 时代的 Terminal 字段原样)。
struct Terminal {
    term: Surface,
    session: PtySession,
    /// pty 输出落点:转发线程(独占 PtyReader 通道)往里追加,WM_APP_RENDER
    /// 在主线程取空。放 Terminal 里让 handler 借一次取齐,关闭标签时一起拆
    pty_buf: Arc<Mutex<Vec<u8>>>,
    router: DwriteRouter,
    metrics: FontMetrics,
    /// 配置合成出的调色板:渲染实例着色与 OSC 4/10/11/12 应答同源(T2 Settings)
    palette: Palette,
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

/// 标签池条目:pane 池 + 布局树 + 池簿记(T7 分屏)。
struct TabState {
    /// OSC 0 标题(strip 显示;shell 未设时用占位)
    title: String,
    /// 后台标签有未渲染数据(D18:feed 照跑,渲染跳过)
    dirty: bool,
    panes: Vec<PaneState>,
    layout: mica_core::layout::Layout,
    /// focused pane 在 panes 里的下标
    focused: usize,
}

/// 单个 pane:Terminal + 全局 pane id(转发线程 WPARAM 用)。
struct PaneState {
    id: u64,
    terminal: Terminal,
}

impl TabState {
    /// 活跃 pane 的 Terminal(全部输入/渲染路径经此)。
    fn active_pane(&mut self) -> Option<&mut Terminal> {
        self.panes.get_mut(self.focused).map(|p| &mut p.terminal)
    }
    fn active_pane_id(&self) -> Option<u64> {
        self.panes.get(self.focused).map(|p| p.id)
    }
    fn pane_by_id(&mut self, id: u64) -> Option<&mut PaneState> {
        self.panes.iter_mut().find(|p| p.id == id)
    }
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
        // 光标闪烁(D20):默认不闪;cursor-blink = true 时窗口创建后开定时器
        CURSOR_BLINK.with(|b| b.set(settings.cursor_blink));
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
        // Mica 材质(T8,Win11 22H2+;DWMSBT_MAINWINDOW=2):老系统静默降级
        let backdrop: i32 = 2;
        let _ = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );

        if CURSOR_BLINK.with(std::cell::Cell::get) {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 1, 500, None);
        }

        // T5/T7:后台线程共用的窗口哨兵,窗口创建后建立(两个线程都要投递)
        let hwnd_slot: SharedHwnd = Arc::new(Mutex::new(Some(hwnd.0 as isize)));
        SHARED_HWND.with(|s| *s.borrow_mut() = Some(Arc::clone(&hwnd_slot)));
        TAB_SEED
            .with(|s| *s.borrow_mut() = (settings.font_families.clone(), settings.font_size_pt));
        CURRENT_PALETTE.with(|p| *p.borrow_mut() = settings.palette);
        let gpu = init_window_gpu(hwnd);
        GPU.with(|g| *g.borrow_mut() = Some(gpu));

        // T5:热重载 watcher 在窗口创建后启动(投递 WM_APP_CONFIG 需要 hwnd);
        // config 文件缺失(全默认启动)则不监听——首次创建配置需重启生效
        let reload = spawn_config_watcher(Arc::clone(&hwnd_slot));
        // M3a:单实例 IPC 服务(管道被占说明已有实例——main 侧已分流,这里
        // 只可能首实例到达;失败降级单开不致命)
        match ipc::serve(Arc::clone(&hwnd_slot)) {
            Ok(rx) => IPC_RX.with(|slot| *slot.borrow_mut() = Some(rx)),
            Err(false) => {} // 建管道失败:已在 ipc::serve 记日志
            Err(true) => unreachable!("main 已分流,GUI 路径不会撞已占管道"),
        }
        // M3a:AUMID + jump list(profile 任务直达)
        jumplist::set_appuser_model_id();
        let profiles: Vec<(String, String)> = mica_core::profile::scan_all()
            .into_iter()
            .map(|p| (p.name, p.command))
            .collect();
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        jumplist::install(&exe, &profiles);
        // T7:首个标签与 GPU 就位(forwarder 由 start_tab 内部起;
        // handle 不 join——退出序由杀 pty 断源自然收尾,detach 可接受)。
        // M3a:CLI/IPC 自启动路径经 MICA_START_PROFILE 指定首标签 profile
        // SAFETY: run() 本就在 unsafe 块内;remove_var 在窗口/线程起前
        let start_profile = std::env::var("MICA_START_PROFILE").ok();
        if start_profile.is_some() {
            std::env::remove_var("MICA_START_PROFILE");
        }
        start_tab_with_profile(hwnd, start_profile.as_deref());
        // GPU 建后补一次 clear_color(默认 palette;create_tab 不碰窗口资源)
        GPU.with(|g| {
            if let Some(gpu) = g.borrow_mut().as_mut() {
                gpu.renderer.set_clear_color(&settings.palette);
            }
        });

        // 首帧显式化:旧轮询循环里第一帧混在首轮 drain 中,事件化后没有输出
        // 就没人画——进循环前先铺一帧(底色+空网格),不等第一条 pty 输出
        draw_frame();
        // M3b:Quick Terminal(热键 + 下拉窗;失败只记日志)
        // QT 失败已内部记日志;Err = QT 禁用(主窗照常,增强件不炸主流程)
        let _ = quickterm::init(hwnd);
        message_loop(reload, hwnd_slot);
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
    tab_id: u64,
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
                                PostMessageW(
                                    Some(hwnd),
                                    WM_APP_RENDER,
                                    WPARAM(tab_id as usize),
                                    LPARAM(0),
                                )
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

    // 客户区像素尺寸不变(不碰窗口尺寸),终端区按新度量重算(减 strip)
    let mut rect = RECT::default();
    GetClientRect(hwnd, &mut rect).expect("GetClientRect");
    let width = rect.right.max(1) as u32;
    let term_h = (rect.bottom.max(1) as u32)
        .saturating_sub(mica_render::frame::STRIP_H as u32)
        .max(1);

    let mut reloaded = false;
    TABS.with(|tabs| {
        // 全部标签全部 pane 一起换(T7):palette/字体是全局语义
        for tab in tabs.borrow_mut().iter_mut() {
            for pane in tab.panes.iter_mut() {
                let t = &mut pane.terminal;
                let Ok(router) = DwriteRouter::new(settings.font_size_pt, &families) else {
                    return;
                };
                let cols = ((width as f32 / metrics.cell_width).max(1.0)) as u16;
                let rows = ((term_h as f32 / metrics.line_height).max(1.0)) as u16;
                t.router = router;
                t.metrics = metrics;
                // 修订号镜像回 u64::MAX:新图集必完成首次上传(与 init 同款契约)
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
                t.palette = settings.palette;
            }
        }
        reloaded = true;
    });
    // 窗口级同步:清屏色、键位表、建标签种子(T7:新标签跟随当前设置)
    GPU.with(|g| {
        if let Some(gpu) = g.borrow_mut().as_mut() {
            gpu.renderer.set_clear_color(&settings.palette);
        }
    });
    KEYMAP.with(|k| *k.borrow_mut() = settings.keymap.clone());
    TAB_SEED.with(|s| *s.borrow_mut() = (settings.font_families.clone(), settings.font_size_pt));
    CURRENT_PALETTE.with(|p| *p.borrow_mut() = settings.palette);
    // draw_frame 自己也要借 TABS/GPU,必须在 with 之外调用
    if reloaded {
        draw_frame();
    }
}

/// 初始化终端状态,返回 (pty 读端, 共享输出缓冲):读端连同缓冲交给转发
/// 线程(T7),缓冲同时存进 Terminal 供 WM_APP_RENDER 取空。
/// 窗口级 GPU 一次建:TAB_SEED 供 NewTab 复用启动设置。
unsafe fn init_window_gpu(hwnd: HWND) -> WindowGpu {
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
    let renderer = Renderer::new(&ctx, config.format);
    WindowGpu {
        ctx,
        renderer,
        wgpu_surface,
        config,
    }
}

/// 建一个新标签(T7):router 按当前 TAB_SEED(启动设置或最近热重载),
/// 网格按客户区减 strip 高换算。返回 (TabState, reader, pty_buf)。
/// 活跃标签的 pane 矩形表(鼠标路由与 draw_frame 同源几何)。
fn active_pane_rects() -> Vec<(u64, mica_core::layout::Rect)> {
    let (aw, ah) = GPU
        .with(|g| {
            g.borrow().as_ref().map(|gpu| {
                (
                    gpu.config.width as f32,
                    gpu.config.height as f32 - mica_render::frame::STRIP_H,
                )
            })
        })
        .unwrap_or((1.0, 1.0));
    let area =
        mica_core::layout::Rect::new(0.0, mica_render::frame::STRIP_H, aw.max(1.0), ah.max(1.0));
    TABS.with(|tabs| {
        tabs.borrow()
            .get(ACTIVE.get())
            .map(|tab| tab.layout.rects(area))
            .unwrap_or_default()
    })
}

/// 焦点 pane 在布局树里的最近邻(方向几何:候选中心与当前中心的方向向量
/// 点积最大且分量同向)——Alt+方向跳焦点(spec §6)。
fn focus_neighbor(dir: input::Key) {
    use mica_core::layout::Rect;
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        let Some(tab) = guard.get_mut(ACTIVE.get()) else {
            return;
        };
        let Some(cur_id) = tab.active_pane_id() else {
            return;
        };
        // 面积与 draw_frame 同源重算
        let (aw, ah) = GPU
            .with(|g| {
                g.borrow().as_ref().map(|gpu| {
                    (
                        gpu.config.width as f32,
                        gpu.config.height as f32 - mica_render::frame::STRIP_H,
                    )
                })
            })
            .unwrap_or((1.0, 1.0));
        let area = Rect::new(0.0, mica_render::frame::STRIP_H, aw.max(1.0), ah.max(1.0));
        let rects = tab.layout.rects(area);
        let Some((_, cur)) = rects.iter().find(|(id, _)| *id == cur_id) else {
            return;
        };
        let cur_c = (cur.x + cur.w / 2.0, cur.y + cur.h / 2.0);
        let (dx, dy) = match dir {
            input::Key::Left => (-1.0, 0.0),
            input::Key::Right => (1.0, 0.0),
            input::Key::Up => (0.0, -1.0),
            input::Key::Down => (0.0, 1.0),
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
                continue; // 反向
            }
            let score = dx * vx + dy * vy;
            if best.is_none_or(|(b, _)| score > b) {
                best = Some((score, i));
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
fn close_pane_action() {
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
            tab.panes.remove(pos); // Drop 杀 pty(既有契约)
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
unsafe fn split_pane(hwnd: HWND, dir: mica_core::layout::SplitDir) {
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
        let (pane, reader) = create_pane(hwnd, "powershell.exe -NoLogo");
        let pane_id = pane.id;
        let pty_buf = Arc::clone(&pane.terminal.pty_buf);
        tab.layout.split(target, dir, pane_id);
        tab.panes.push(pane);
        tab.focused = tab.panes.len() - 1; // 新 pane 焦点(WT 语义)
        for p in tab.panes.iter_mut() {
            p.terminal.force_full = true;
        }
        spawn_render_forwarder(reader, pty_buf, slot, pane_id);
    });
}

unsafe fn create_pane(hwnd: HWND, shell_command: &str) -> (PaneState, PtyReader) {
    let (families, size_pt) = TAB_SEED.with(|s| s.borrow().clone());
    let families: Vec<&str> = if families.is_empty() {
        DEFAULT_FAMILIES.to_vec()
    } else {
        families.iter().map(String::as_str).collect()
    };
    let router = DwriteRouter::new(size_pt, &families).expect("no fonts resolved");
    let metrics = router.metrics();
    let palette = CURRENT_PALETTE.with(|p| *p.borrow());

    let mut rect = RECT::default();
    GetClientRect(hwnd, &mut rect).expect("GetClientRect");
    let width = rect.right.max(1) as u32;
    let height = (rect.bottom.max(1) as u32)
        .saturating_sub(mica_render::frame::STRIP_H as u32)
        .max(1);

    let cols = ((width as f32 / metrics.cell_width).max(1.0)) as u16;
    let rows = ((height as f32 / metrics.line_height).max(1.0)) as u16;
    let mut term = Surface::new(ScreenSize::new(cols as usize, rows as usize));
    // 查询应答(DSR/OSC 尺寸)用格子度量;应答值取整不截断(I3:真度量如
    // 8.53px 时 as u16 会答 8,应答与像素网格漂移;布局换算保持 f32)
    term.set_cell_metrics(
        metrics.cell_width.round() as u16,
        metrics.line_height.round() as u16,
    );
    // 调色板接线:OSC 4/10/11/12 应答与渲染/清屏同源(窗口级 clear_color
    // 由 run/reload 维护,标签只管自己的应答与实例着色)
    term.set_palette(&palette);
    // OSC 52 读向:终端请求剪贴板内容时经 provider 读系统剪贴板
    // (主线程排空事件时调用,Win32 剪贴板无跨线程顾虑)
    term.set_clipboard_provider(std::sync::Arc::new(|| {
        clipboard::get_text().unwrap_or_default()
    }));
    let (session, reader) =
        PtySession::spawn(mica_core::pty::command_from_str(shell_command), cols, rows)
            .expect("spawn shell");
    let pty_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

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
        row_insts: Vec::new(), // 首建即空:build_rows 的长度守恒兜底 → 首帧全量
        force_full: true,      // 首帧显式全量,不依赖哨兵的先后
    };
    let pane_id = NEXT_TAB_ID.with(|n| n.replace(n.get() + 1));
    (
        PaneState {
            id: pane_id,
            terminal,
        },
        reader,
    )
}

/// 建标签并接入渲染链(T7):create_pane 包成 Tab → 入池 → forwarder
/// (WPARAM=pane id)→ 置为活跃。NewTab 动作与启动路径共用。
unsafe fn start_tab(hwnd: HWND) {
    start_tab_with_profile(hwnd, None);
}

/// 带 profile 的建标签(M3a):IPC new-tab / CLI 注入。名字在 scan_all
/// 里查(大小写不敏感);查不到回落默认并记日志。
unsafe fn start_tab_with_profile(hwnd: HWND, profile: Option<&str>) {
    let profiles = mica_core::profile::scan_all();
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
    let shell = picked
        .as_ref()
        .map(|p| p.command.clone())
        .unwrap_or_else(|| "powershell.exe -NoLogo".to_string());
    let title = picked
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "PowerShell".to_string());
    let (pane, reader) = create_pane(hwnd, &shell);
    let pane_id = pane.id;
    let tab = TabState {
        title,
        dirty: false,
        panes: vec![pane],
        layout: mica_core::layout::Layout::leaf(pane_id),
        focused: 0,
    };
    let id = tab.panes[0].id; // WPARAM 按 pane 寻址(T7)
    let pty_buf = Arc::clone(&tab.panes[0].terminal.pty_buf);
    TABS.with(|tabs| tabs.borrow_mut().push(tab));
    ACTIVE.with(|a| a.set(TABS.with(|tabs| tabs.borrow().len() - 1)));
    if let Some(slot) = SHARED_HWND.with(|s| s.borrow().clone()) {
        spawn_render_forwarder(reader, pty_buf, slot, id);
    }
}

/// 主循环:GetMessageW 阻塞等消息,零轮询(T7)。返回 0 = 取到 WM_QUIT,
/// -1 = 错误,其余为有消息——不能按真值判(-1 也非零),先精确判 -1 再判 0。
/// 渲染唤醒(WM_APP_RENDER)、输入、尺寸、热重载全在 wndproc 侧处理。
///
/// Quick Terminal 接线辅助(pub(crate) 供 quickterm 模块)
pub(crate) fn tab_seed() -> (Vec<String>, f32) {
    TAB_SEED.with(|s| s.borrow().clone())
}
pub(crate) fn current_palette() -> Palette {
    CURRENT_PALETTE.with(|p| *p.borrow())
}
pub(crate) fn shared_hwnd_for_quickterm() -> Option<SharedHwnd> {
    SHARED_HWND.with(|s| s.borrow().clone())
}
/// QT 转发线程:与主 forwarder 同款,但 WM_APP(非 WM_APP_RENDER)投给
/// QT 窗口(独立消息域;WPARAM=0xFF00 哨兵)。
pub(crate) fn spawn_qt_forwarder(
    reader: mica_core::pty::PtyReader,
    buffer: Arc<Mutex<Vec<u8>>>,
    hwnd_slot: SharedHwnd,
) {
    std::thread::Builder::new()
        .name("qt-forwarder".into())
        .spawn(move || {
            loop {
                match reader.recv_block() {
                    Some(chunk) => {
                        {
                            let mut pending = buffer.lock().expect("qt buf poisoned");
                            pending.extend_from_slice(&chunk);
                        }
                        let hwnd = *hwnd_slot.lock().expect("hwnd slot poisoned");
                        if let Some(raw) = hwnd {
                            let hwnd = HWND(raw as *mut std::ffi::c_void);
                            // SAFETY: Post 到死句柄无害
                            let _ = unsafe {
                                PostMessageW(Some(hwnd), WM_APP_MSG, WPARAM(0xFF00), LPARAM(0))
                            };
                        }
                    }
                    None => return,
                }
            }
        })
        .expect("spawn qt forwarder");
}

unsafe fn message_loop(reload: Option<ReloadHandle>, hwnd_slot: SharedHwnd) {
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
    // 标签池与 GPU 显式拆:Terminal Drop 杀各自 pty → 读线程 EOF →
    // 转发线程 recv None 退场(detach,不悬挂——哨兵已 None 不再 Post)
    TABS.with(|tabs| tabs.borrow_mut().clear());
    GPU.with(|g| g.borrow_mut().take());
}

fn draw_frame() {
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        // titles 先行收集(不可变借用),再取活跃标签可变借用
        let titles: Vec<(String, bool)> = guard
            .iter()
            .enumerate()
            .map(|(i, tab)| (tab.title.clone(), i == ACTIVE.get()))
            .collect();
        let Some(active) = guard.get_mut(ACTIVE.get()) else {
            return;
        };
        active.dirty = false;

        // ---- T7 分屏渲染:客户区(减 strip)按布局树切给各 pane,逐 pane
        // build(各自己的 damage/router/光标)+ 实例偏移 pane 矩形;焦点
        // pane 画 1px 亮边。strip 文字走焦点 pane 的图集(同帧同缓冲)。
        let area_w = GPU
            .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.width as f32))
            .unwrap_or(0.0);
        let area_h = GPU
            .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.height as f32))
            .unwrap_or(0.0)
            - mica_render::frame::STRIP_H;
        use mica_core::layout::Rect;
        let area = Rect::new(
            0.0,
            mica_render::frame::STRIP_H,
            area_w.max(1.0),
            area_h.max(1.0),
        );
        let rects: Vec<(u64, Rect)> = active.layout.rects(area);
        let focused_pane_id = active.active_pane_id();

        let mut all_instances: Vec<mica_render::frame::CellInstance> = Vec::new();
        // 焦点边框的色与 strip 用同一个 pane 的 router/色板
        let mut head_pane_index = active.focused;
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
            // 布局变了 → 网格按 pane 矩形重算(只在几何不匹配时;rects 每
            // 帧重算,网格重排由 resize/分屏动作显式触发,这里只兜底)
            let want_cols = ((rect.w / t.metrics.cell_width).max(1.0)) as u16;
            let want_rows = ((rect.h / t.metrics.line_height).max(1.0)) as u16;
            if want_cols != t.cols || want_rows != t.rows {
                t.cols = want_cols;
                t.rows = want_rows;
                t.term
                    .resize(ScreenSize::new(want_cols as usize, want_rows as usize));
                let _ = t.session.resize(want_cols, want_rows);
                t.force_full = true;
            }
            // 脏区分路(同 Task 8)
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
            for mut inst in repack(&t.row_insts) {
                inst.pos_uv[0] += rect.x;
                inst.pos_uv[1] += rect.y;
                all_instances.push(inst);
            }
        }
        // 焦点 pane 边框:1px 亮线四条(palette.colors[4] 蓝)
        if let Some(pid) = focused_pane_id
            && let Some((_, r)) = rects.iter().find(|(id, _)| *id == pid)
            && let Some(t) = active
                .panes
                .get_mut(head_pane_index)
                .map(|p| &mut p.terminal)
        {
            let ink = [
                t.palette.colors[4].r,
                t.palette.colors[4].g,
                t.palette.colors[4].b,
            ];
            let b = |x: f32, y: f32, w: f32, h: f32| mica_render::frame::CellInstance {
                pos_uv: [x, y, 0.0, 0.0],
                size_uv: [w, h, 0.0, 0.0],
                fg: [
                    ink[0] as f32 / 255.0,
                    ink[1] as f32 / 255.0,
                    ink[2] as f32 / 255.0,
                    0.0,
                ],
                bg: [
                    ink[0] as f32 / 255.0,
                    ink[1] as f32 / 255.0,
                    ink[2] as f32 / 255.0,
                    0.0,
                ],
            };
            all_instances.extend([
                b(r.x, r.y, r.w, 1.0),
                b(r.x, r.y + r.h - 1.0, r.w, 1.0),
                b(r.x, r.y, 1.0, r.h),
                b(r.x + r.w - 1.0, r.y, 1.0, r.h),
            ]);
            head_pane_index = active.focused; // 抑制未用警告的副带(见下)
        }
        let _ = head_pane_index;

        // strip:焦点 pane 的 router 路由标题(活跃 tab 列表)
        let Some(active) = guard.get_mut(ACTIVE.get()) else {
            return;
        };
        let Some(t) = active.active_pane() else {
            return;
        };
        let mut strip =
            mica_render::frame::strip_quads(&titles, &mut t.router, &t.metrics, &t.palette);
        let revision = t.router.atlas_revision();
        let atlas = t.router.atlas();
        strip.append(&mut all_instances);
        // GPU 窗口级(T7):set_atlas 切到活跃标签的图集再 draw
        GPU.with(|g| {
            let mut gpu_guard = g.borrow_mut();
            let Some(gpu) = gpu_guard.as_mut() else {
                return;
            };
            if t.renderer_atlas_revision != revision {
                gpu.renderer.set_atlas(atlas);
                t.renderer_atlas_revision = revision;
            }
            gpu.renderer.draw(&gpu.wgpu_surface, &gpu.config, &strip);
        });
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
            // kitty 模式下 C0 控制字符已由 WM_KEYDOWN 侧编码成 CSI-u,这里
            // 再发就是双发(Ctrl+字母/退格 0x08/Tab);可打印文本照旧走此处。
            // 代理对(上文已归并的 ch ≥ 0x10000)不受影响
            {
                let kitty = TABS.with(|tabs| {
                    tabs.borrow_mut()
                        .get_mut(ACTIVE.get())
                        .and_then(|tab| tab.active_pane())
                        .is_some_and(|t| t.term.kitty_flags().any())
                });
                if kitty && (ch as u32) < 0x20 {
                    return LRESULT(0);
                }
            }
            // Windows 把退格发成 0x08,终端世界统一 DEL(0x7f)
            let bytes: Vec<u8> = if ch == '\u{8}' {
                vec![0x7f]
            } else {
                ch.to_string().into_bytes() // UTF-8,中文/emoji 原样
            };
            TABS.with(|tabs| {
                if let Some(t) = tabs
                    .borrow_mut()
                    .get_mut(ACTIVE.get())
                    .and_then(|tab| tab.active_pane())
                {
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
                && execute_action(action, hwnd)
            {
                return LRESULT(0);
            }
            if mods.ctrl && !mods.alt && !mods.shift && vk == 'C' as u32 {
                let mut copied = false;
                TABS.with(|tabs| {
                    if let Some(t) = tabs
                        .borrow_mut()
                        .get_mut(ACTIVE.get())
                        .and_then(|tab| tab.active_pane())
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
                    TABS.with(|tabs| {
                        if let Some(t) = tabs
                            .borrow_mut()
                            .get_mut(ACTIVE.get())
                            .and_then(|tab| tab.active_pane())
                        {
                            // bracketed paste(M4a):应用开了 DECSET 2004 就包
                            // ESC[200~/201~ 边界——shell 把多行粘贴当一块处理,
                            // 立即执行。包裹在 CRLF 归一之后(边界不可被改写)
                            let bytes = if t.term.bracketed_paste_active() {
                                let mut wrapped = b"[200~".to_vec();
                                wrapped.extend_from_slice(normalized.as_bytes());
                                wrapped.extend_from_slice(b"[201~");
                                wrapped
                            } else {
                                normalized.into_bytes()
                            };
                            let _ = t.session.write(&bytes);
                        }
                    });
                }
                return LRESULT(0);
            }
            // kitty 分流(M4a/D27):flags 非零时功能键走 CSI-u 编码,
            // None(全关/可打印键)回 legacy;repeat = lparam bit30
            TABS.with(|tabs| {
                if let Some(t) = tabs
                    .borrow_mut()
                    .get_mut(ACTIVE.get())
                    .and_then(|tab| tab.active_pane())
                {
                    let flags = t.term.kitty_flags();
                    if flags.any() {
                        let kind = if lparam.0 & (1 << 30) != 0 {
                            mica_core::protocol::kitty::EventKind::Repeat
                        } else {
                            mica_core::protocol::kitty::EventKind::Press
                        };
                        if let Some(key) = vk_to_key(vk)
                            && let Some(bytes) =
                                mica_core::protocol::kitty::kitty_encode(key, mods, kind, flags)
                        {
                            let _ = t.session.write(&bytes);
                            return;
                        }
                    }
                    if let Some(bytes) = vkey_bytes(vk, mods, t.term.app_cursor_mode()) {
                        let _ = t.session.write(&bytes);
                    }
                }
            });
            LRESULT(0)
        }
        WM_KEYUP => {
            // kitty release 事件(M4a):仅 REPORT_EVENT_TYPES 开启时上报,
            // 其余静默(legacy 无 release 语义)
            TABS.with(|tabs| {
                if let Some(t) = tabs
                    .borrow_mut()
                    .get_mut(ACTIVE.get())
                    .and_then(|tab| tab.active_pane())
                {
                    let flags = t.term.kitty_flags();
                    if flags.contains(mica_core::protocol::kitty::KittyFlags::REPORT_EVENT_TYPES)
                        && let Some(key) = vk_to_key(wparam.0 as u32)
                        && let Some(bytes) = mica_core::protocol::kitty::kitty_encode(
                            key,
                            current_mods(),
                            mica_core::protocol::kitty::EventKind::Release,
                            flags,
                        )
                    {
                        let _ = t.session.write(&bytes);
                    }
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
                    TABS.with(|tabs| {
                        if let Some(t) = tabs
                            .borrow_mut()
                            .get_mut(ACTIVE.get())
                            .and_then(|tab| tab.active_pane())
                        {
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
                TABS.with(|tabs| {
                    if let Some(t) = tabs
                        .borrow_mut()
                        .get_mut(ACTIVE.get())
                        .and_then(|tab| tab.active_pane())
                    {
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
            // strip 区(T7):命中标签则切换、命中 + 则新建,不走选择
            match strip_hit(px, py) {
                Some(Some(idx)) => {
                    switch_tab(idx);
                    return LRESULT(0);
                }
                Some(None) => {
                    unsafe { start_tab(hwnd) };
                    draw_frame();
                    return LRESULT(0);
                }
                None => {}
            }
            // OSC 8 Ctrl+Click(D28):按住 Ctrl 不走选择;命中带链接格直接打开浏览器
            if current_mods().ctrl {
                let rects = active_pane_rects().into_iter().find(|(_, r)| {
                    (px as f32) >= r.x
                        && (px as f32) < r.x + r.w
                        && (py as f32) >= r.y
                        && (py as f32) < r.y + r.h
                });
                if let Some((_, r)) = rects {
                    let (cx, cy) = (px as f32 - r.x, py as f32 - r.y);
                    TABS.with(|tabs| {
                        let mut guard = tabs.borrow_mut();
                        let Some(tab) = guard.get_mut(ACTIVE.get()) else {
                            return;
                        };
                        let Some(t) = tab.active_pane() else { return };
                        let m = &t.metrics;
                        let line = (cy / m.line_height) as i32 + t.term.display_offset() as i32;
                        let col = (cx / m.cell_width) as usize;
                        let link = t.term.grid()[Line(line)][Column(col)].hyperlink();
                        if let Some(link) = link {
                            let uri = windows::core::HSTRING::from(link.uri().to_string());
                            // SAFETY: OS open;失败静默(scheme 异常不弹窗)
                            unsafe {
                                ShellExecuteW(
                                    None,
                                    windows::core::w!("open"),
                                    windows::core::PCWSTR(uri.as_ptr()),
                                    None,
                                    None,
                                    SW_SHOWNORMAL,
                                );
                            }
                        }
                    });
                }
                return LRESULT(0);
            }
            let mods = current_mods();
            // pane 命中(T7):点中的 pane 聚焦,坐标换算到该 pane 原点
            let rects = active_pane_rects();
            let hit = rects.iter().find(|(_, r)| {
                (px as f32) >= r.x
                    && (px as f32) < r.x + r.w
                    && (py as f32) >= r.y
                    && (py as f32) < r.y + r.h
            });
            if let Some((pid, r)) = hit {
                TABS.with(|tabs| {
                    let mut guard = tabs.borrow_mut();
                    let Some(tab) = guard.get_mut(ACTIVE.get()) else {
                        return;
                    };
                    if let Some(pos) = tab.panes.iter().position(|p| p.id == *pid)
                        && pos != tab.focused
                    {
                        tab.focused = pos;
                        for p in tab.panes.iter_mut() {
                            p.terminal.force_full = true;
                        }
                    }
                    if let Some(t) = tab.pane_by_id(*pid).map(|p| &mut p.terminal) {
                        let offset = t.term.display_offset();
                        let (point, side) = px_to_buffer_point(
                            px - r.x as i32,
                            py - r.y as i32,
                            &t.metrics,
                            offset,
                        );
                        if mods.shift && t.term.selection_range().is_some() {
                            t.term.selection_update(point, side);
                        } else {
                            t.term.selection_begin(SelectionType::Simple, point, side);
                        }
                        t.force_full = true;
                    }
                });
            }
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
            // 拖选在焦点 pane 内(坐标相对 pane 原点)
            let rects = active_pane_rects();
            TABS.with(|tabs| {
                let mut guard = tabs.borrow_mut();
                let Some(tab) = guard.get_mut(ACTIVE.get()) else {
                    return;
                };
                let Some(pid) = tab.active_pane_id() else {
                    return;
                };
                if let Some((_, r)) = rects.iter().find(|(id, _)| *id == pid)
                    && let Some(t) = tab.pane_by_id(pid).map(|p| &mut p.terminal)
                {
                    let offset = t.term.display_offset();
                    let (point, side) =
                        px_to_buffer_point(px - r.x as i32, py - r.y as i32, &t.metrics, offset);
                    t.term.selection_update(point, side);
                    t.force_full = true;
                }
            });
            draw_frame();
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            // WT 语义:中键点标签关闭。strip 外中键无语义(不转发)
            let (px, py) = mouse_xy(lparam);
            if let Some(Some(idx)) = strip_hit(px, py) {
                close_tab(idx);
            }
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
            TABS.with(|tabs| {
                if let Some(t) = tabs
                    .borrow_mut()
                    .get_mut(ACTIVE.get())
                    .and_then(|tab| tab.active_pane())
                {
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
            TABS.with(|tabs| {
                if let Some(t) = tabs
                    .borrow_mut()
                    .get_mut(ACTIVE.get())
                    .and_then(|tab| tab.active_pane())
                {
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
            // surface 是整窗的(含 strip);终端网格按客户区减 strip 高
            let term_h = height
                .saturating_sub(mica_render::frame::STRIP_H as u32)
                .max(1);
            TABS.with(|tabs| {
                // 全部标签一起重排(T7):后台标签的 pty/网格也必须跟新几何,
                // 切换时才不重排跳变
                let mut guard = tabs.borrow_mut();
                for tab in guard.iter_mut() {
                    for t in tab.panes.iter_mut().map(|p| &mut p.terminal) {
                        let cols = ((width as f32 / t.metrics.cell_width).max(1.0)) as u16;
                        let rows = ((term_h as f32 / t.metrics.line_height).max(1.0)) as u16;
                        if cols == t.cols && rows == t.rows {
                            continue;
                        }
                        t.cols = cols;
                        t.rows = rows;
                        t.term.resize(ScreenSize::new(cols as usize, rows as usize));
                        t.term.set_cell_metrics(
                            t.metrics.cell_width.round() as u16,
                            t.metrics.line_height.round() as u16,
                        );
                        let _ = t.session.resize(cols, rows);
                        // 行缓存与视口几何脱节:显式全量(Task 8)
                        t.force_full = true;
                        resized = true;
                    }
                }
            });
            GPU.with(|g| {
                if let Some(gpu) = g.borrow_mut().as_mut()
                    && (gpu.config.width != width || gpu.config.height != height)
                {
                    gpu.config.width = width;
                    gpu.config.height = height;
                    gpu.wgpu_surface.configure(&gpu.ctx.device, &gpu.config);
                    resized = true;
                }
            });
            // draw_frame 自己也要借 TABS/GPU,必须在 with 之外调用
            if resized {
                draw_frame();
            }
            LRESULT(0)
        }
        WM_APP_RENDER => {
            // pty 数据到了(转发线程 Post,WPARAM = tab id):取空共享缓冲 →
            // 喂终端 → 查询应答写回(否则 shell 会卡在等应答)→ 标题 → 重绘。
            // 后台标签(D18)只置脏不画;输入回显经 pty 回来走同一唤醒
            let pane_id = wparam.0 as u64;
            let mut drew = false;
            TABS.with(|tabs| {
                let mut guard = tabs.borrow_mut();
                // pane 全局寻址(T7):pane id 跨标签唯一
                let Some(tab_idx) = guard
                    .iter()
                    .position(|tab| tab.panes.iter().any(|p| p.id == pane_id))
                else {
                    return; // pane 已关,残余投递丢弃
                };
                let is_active = tab_idx == ACTIVE.get();
                let Some(t) = guard
                    .get_mut(tab_idx)
                    .and_then(|tab| tab.pane_by_id(pane_id))
                    .map(|p| &mut p.terminal)
                else {
                    return;
                };
                let bytes = std::mem::take(&mut *t.pty_buf.lock().expect("pty buffer poisoned"));
                if bytes.is_empty() {
                    return; // 积压的重复唤醒:缓冲已被上一条取空,免重绘
                }
                t.term.feed(&bytes);
                // 钉在历史区时仍有输出:保守全量(滚动期间通常无输出,量小)
                if t.term.display_offset() > 0 {
                    t.force_full = true;
                }
                // 终端对查询的应答(DSR/OSC)必须写回 pty,否则 shell 会卡在等待
                for reply in t.term.take_pty_writes() {
                    let _ = t.session.write(reply.as_bytes());
                }
                // OSC 52 写向(tmux 复制到本机):转交系统剪贴板
                // (多条取最后一条——同一轮多次 set 是覆盖语义)
                if let Some(text) = t.term.take_clipboard_out().pop() {
                    clipboard::set_text(&text);
                }
                // 2026 同步输出(D29):持帧只跳 draw,数据已照 feed,不丢。
                // 先取状态到局部再写 guard,避开 t(&mut 借用)与 guard 交叉
                // 全部 t 状态先取局部(t 是 guard 的 &mut,不再交叉借用 guard
                let title_opt: Option<String> = t.term.take_title();
                // OSC 133 行属性表(D37):core 持久累积(上限内),app 不再
                // 排空;jump(T2)与 exit 状态点(T3)经 shell_marks() 消费
                let sync_open = t.term.sync_output_active();
                let mut flush = sync_open; // 放帧时刻需要全量重绘
                let was_held = SYNC_HELD.replace(false);
                if sync_open {
                    t.force_full = true;
                }
                if let Some(title) = title_opt {
                    guard[tab_idx].title = title.clone();
                    if is_active {
                        let _ = SetWindowTextW(hwnd, &HSTRING::from(title));
                    }
                }
                if is_active {
                    if sync_open {
                        // 起安全阀(D29):150ms 内 2026-l 不来即强制放帧
                        SYNC_HELD.with(|f| f.set(true));
                        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
                            Some(hwnd),
                            2,
                            150,
                            None,
                        );
                    } else if was_held {
                        // 窗口刚关(2026-l 到达):放一帧全量
                        windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 2).ok();
                        flush = true;
                        drew = true; // l 即刻放帧
                    } else {
                        drew = true;
                    }
                } else {
                    guard[tab_idx].dirty = true;
                }
                let _ = flush;
            });
            // draw_frame 自己也要借 TABS/GPU,必须在 with 之外调用
            if drew {
                draw_frame();
            }
            LRESULT(0)
        }
        WM_TIMER => {
            // id 分流:2 = 2026 同步安全阀(D29);1 = 光标闪烁(D20)
            match wparam.0 {
                2 => {
                    windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 2).ok();
                    if SYNC_HELD.with(std::cell::Cell::take) {
                        SYNC_HELD.with(|f| f.set(false));
                        TABS.with(|tabs| {
                            if let Some(t) = tabs.borrow_mut().get_mut(ACTIVE.get())
                                && let Some(p) = t.active_pane()
                            {
                                p.term.mark_sync_flushed();
                                p.force_full = true;
                            }
                        });
                        draw_frame();
                    }
                }
                _ => {
                    // 光标闪烁(D20):翻相位,只重绘(光标行在 damage 里自然带出;
                    // 无输入时 term 脏区为空,force_full 兜底成本可接受——闪烁期
                    // 本来就是持续重绘)
                    BLINK_PHASE.with(|p| p.set(!p.get()));
                    TABS.with(|tabs| {
                        if let Some(t) = tabs.borrow_mut().get_mut(ACTIVE.get())
                            && let Some(p) = t.active_pane()
                        {
                            p.force_full = true;
                        }
                    });
                    draw_frame();
                }
            }
            LRESULT(0)
        }
        WM_NCCALCSIZE => {
            // 去系统标题栏(T8):整窗=客户区,顶栏由 strip 自绘。
            // wparam=0 时是查询形态,交默认;=1 才是真正的尺寸计算
            if wparam.0 == 1 {
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_NCHITTEST => {
            // lparam = 屏幕坐标;转窗口本地坐标判区域
            let px = (lparam.0 & 0xffff) as u16 as i16 as i32;
            let py = ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            let (cx, cy) = (px - wr.left, py - wr.top);
            let (w, h) = (wr.right - wr.left, wr.bottom - wr.top);
            let strip_h = mica_render::frame::STRIP_H as i32;
            // 边缘 8 向 resize(优先于一切)
            let (near_l, near_r) = (cx < RESIZE_BORDER, cx >= w - RESIZE_BORDER);
            let (near_t, near_b) = (cy < RESIZE_BORDER, cy >= h - RESIZE_BORDER);
            if near_t && near_l {
                return LRESULT(HTTOPLEFT as isize);
            }
            if near_t && near_r {
                return LRESULT(HTTOPRIGHT as isize);
            }
            if near_b && near_l {
                return LRESULT(HTBOTTOMLEFT as isize);
            }
            if near_b && near_r {
                return LRESULT(HTBOTTOMRIGHT as isize);
            }
            if near_l {
                return LRESULT(HTLEFT as isize);
            }
            if near_r {
                return LRESULT(HTRIGHT as isize);
            }
            if near_t {
                return LRESULT(HTTOP as isize);
            }
            if near_b {
                return LRESULT(HTBOTTOM as isize);
            }
            // 顶栏:右上按钮区优先,其余 = 拖拽区
            if cy < strip_h {
                let btn_left = w - CAPTION_BTN_W * 3;
                if cx >= btn_left {
                    if cx < btn_left + CAPTION_BTN_W {
                        return LRESULT(HTMINBUTTON as isize);
                    }
                    if cx < btn_left + CAPTION_BTN_W * 2 {
                        return LRESULT(HTMAXBUTTON as isize);
                    }
                    return LRESULT(HTCLOSE as isize);
                }
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }
        WM_NCLBUTTONDOWN => {
            // 系统按钮(T8):SC 命令驱动最小化/最大化;关闭走销毁路径
            match wparam.0 as u32 {
                HTMINBUTTON => {
                    let _ = SendMessageW(
                        hwnd,
                        WM_SYSCOMMAND,
                        Some(WPARAM(SC_MINIMIZE as usize)),
                        Some(LPARAM(0)),
                    );
                }
                HTMAXBUTTON => {
                    let _ = SendMessageW(
                        hwnd,
                        WM_SYSCOMMAND,
                        Some(WPARAM(SC_MAXIMIZE as usize)),
                        Some(LPARAM(0)),
                    );
                }
                HTCLOSE => {
                    let _ = PostMessageW(Some(hwnd), WM_DESTROY, WPARAM(0), LPARAM(0));
                }
                _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
            }
            LRESULT(0)
        }
        WM_NCLBUTTONDBLCLK => {
            // 双击顶栏 = 最大化/还原(WT 同款)
            if wparam.0 as u32 == HTCAPTION {
                let _ = SendMessageW(
                    hwnd,
                    WM_SYSCOMMAND,
                    Some(WPARAM(SC_MAXIMIZE as usize)),
                    Some(LPARAM(0)),
                );
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_APP_IPC => {
            // IPC 客户端请求(M3a):主线程排空消息队列——new-tab 起 profile
            // 标签,activate 仅前置窗口
            while let Some(msg) =
                IPC_RX.with(|slot| slot.borrow_mut().as_mut().and_then(|rx| rx.try_recv().ok()))
            {
                if msg.op == "new-tab" {
                    let profile = msg.profile.as_deref();
                    start_tab_with_profile(hwnd, profile);
                }
            }
            // activate 语义(两条消息共用):前置既有窗口
            let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd) };
            draw_frame();
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
/// VK → 终端键(kitty 编码用;与 vk_to_trigger 同表源,不经 Keymap)。
/// match 模式不能用 VK 常量表达式(pattern 限制),数值注释留来源。
fn vk_to_key(vk: u32) -> Option<mica_core::input::Key> {
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

/// 切到指定下标的标签:置活跃 + force_full(strip 也要换高亮)+ 重绘。
fn switch_tab(idx: usize) {
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
    draw_frame();
}

/// 关闭指定标签:Drop 杀 pty(既有契约)→ 读线程 EOF → forwarder 自然退。
/// 关的是活跃标签则邻位顶上;池空 → 退出应用。
fn close_tab(idx: usize) {
    let mut quit = false;
    TABS.with(|tabs| {
        let mut guard = tabs.borrow_mut();
        if idx >= guard.len() {
            return;
        }
        guard.remove(idx);
        let len = guard.len();
        if len == 0 {
            quit = true;
            return;
        }
        let active = ACTIVE.get().min(len - 1);
        ACTIVE.with(|a| a.set(active));
        if let Some(tab) = guard.get_mut(active) {
            for p in tab.panes.iter_mut() {
                p.terminal.force_full = true;
            }
        }
    });
    if quit {
        unsafe { PostQuitMessage(0) };
    } else {
        draw_frame();
    }
}

/// strip 命中(T7):Some(Some(idx)) = 标签;Some(None) = "+" 按钮;
/// None = 终端区。布局常量单源在 frame(TAB_W/TAB_PLUS_W)。
fn strip_hit(px: i32, py: i32) -> Option<Option<usize>> {
    if py < 0 || py as f32 >= mica_render::frame::STRIP_H {
        return None; // 不在 strip 区
    }
    let x = px.max(0) as f32;
    let n = TABS.with(|tabs| tabs.borrow().len());
    let idx = (x / mica_render::frame::TAB_W) as usize;
    if idx < n {
        Some(Some(idx))
    } else if x < mica_render::frame::TAB_W * n as f32 + mica_render::frame::TAB_PLUS_W {
        Some(None) // + 按钮
    } else {
        None
    }
}

/// 执行终端外语义动作。返回 true = 已消费(不再走 pty 编码)。
/// 标签类动作待 T7 标签池落地(TODO 占位消费,防误发 pty 序列)。
fn execute_action(action: Action, hwnd: HWND) -> bool {
    let mut need_draw = false;
    let mut new_tab = false;
    let mut close_idx: Option<usize> = None;
    let mut next_idx: Option<usize> = None;
    let mut goto_idx: Option<usize> = None;
    let mut split: Option<mica_core::layout::SplitDir> = None;
    let mut close_pane = false;
    let mut focus_dir: Option<input::Key> = None;
    TABS.with(|cell| {
        if let Some(t) = cell
            .borrow_mut()
            .get_mut(ACTIVE.get())
            .and_then(|tab| tab.active_pane())
        {
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
                // 分屏是 M2b;标签操作这里只标记,段外执行(避免嵌套借 TABS)
                Action::NewTab => new_tab = true,
                Action::CloseTab => close_idx = Some(ACTIVE.get()),
                Action::NextTab => {
                    let n = cell.borrow().len();
                    next_idx = Some((ACTIVE.get() + 1) % n.max(1));
                }
                Action::PrevTab => {
                    let n = cell.borrow().len();
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
    // y 是全客户区坐标:终端内容从 STRIP_H 起(窗口级布局,T7)
    let py = py - mica_render::frame::STRIP_H as i32;
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
            win: false,
        }
    }
}
