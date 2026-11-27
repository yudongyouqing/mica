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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mica_core::config::palette::Palette;
use mica_core::config::settings::{DEFAULT_FAMILIES, MouseRightClick, Settings};
use mica_core::input::{Key, Mods};
use mica_core::ipc::{IpcError, IpcRequest, IpcResponse};
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
    GetKeyState, ReleaseCapture, SetCapture, TME_HOVER, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
    TrackMouseEvent, VIRTUAL_KEY, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_HOME, VK_INSERT,
    VK_LEFT, VK_MENU, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_SHIFT, VK_TAB, VK_UP,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::HICON;
/// ShellExecuteW show cmd(打开链接用)
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, GetClientRect, GetMessageW, GetWindowRect, LoadCursorW, MSG, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW, SetWindowTextW,
    TranslateMessage, WINDOW_EX_STYLE, WM_ACTIVATE, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED,
    WM_ERASEBKGND, WM_GETOBJECT, WM_IME_SETCONTEXT, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCMOUSEMOVE, WM_PAINT,
    WM_RBUTTONDOWN, WM_SETTINGCHANGE, WM_SIZE, WM_SYSCHAR, WM_SYSCOMMAND, WM_SYSKEYDOWN,
    WM_THEMECHANGED, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTCLOSE, HTLEFT, HTMAXBUTTON,
    HTMINBUTTON, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, MB_ICONWARNING, MB_OK, SC_MAXIMIZE,
    SC_MINIMIZE, SC_RESTORE,
};
use windows::core::{HSTRING, PCWSTR, w};

/// WM_APP 用户消息区(0x8000 起):
/// +1 = WM_APP_RENDER(T7,pty 转发线程唤醒渲染);+2 = WM_APP_CONFIG(T5,热重载)
const WM_APP_RENDER: u32 = 0x8000 + 1;
const WM_APP_CONFIG: u32 = 0x8000 + 2;
const WM_APP_IPC: u32 = 0x8000 + 3;
const WM_APP_TRANSFER: u32 = 0x8004;
const WM_APP_CLOSE_EMPTY: u32 = 0x8005;
const WM_APP_DETACH_READY: u32 = 0x8006;
/// watch/转发线程共持的窗口句柄哨兵,唯一语义是"窗口是否存活":退出序第一
/// 步统一落 None,两个后台线程自此不再向窗口投递消息;Post 到死句柄本就无害,
/// 纪律照守。存裸 isize 而非 HWND——windows-rs 的句柄包着 *mut c_void,不是
/// Send,投递侧再包回 HWND。
type SharedHwnd = Arc<Mutex<Option<isize>>>;

mod config;
use config::{ReloadHandle, load_settings, reload_config, spawn_config_watcher};

mod clipboard;
pub(crate) mod crash;
mod input;
pub(crate) mod ipc;
mod jumplist;
#[cfg(test)]
mod keyboard_tests;
pub(crate) mod lifecycle;
pub mod orphan; // pub:tests 直入(真进程集成回归,T02)
mod overlay;
pub(crate) mod pty_bridge;
pub(crate) mod quickterm;
mod render;
mod strip;
mod tab_drag;
mod tabs;
mod transfer;
pub mod uia;
mod window;
pub mod window_registry; // pub:tests 直入(真窗口生命周期回归,T04)

use input::*;
use overlay::*;
use render::*;
use strip::*;
pub(crate) use tabs::scaled_pt;
use tabs::*;
pub use window::run;
use window::*;

thread_local! {
    static GPU: RefCell<Option<WindowGpu>> = const { RefCell::new(None) };
    static TABS: RefCell<Vec<TabState>> = const { RefCell::new(Vec::new()) };
    static ACTIVE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };

    /// SharedHwnd 的全局副本:NewTab 动作要起新 forwarder,而 hwnd 哨兵
    /// 只在 run() 手里有——建池时存一份进 TLS(与窗口同生命周期)
    static SHARED_HWND: RefCell<Option<SharedHwnd>> = const { RefCell::new(None) };
    /// 新建标签所需的字体/度量种子(load_settings 快照):热重载后建的
    /// 标签跟随当前设置,而不是启动时的
    static TAB_SEED: RefCell<(Vec<String>, f32)> = const { RefCell::new((Vec::new(), 12.0)) };
    /// Default launch command for future terminals; existing sessions keep theirs.
    static QUICK_TERMINAL_KEY: RefCell<Option<String>> = const { RefCell::new(None) };
    static QUICK_TERMINAL_HEIGHT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
    static CONTENT_PADDING: RefCell<(f32, f32)> = const { RefCell::new((0.0, 0.0)) };
    static SESSION_COMMAND: RefCell<Option<String>> = const { RefCell::new(None) };
    /// 当前生效调色板(建新标签用;热重载时更新)
    static CURRENT_PALETTE: RefCell<Palette> = const { RefCell::new(Palette::DEFAULT) };
    /// 锁定标题(Ghostty `title` 同义,2026-10-07):Some = 窗口/标签恒显
    /// 此值,OSC 0/2 忽略;None = 跟随 shell 标题(exe 路径智能缩短)
    static TITLE_LOCK: RefCell<Option<String>> = const { RefCell::new(None) };
    /// 打字藏鼠标(Ghostty mouse-hide-while-typing 默认 true):键入隐藏
    /// 指针、移动恢复;ShowCursor 计数严格配对(藏一次显一次)
    static MOUSE_HIDE_ON_TYPE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    /// 选中即复制(#58 同批:copy-on-select 配置键,false 时松开鼠标
    /// 不写剪贴板——修剪贴板历史双份)
    static COPY_ON_SELECT: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    /// #80 右键行为(配置驱动;Paste = 粘贴剪贴板)
    static RIGHT_CLICK_PASTE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// #79 修饰键重映射表(encode 前应用;动作判定不受影响)
    static KEY_REMAP: RefCell<Vec<mica_core::keymap::KeyRemap>> =
        const { RefCell::new(Vec::new()) };
    /// #78 透明度档位:true = 透明档(background_opacity),false = 不透明
    static WINDOW_TRANSLUCENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// #77 背景图路径(启动/热重载/设备重建同步;Some(path))
    static BG_IMG_PATH: RefCell<Option<String>> = const { RefCell::new(None) };
    /// #77 背景图混合强度(1.0 未配置时默认 0.9)
    static BG_IMG_ALPHA: std::cell::Cell<f32> = const { std::cell::Cell::new(0.9) };
    /// #88 关闭标签历史(LIFO,容量 8):重开恢复 shell/cwd/标题
    static CLOSED_TABS: RefCell<Vec<ClosedTab>> = const { RefCell::new(Vec::new()) };
    /// #87 重命名模式(键入进查询替换标签标题,Enter 确认/Esc 取消)
    static RENAME_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static RENAME_QUERY: RefCell<String> = const { RefCell::new(String::new()) };
    static RENAME_IDX: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// #78 配置的透明档 alpha(255.0*background_opacity;1.0 = 未配置→0.85 默认)
    static WINDOW_ALPHA_DIM: std::cell::Cell<u8> = const { std::cell::Cell::new(217) };
    /// 鼠标捕获进行中(应用启用 DECSET 1000 族,按下到抬起间全部上报)
    static MOUSE_CAPTURED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// keymap 消费了字母/数字键(ctrl+shift+t 等):TranslateMessage 生成
    /// 的对应 WM_CHAR(0x00-0x1F 控制字节)与 wndproc 返回值无关、照投
    /// 不误——曾致 Ctrl+Shift+T 开新标签的同时给 shell 送去 ^T
    static KEYMAP_ATE_CHAR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CURSOR_HIDDEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// strip 动画相位(2026-10-12"丝滑"工程):focus 0..1 连续(1=聚焦,
    /// 条色/标题墨随相位混合,WM_ACTIVATE 设目标 timer 5 渐近 150ms);
    /// CAPS_* 是活跃标签胶囊横移(切/建/关标签时置初值,120ms ease-out
    /// 衰减到 0——Tahoe NSTabBar 切换滑动的移植;上游源码实证:Ghostty
    /// 自身零动画代码,丝滑全靠系统组件,Mica 用 timer 重画逼近)
    static FOCUS_PHASE: std::cell::Cell<f32> = const { std::cell::Cell::new(1.0) };
    static FOCUS_TARGET: std::cell::Cell<f32> = const { std::cell::Cell::new(1.0) };
    static CAPS_START: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    /// 动画起点(Instant 秒);负值 = 无动画
    static CAPS_T0: std::cell::Cell<f64> = const { std::cell::Cell::new(-1.0) };
    /// 标签数缓存(strip_h 用):TABS 是 RefCell,持 borrow_mut 的路径
    /// (拖选坐标换算/分屏建 pane)再借 TABS 会双借 panic——wndproc 内
    /// panic = 进程崩(2026-10-12 闪退实证)。标签数只在建/关标签时变,
    /// 建/关两处同步维护此 Cell
    static TAB_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// #59 启动计时锚(主线程;timer 7 补做非关键初始化时输出分段)
    static START_T0: std::cell::Cell<Option<std::time::Instant>> = const {
        std::cell::Cell::new(None)
    };
    /// #59 延迟初始化只做一次(副窗线程不触发:timer 只挂在主窗 hwnd)
    static DEFER_DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// strip 外观配置快照(load_settings;热重载时整表替换)——几何/
    /// 颜色/字体/动画全从这里走(2026-10-12 全量可配)
    static STRIP_CFG: RefCell<mica_core::config::settings::StripCfg> =
        RefCell::new(mica_core::config::settings::StripCfg::default());
    /// 标签拖拽换序(2026-10-12 挤压式二版):按下置 Some(idx)+捕获,
    /// 手粘光标(DRAG_X),邻位按目标槽实时让位,松手一次性提交顺序
    static DRAG_TAB: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static DRAG_SLOT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// 被拖标签左缘(px,标签空间含滚动 shift)/ 按下时光标在标签内的偏移
    static DRAG_X: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    static DRAG_GRAB: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    /// 邻位滑移量(按原始标签下标;槽位变化时记跳变差,timer 5 指数
    /// 衰减到 0)——挤压是滑过去不是瞬移(2026-10-12 三版)
    static TAB_DX: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
    /// #57 回看搜索:开态 + 查询串 + 命中表(行 buffer 坐标/列/宽)+ 游标。
    /// 窗口级状态,重绘与 WM_CHAR/WM_KEYDOWN 路由共享
    static SEARCH_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SEARCH_QUERY: RefCell<String> = const { RefCell::new(String::new()) };
    static SEARCH_MATCHES: RefCell<Vec<(i32, usize, usize)>> =
        const { RefCell::new(Vec::new()) };
    static SEARCH_IDX: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// 非 BMP 字符(emoji、扩展区汉字)以 UTF-16 代理对各发一次 WM_CHAR,
    /// 高代理暂存于此,低代理到达时重组成码点
    static PENDING_SURROGATE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// 窗口级键位表(Settings 合成;热重载时整表替换)
    static KEYMAP: RefCell<Keymap> = RefCell::new(Keymap::wt_default());
    /// IPC 服务端消息队列(主线程 WM_APP_IPC 时 try_recv)

    /// 2026 同步持帧(D29):h 起置 true,l 或 150ms 超时放帧。
    /// 持帧只跳 draw(数据照 drain+feed,不丢)
    static SYNC_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 合帧定时器(id 4)已武装:续批不再顺延(见 frame_sched)
    static DRAW_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 上次绘帧时刻:持续输出下距上帧超 16ms 即直绘兜底(WM_TIMER 低
    /// 优先级,渲染消息自唤醒链灌满队列时可能永远排不上)
    static LAST_FRAME: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
    /// 光标闪烁相位(true = 亮);开关由配置驱动(settings.cursor_blink)
    static BLINK_PHASE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static CURSOR_BLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 拖选中(WM_LBUTTONDOWN 起、WM_LBUTTONUP 止)
    static MOUSE_DOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// hover 追踪已武装(TrackMouseEvent 每 hover 会话只需请求一次)
    static HOVER_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 标签溢出滚动(T8):向左滚过的标签数
    static TAB_SCROLL: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    /// per-monitor DPI 缩放(M5b/T4):当前窗口 DPI/96;字号在建 router 时
    /// 乘它(度量数学层零改动),配置里的 pt 值不受污染(换屏可逆)
    static DPI_SCALE: std::cell::Cell<f32> = const { std::cell::Cell::new(1.0) };
    /// D28 hover 气泡:URL + 绘制锚(客户区坐标)。None = 不画
    static HOVER_URL: RefCell<Option<(String, f32, f32)>> =
        const { RefCell::new(None) };
    /// #58 悬停路径 span(buffer 行,列,格宽):即时下划线的数据源;
    /// None = 光标不在路径上(WM_MOUSEMOVE 每次刷新,替代 300ms 气泡)
    static HOVER_PATH: RefCell<Option<PathHover>> =
        const { RefCell::new(None) };
    static UNFOCUSED_SPLIT_OPACITY: std::cell::Cell<f32> =
        std::cell::Cell::new(Settings::default().unfocused_split_opacity);
    /// strip 悬停态(2026-10 UI 对标):标签/加号/caption 按钮的反馈
    /// (const 初始化用字面量——Default::default 不是 const fn)
    static HOVER_STRIP: std::cell::Cell<mica_render::frame::StripHover> = const {
        std::cell::Cell::new(mica_render::frame::StripHover {
            tab: None,
            plus: false,
            arrow_l: false,
            arrow_r: false,
            caption: None,
        })
    };
    /// 上次双击的 (时刻, x, y):三击 = 同位置 450ms 内的第二次双击
    static LAST_DBLCLK: std::cell::RefCell<Option<(std::time::Instant, i32, i32)>> =
        const { std::cell::RefCell::new(None) };
}

/// device lost 标志(M5b/T2):静态而非 TLS——wgpu 的 lost callback 可能
/// 从内部线程触发,主线程在 draw_frame 入口轮询消费。
static GPU_LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// IPC 服务端消息队列(T06):**进程级**而非 TLS——窗口表会把
/// WM_APP_IPC 投给任意存活窗口(主窗先关时副窗接管),任意窗口线程
/// 都必须能排空;请求带单槽回复通道,处理完必须应答。
static IPC_RX: std::sync::Mutex<Option<std::sync::mpsc::Receiver<ipc::IncomingRequest>>> =
    std::sync::Mutex::new(None);

/// 配置 watcher 的进程级持有:在首帧之后、主窗进入消息循环前建立,
/// **不随主窗销毁**——主窗先关、副窗仍在时热重载必须继续工作。
/// 进程退出时随进程回收(watcher Drop 停后端,线程随进程结束)。
static CONFIG_WATCHER: Mutex<Option<ReloadHandle>> = Mutex::new(None);

/// 存活窗口计数(2026-10-12 多窗口):创建 +1 / WM_DESTROY -1;
/// 主窗退出序后按此数等剩余窗口,归零才进程退出
static WINDOW_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 窗口级 GPU 资源(T7 标签架构):surface/renderer 全标签共享,
/// 切标签只换绑"喂给渲染器的数据",不动 GPU。
struct WindowGpu {
    ctx: mica_render::pipeline::GpuContext,
    renderer: Renderer,
    wgpu_surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

pub(crate) use pty_bridge::{PtyBacklog, WriteQueue};

/// 单个标签的终端态:GPU 以外的一切(M1 时代的 Terminal 字段原样)。
pub(crate) struct Terminal<R = DwriteRouter> {
    term: Surface,
    /// 会话(控制面:kill/resize/轮询,全在 UI 线程;写端经
    /// writer_handle 分发给 writer 线程——锁分离,复核 3)
    session: PtySession,
    /// 会话槽位(T08 复核 5:严格 ≤MAX_SESSIONS 的 RAII 占用;
    /// None = pane 无活会话(重启槽位被抢的失败态,可重试)。
    /// 字段顺序保证 permit 在 session 之后 drop。
    session_permit: Option<lifecycle::SessionPermit>,
    /// 有界输出积压(T08:零丢弃背压 + 唤醒合并)
    pty_buf: Arc<pty_bridge::PtyBacklog>,
    /// 单 writer 保序写队列(T08:输入/粘贴/注入/协议回复)
    writeq: Arc<pty_bridge::WriteQueue>,
    /// 输入暂存(T08:统一 FIFO——按键/DSR/IPC/粘贴同序;有界;
    /// 写队列满时的 UI 侧欠账,消息循环分批续灌)
    input_stage: pty_bridge::InputStage,
    /// 当前会话代(与 pty_buf 同源;重启递增,转发线程/写队列据此弃旧)
    session_gen: u64,
    router: R,
    metrics: FontMetrics,
    /// 配置合成出的调色板:渲染实例着色与 OSC 4/10/11/12 应答同源(T2 Settings)
    palette: Palette,
    cols: u16,
    rows: u16,
    /// 行级实例缓存(Task 8):draw_frame 按 damage 增量重建,容量恒等于
    /// 视口行数(结构守恒在 frame::build_rows 内兜底)
    row_insts: Vec<RowInst>,
    /// 重开用(M5b/T3):pane 的 shell 命令串(profile 出处不存,重启等价)
    shell: String,
    /// strip 小号字体路由(2026-10-12):终端字号字形会挤出 × 关闭区
    /// (用户实报),strip 文字改 9.5pt 等宽(族链仍随配置)——与终端
    /// 路由共享图集(单 draw 单纹理约束)。放 Terminal 是渲染借用拓扑
    /// 需要(draw 时与 router 同一借用链)
    strip_router: Option<R>,
    /// 上次消费的图集布局版本(grow 重排即变)——变了必须 force_full
    /// 重建行实例,否则旧 UV 指向重排后的错误条目(一块一块根因)
    last_atlas_ver: u64,
    /// 子进程退出码(M5b/T3):Some = 已退出,冻结尾帧 + strip 提示 + 重开
    exited: Option<u32>,
    /// 结构性失配(resize、热重载换字体/主题)置位:下一帧无视 term 脏区
    /// 强制全量重建。Term::resize 自身会标 full,但那属于上游实现细节,
    /// 几何失配必须显式钉死在本层
    force_full: bool,
}

/// #88 关闭标签的恢复元数据(首个 pane;多 pane 标签重开为单 pane——
/// 布局树不保真,浏览器同款取舍)
struct ClosedTab {
    shell: String,
    cwd: Option<String>,
    title: String,
    color: Option<[u8; 3]>,
}

/// 标签池条目:pane 池 + 布局树 + 池簿记(T7 分屏)。
struct TabState<R = DwriteRouter> {
    /// OSC 0 标题(strip 显示;shell 未设时用占位)
    title: String,
    /// #87 自定义色(标签顶部 2px 色条;None = 无)
    color: Option<[u8; 3]>,
    /// 后台标签有未渲染数据(D18:feed 照跑,渲染跳过)
    dirty: bool,
    panes: Vec<PaneState<R>>,
    layout: mica_core::layout::Layout,
    /// focused pane 在 panes 里的下标
    focused: usize,
}

/// 单个 pane:Terminal + 全局 pane id(转发线程 WPARAM 用)。
struct PaneState<R = DwriteRouter> {
    id: u64,
    terminal: Terminal<R>,
}

impl TabState {
    /// 活跃 pane 的 Terminal(全部输入/渲染路径经此)。
    fn active_pane(&mut self) -> Option<&mut Terminal> {
        self.panes.get_mut(self.focused).map(|p| &mut p.terminal)
    }
    /// 同 [`Self::active_pane`] 的共享版(查询类路径:链接查找等)。
    fn active_pane_ref(&self) -> Option<&Terminal> {
        self.panes.get(self.focused).map(|p| &p.terminal)
    }
    fn active_pane_id(&self) -> Option<u64> {
        self.panes.get(self.focused).map(|p| p.id)
    }
    fn pane_by_id(&mut self, id: u64) -> Option<&mut PaneState> {
        self.panes.iter_mut().find(|p| p.id == id)
    }
}

/// Quick Terminal 接线辅助(pub(crate) 供 quickterm 模块)
pub(crate) fn quick_terminal_key() -> Option<String> {
    QUICK_TERMINAL_KEY.with(|k| k.borrow().clone())
}

pub(crate) fn quick_terminal_height() -> u8 {
    QUICK_TERMINAL_HEIGHT.with(std::cell::Cell::get)
}

pub(crate) fn tab_seed() -> (Vec<String>, f32) {
    TAB_SEED.with(|s| s.borrow().clone())
}

/// Explicit profiles override the configured default; never rewrite a saved
/// terminal command when restarting or tearing off an existing session.
pub(crate) fn launch_command(profile: Option<&mica_core::profile::Profile>) -> String {
    profile.map(|p| p.command.clone()).unwrap_or_else(|| {
        SESSION_COMMAND
            .with(|s| s.borrow().clone())
            .unwrap_or_else(|| "powershell.exe -NoLogo".into())
    })
}

/// 测试面:无窗口路径种字体/调色板 TLS——quickterm 回归测试直接调
/// quickterm::init,其读 TAB_SEED/CURRENT_PALETTE,而 TLS 默认是空
/// families(会让 DwriteRouter 失败 → init Err)。
#[cfg(test)]
pub(crate) fn seed_tls_for_tests(settings: &mica_core::config::settings::Settings) {
    TAB_SEED.with(|s| *s.borrow_mut() = (settings.font_families.clone(), settings.font_size_pt));
    SESSION_COMMAND.with(|s| *s.borrow_mut() = settings.command.clone());
    CONTENT_PADDING
        .with(|p| *p.borrow_mut() = (settings.window_padding_x, settings.window_padding_y));
    QUICK_TERMINAL_HEIGHT.with(|h| h.set(settings.quick_terminal_height));
    QUICK_TERMINAL_KEY.with(|k| *k.borrow_mut() = settings.quick_terminal_key.clone());
    CURRENT_PALETTE.with(|p| *p.borrow_mut() = settings.palette);
}
pub(crate) fn current_palette() -> Palette {
    CURRENT_PALETTE.with(|p| *p.borrow())
}
/// QT 转发线程:与主 forwarder 同款,但 WM_APP(非 WM_APP_RENDER)投给
/// QT 窗口(独立消息域;WPARAM=0xFF00 哨兵)。
pub(crate) fn spawn_qt_forwarder(
    reader: mica_core::pty::PtyReader,
    backlog: Arc<PtyBacklog>,
    _pane_id: u64,
    epoch: u64,
) {
    std::thread::Builder::new()
        .name("qt-forwarder".into())
        .spawn(move || {
            use mica_core::pty::RecvEvent;
            loop {
                match reader.recv_event(Duration::from_millis(200)) {
                    RecvEvent::Data(chunk) => {
                        if backlog.push(epoch, &chunk).is_err() {
                            return;
                        }
                    }
                    RecvEvent::Timeout => {} /* 唤醒重试由 monitor 专职 */
                    RecvEvent::Closed => return,
                }
            }
        })
        .expect("spawn qt forwarder");
}

/// 输入约定(见 mica_core::input 文档):Enter/Tab/Esc/Backspace/普通字符走
/// WM_CHAR;方向/编辑键没有字符事件,走 WM_KEYDOWN 的 VK 映射。
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        tab_drag::PREVIEW => tab_drag::preview(hwnd, wparam.0 != 0, lparam),
        windows::Win32::UI::WindowsAndMessaging::WM_MOVING => {
            tab_drag::native_move(hwnd, false);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_EXITSIZEMOVE => {
            tab_drag::native_move(hwnd, true);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_CAPTURECHANGED if DRAG_TAB.get().is_some() => {
            tab_drag::cancel();
            draw_frame();
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_CAPTURECHANGED => {
            cancel_search_press();
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_KEYDOWN if wparam.0 == 0x1b && DRAG_TAB.get().is_some() => {
            tab_drag::cancel();
            KEYMAP_ATE_CHAR.set(true);
            let _ = ReleaseCapture();
            draw_frame();
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_CONTEXTMENU => {
            let (x, y) = mouse_xy(lparam);
            let mut pt = windows::Win32::Foundation::POINT { x, y };
            if x == -1 && y == -1 {
                let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
            }
            let mut local = pt;
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut local);
            let tab = match strip_hit(local.x, local.y) {
                Some(mica_render::frame::StripTabHit::Tab(i)) => Some(i),
                _ if TAB_COUNT.get() == 1
                    && local.y >= 0
                    && (local.y as f32) < strip_metrics().title_h =>
                {
                    Some(0)
                }
                _ => None,
            };
            if let Some(i) = tab {
                tab_drag::menu(hwnd, i, pt);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_GETOBJECT => {
            if let Some(r) = uia::handle_getobject(hwnd, wparam, lparam) {
                return r;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_IME_SETCONTEXT | WM_IME_STARTCOMPOSITION => {
            // IME 跟随(M5b/T5):激活/开始组合前把组合窗与候选窗定到终端
            // 光标格下缘;默认链(DefWindowProc)负责上下文与结果字符——
            // 提交文本经 WM_IME_CHAR→WM_CHAR 到达,既有 WM_CHAR 路径接管
            position_ime(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ACTIVATE => {
            // 聚焦相位(2026-10-12):失焦洗白不再跳变——设目标后由
            // timer 5 以 150ms 渐近(wparam 低 16 位 0 = WA_INACTIVE)
            let active = (wparam.0 & 0xffff) != 0;
            FOCUS_TARGET.with(|t| t.set(if active { 1.0 } else { 0.0 }));
            let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 5, 15, None);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CHAR => {
            hide_cursor_if_typing();
            // #57 搜索态字符全吞(查询/跳转/关闭),不进 pty
            if search_char((wparam.0 as u32).try_into().unwrap_or('\0')) {
                return LRESULT(0);
            }
            // #87 重命名态字符全吞(查询进新名,不进 pty)
            if rename_char((wparam.0 as u32).try_into().unwrap_or('\0')) {
                return LRESULT(0);
            }
            // keymap 已消费该键(如 Ctrl+Shift+T):TranslateMessage 的控制
            // 字节伴生投递吞掉;任何 WM_CHAR 到来都清位(正常打字即复位)
            if KEYMAP_ATE_CHAR.with(std::cell::Cell::take) && (wparam.0 as u32) < 0x20 {
                return LRESULT(0);
            }
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
            // Tab (plain or modified) is handled on WM_KEYDOWN. Its companion
            // WM_CHAR contains only U+0009 and loses modifier information.
            if ch == '\t' {
                return LRESULT(0);
            }
            // kitty disambiguate 模式下仅 Enter/Backspace/Tab/Esc 已在 WM_KEYDOWN 侧
            // 编码成 CSI-u,其余 C0(如 Ctrl+C=0x03、Ctrl+D=0x04)未被 WM_KEYDOWN 消费,
            // 必须放行给 PTY 否则中断与控制快捷键失效
            {
                let kitty_disambig = TABS.with(|tabs| {
                    tabs.borrow_mut()
                        .get_mut(ACTIVE.get())
                        .and_then(|tab| tab.active_pane())
                        .is_some_and(|t| t.term.kitty_flags().disambiguate())
                });
                if kitty_disambig && matches!(ch, '\r' | '\u{8}' | '\t' | '\x1b') {
                    return LRESULT(0);
                }
            }
            // Windows 把退格发成 0x08,终端世界统一 DEL(0x7f)
            if std::env::var_os("MICA_DEBUG_KEYS").is_some() {
                let kitty = TABS.with(|tabs| {
                    tabs.borrow_mut()
                        .get_mut(ACTIVE.get())
                        .and_then(|tab| tab.active_pane())
                        .is_some_and(|t| t.term.kitty_flags().any())
                });
                eprintln!("[key] WM_CHAR ch={ch:?} code={code:#x} kitty={kitty}");
            }
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
                    let _ = stage_input(t, &bytes);
                }
            });
            LRESULT(0)
        }
        WM_KEYDOWN => {
            hide_cursor_if_typing();
            // #57 搜索态 Up/Down 步进命中,其余键放行(keymap 不会命中,
            // ctrl+shift+f 的 toggle 仍可关——search_char 不碰它)
            if search_key(wparam.0 as u32) {
                return LRESULT(0);
            }
            // 剪贴板组合(T5;T6 Keymap 落地后迁入 Action 表):
            // Ctrl+C 有选区 → 复制并清选区,无选区 → 放行给 WM_CHAR 的
            // 0x03(ETX 中断,WT 同款语义);Ctrl+V / Shift+Insert → 粘贴
            let vk = wparam.0 as u32;
            let mods = current_mods();
            // Keymap 终端外语义优先(D15):命中即消费,不再走 pty 编码。
            // 字母/数字键(VK 0x30..=0x5A)会被 TranslateMessage 生成对应
            // WM_CHAR 控制字节(独立投递,拦不住)——置位让 WM_CHAR 吞
            if let Some(trigger_key) = vk_to_trigger(vk)
                && let Some(action) = KEYMAP.with(|k| k.borrow().lookup(mods, trigger_key))
                && execute_action(action, hwnd)
            {
                if (0x30..=0x5A).contains(&vk) {
                    KEYMAP_ATE_CHAR.set(true);
                }
                return LRESULT(0);
            }
            // The search/rename editor owned Tab's character event before the
            // keydown routing fix; keep it from reaching the terminal beneath.
            if vk == VK_TAB.0 as u32 && (SEARCH_OPEN.get() || RENAME_OPEN.get()) {
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
                    // 复制即消费:吞伴生 WM_CHAR(0x03 ETX)——否则复制完
                    // 还给 shell 送中断(WT 语义:选中 Ctrl+C 只复制)
                    KEYMAP_ATE_CHAR.with(|c| c.set(true));
                    draw_frame();
                    return LRESULT(0);
                }
            }
            if contextual_paste(mods, vk) {
                // 吞伴生 WM_CHAR(0x16):不置位则 SYN 泄给 pty,PSReadLine
                // 把它当第二次 Ctrl+V 粘贴 → 文本双份(用户实报;与当年
                // Ctrl+Shift+T 送 ^T 同类,keymap 分支修过硬编码漏了)
                KEYMAP_ATE_CHAR.with(|c| c.set(true));
                if let Some(text) = clipboard::get_text() {
                    TABS.with(|tabs| {
                        if let Some(t) = tabs
                            .borrow_mut()
                            .get_mut(ACTIVE.get())
                            .and_then(|tab| tab.active_pane())
                        {
                            paste_text(t, &text);
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
                            && let Some(bytes) = mica_core::protocol::kitty::kitty_encode(
                                key,
                                remap_mods(mods),
                                kind,
                                flags,
                            )
                        {
                            let _ = stage_input(t, &bytes);
                            return;
                        }
                    }
                    if let Some(bytes) = vkey_bytes(vk, mods, t.term.app_cursor_mode()) {
                        let _ = stage_input(t, &bytes);
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
                        let _ = stage_input(t, &bytes);
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
                            let _ = stage_input(t, &bytes);
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
                        let _ = stage_input(t, &prefixed);
                    }
                });
                LRESULT(0)
            } else {
                LRESULT(0)
            }
        }
        WM_LBUTTONDOWN => {
            let (px, py) = mouse_xy(lparam);
            if search_press(hwnd, px, py) {
                return LRESULT(0);
            }
            // strip 区(T7):命中标签则切换、+ 新建、箭头步进滚动(#39)
            match strip_hit(px, py) {
                Some(mica_render::frame::StripTabHit::Tab(idx)) => {
                    // 悬停态即刻同步到按下位(点击后旧高亮不再闪别的标签)
                    HOVER_STRIP.with(|s| s.set(strip_hover_at(px, py)));
                    // × 关闭区(与渲染同几何,均分等宽):标签尾部 close_w;
                    // 单标签无 ×(整窗关闭走 caption),退回切换
                    let sm = strip_metrics();
                    let n = TABS.with(|t| t.borrow().len());
                    let area_w = GPU
                        .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.width as f32))
                        .unwrap_or(0.0);
                    let w = mica_render::frame::tab_w_now(n, area_w, &sm);
                    let shift = TAB_SCROLL.with(std::cell::Cell::get) as f32 * sm.tab_min_w;
                    let local = px as f32 + shift
                        - mica_render::frame::tabs_x0(&sm)
                        - idx as f32 * (w + sm.tab_gap);
                    if local >= 0.0 && local < sm.tab_close_w {
                        close_tab(idx);
                        return LRESULT(0);
                    }
                    switch_tab(idx);
                    // 按下即捕获,挤压式拖拽换序(× 区不进);手粘光标
                    {
                        tab_drag::begin(idx);
                        DRAG_SLOT.with(|s| s.set(idx));
                        let span = w + sm.tab_gap;
                        let local = px.max(0) as f32 + shift;
                        DRAG_GRAB.with(|g| g.set((local - idx as f32 * span).clamp(0.0, w)));
                        DRAG_X.with(|x| x.set(idx as f32 * span));
                        CAPS_T0.with(|t| t.set(-1.0)); // 切换滑移让位给手粘光标
                        let _ = SetCapture(hwnd);
                    }
                    return LRESULT(0);
                }
                Some(mica_render::frame::StripTabHit::Plus) => {
                    unsafe { start_tab(hwnd) };
                    draw_frame();
                    return LRESULT(0);
                }
                Some(hit @ mica_render::frame::StripTabHit::ArrowLeft)
                | Some(hit @ mica_render::frame::StripTabHit::ArrowRight) => {
                    // TAB_SCROLL 增 = 标签左移看右侧;左箭头减(看左侧)、右箭头增(看右侧)
                    let dir = if matches!(hit, mica_render::frame::StripTabHit::ArrowLeft) {
                        -1
                    } else {
                        1
                    };
                    if tab_scroll_adjust(dir) {
                        draw_frame();
                    }
                    return LRESULT(0);
                }
                None => {}
            }
            // 应用捕获鼠标(yazi/TUI 族,DECSET 1000 族):上报优先于
            // 本地选择;捕获按下置位,UP/MOVE 走上报分支
            if forward_mouse(px, py, 0, true, false) {
                MOUSE_DOWN.with(|m| m.set(true));
                MOUSE_CAPTURED.with(|c| c.set(true));
                return LRESULT(0);
            }
            // OSC 8 Ctrl+Click(D28):按住 Ctrl 不走选择;命中带链接格直接打开浏览器
            if current_mods().ctrl {
                if let Some(uri) = link_at(px, py) {
                    let uri = windows::core::HSTRING::from(uri);
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
                            px as f32 - r.x,
                            py as f32 - r.y,
                            &t.metrics,
                            offset,
                        );
                        if mods.shift && t.term.selection_range().is_some() {
                            t.term.selection_update(point, side);
                        } else {
                            // Windows 三击序:DOWN → UP → DBLCLK → UP → DOWN(第 3 下在此)
                            let triple = LAST_DBLCLK.with_borrow_mut(|l| {
                                let hit = l.as_ref().is_some_and(|(t, lx, ly)| {
                                    t.elapsed().as_millis() < 450
                                        && (px - *lx).abs() <= 4
                                        && (py - *ly).abs() <= 4
                                });
                                if hit {
                                    *l = None;
                                }
                                hit
                            });
                            let ty = if triple {
                                SelectionType::Lines
                            } else {
                                SelectionType::Simple
                            };
                            t.term.selection_begin(ty, point, side);
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
            // 打字藏鼠标的恢复(Ghostty mouse-hide-while-typing):一动即显
            if CURSOR_HIDDEN.take() {
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::ShowCursor(true);
                }
            }
            // 标签拖拽(挤压式):手粘光标,目标槽随中心跨位变化,邻位
            // 让位由渲染层按槽位序实时完成;顺序只在松手时提交
            if let Some(di) = DRAG_TAB.with(std::cell::Cell::get) {
                let px = (lparam.0 & 0xffff) as u16 as i16 as i32;
                let py = ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
                if !tab_drag::owns(di) {
                    tab_drag::cancel();
                    let _ = ReleaseCapture();
                    draw_frame();
                    return LRESULT(0);
                }
                if tab_drag::track(hwnd, px, py) {
                    return LRESULT(0);
                }
                let sm = strip_metrics();
                let area_w = GPU
                    .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.width as f32))
                    .unwrap_or(0.0);
                let n = TAB_COUNT.with(std::cell::Cell::get);
                if n < 2 {
                    return LRESULT(0);
                }
                let w = mica_render::frame::tab_w_now(n, area_w, &sm);
                let shift = TAB_SCROLL.with(std::cell::Cell::get) as f32 * sm.tab_min_w;
                let local = px.max(0) as f32 + shift;
                let x0 = mica_render::frame::tabs_x0(&sm);
                let right = mica_render::frame::tabs_right(area_w, &sm);
                let span = w + sm.tab_gap;
                let grab = DRAG_GRAB.with(std::cell::Cell::get);
                let left = (local - grab).clamp(x0, (right - w).max(x0));
                // 目标槽 = 被拖标签**中心**所在槽(跨过邻位半程即让位)
                let slot =
                    (((left - x0 + w / 2.0) / span).floor() as i64).clamp(0, n as i64 - 1) as usize;
                let old_slot = DRAG_SLOT.with(std::cell::Cell::get);
                if slot != old_slot {
                    // 邻位滑移:从各自当前渲染位滑向新槽位(记跳变差,
                    // timer 5 衰减到 0)——滑过去,不瞬移
                    let order_old = tab_order(n, di, old_slot);
                    let order_new = tab_order(n, di, slot);
                    TAB_DX.with(|d| {
                        let mut d = d.borrow_mut();
                        d.resize(n, 0.0);
                        for i in 0..n {
                            if i == di {
                                continue;
                            }
                            let oo = order_old.iter().position(|&j| j == i).unwrap_or(i);
                            let on = order_new.iter().position(|&j| j == i).unwrap_or(i);
                            let cur = x0 + oo as f32 * span + d.get(i).copied().unwrap_or(0.0);
                            let tgt = x0 + on as f32 * span;
                            d[i] = cur - tgt;
                        }
                    });
                    ensure_anim_timer();
                }
                let moved = left != DRAG_X.with(std::cell::Cell::get) || slot != old_slot;
                DRAG_X.with(|d| d.set(left));
                DRAG_SLOT.with(|s| s.set(slot));
                if moved {
                    draw_frame();
                }
                let _ = di;
                return LRESULT(0);
            }
            if !MOUSE_DOWN.with(std::cell::Cell::get) {
                // D28 hover:非拖选时武装悬停追踪(每会话一次,HOVER/LEAVE 都会重置)
                if !HOVER_ARMED.with(|a| a.replace(true)) {
                    arm_hover(hwnd);
                }
                // strip 悬停实时跟随(2026-10-12):悬停高亮曾只在 300ms
                // 停留事件里更新——扫过别的标签后旧高亮挂着,点标签时
                // 那个无关标签"亮一下"再灭(用户实测)。每 MOVE 刷新即除
                let (hx, hy) = mouse_xy(lparam);
                let hover = strip_hover_at(hx, hy);
                let strip_changed = HOVER_STRIP.with(|s| {
                    let old = s.get();
                    if old != hover {
                        s.set(hover);
                        true
                    } else {
                        false
                    }
                });
                // #58 路径悬停 span 实时算(单行扫描,轻):即时下划线;
                // 路径上抑制气泡(旧 300ms 气泡循环 = 用户实报鼠标区闪烁)
                let path_changed = update_hover_path(hx, hy);
                if strip_changed || path_changed {
                    draw_frame();
                }
                return LRESULT(0);
            }
            let (px, py) = mouse_xy(lparam);
            // 应用捕获:拖动全程上报(32+0 = 左键 motion)
            if MOUSE_CAPTURED.with(std::cell::Cell::get) {
                let _ = forward_mouse(px, py, 32, true, true);
                return LRESULT(0);
            }
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
                        px_to_buffer_point(px as f32 - r.x, py as f32 - r.y, &t.metrics, offset);
                    t.term.selection_update(point, side);
                    t.force_full = true;
                }
            });
            draw_frame();
            LRESULT(0)
        }
        WM_MOUSEHOVER => {
            // D28:持续 hover 事件流(重武装);悬停格带链接则显气泡
            HOVER_ARMED.with(|a| a.set(false));
            arm_hover(hwnd);
            let (px, py) = mouse_xy(lparam);
            let path_changed = update_hover_path(px, py);
            // #58:路径上用即时下划线(WM_MOUSEMOVE 算的 HOVER_PATH),
            // 气泡退役——300ms 循环 + 锚点跟随是鼠标区闪烁的来源
            let url = if HOVER_PATH.with(|h| h.borrow().is_some()) {
                None
            } else {
                link_at(px, py)
            };
            let changed = HOVER_URL.with(|h| {
                let mut h = h.borrow_mut();
                let same = match (&*h, &url) {
                    (Some((u, _, _)), Some(v)) => u == v,
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    *h = url.map(|u| (u, px as f32 + 14.0, py as f32 + 22.0));
                }
                !same
            });
            // strip 悬停(2026-10):标签/加号/标题区命中变化 → 反馈重绘
            let hover = strip_hover_at(px, py);
            let hover_changed = HOVER_STRIP.with(|s| {
                let old = s.get();
                if old != hover {
                    s.set(hover);
                    true
                } else {
                    false
                }
            });
            if changed || hover_changed || path_changed {
                draw_frame();
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            // TrackMouseEvent TME_LEAVE 投递(675,本地声明——windows-rs 未收录)
            HOVER_ARMED.with(|a| a.set(false));
            let had = HOVER_URL.with(|h| h.borrow_mut().take().is_some());
            let path_had = HOVER_PATH.with(|h| h.borrow_mut().take().is_some());
            let strip_had = HOVER_STRIP.with(|s| {
                let had = s.get() != mica_render::frame::StripHover::default();
                s.set(mica_render::frame::StripHover::default());
                had
            });
            if had || path_had || strip_had {
                draw_frame();
            }
            LRESULT(0)
        }
        WM_NCMOUSEMOVE => {
            // caption 按钮区(NCHITTEST 判非客户区)的悬停流:lparam 屏幕
            // 坐标 → 客户系后同源命中;变化即重绘并武装 NC leave
            let mut pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
            // SAFETY: 坐标换算,输出写回本侧 POINT
            unsafe {
                pt.x = (lparam.0 & 0xffff) as u16 as i16 as i32;
                pt.y = ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
                let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
            }
            let hover = strip_hover_at(pt.x, pt.y);
            let changed = HOVER_STRIP.with(|s| {
                let old = s.get();
                if old != hover {
                    s.set(hover);
                    true
                } else {
                    false
                }
            });
            if changed {
                arm_nc_leave(hwnd);
                draw_frame();
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_NCMOUSELEAVE => {
            // TME_NONCLIENT 投递(674 = 0x02A2,本地声明——windows-rs 未收录)
            let had = HOVER_STRIP.with(|s| {
                let had = s.get() != mica_render::frame::StripHover::default();
                s.set(mica_render::frame::StripHover::default());
                had
            });
            if had {
                draw_frame();
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            // #80 mouse-right-click = paste:右键粘贴(选区存在先复制);
            // 应用捕获鼠标时上报优先(无论 RIGHT_CLICK_PASTE 开关);strip 区不劫持
            let (px, py) = mouse_xy(lparam);
            if strip_hit(px, py).is_some()
                || forward_mouse(px, py, 2, true, false)
                || !RIGHT_CLICK_PASTE.with(std::cell::Cell::get)
            {
                return LRESULT(0);
            }
            TABS.with(|tabs| {
                if let Some(t) = tabs
                    .borrow_mut()
                    .get_mut(ACTIVE.get())
                    .and_then(|tab| tab.active_pane())
                {
                    // 选区存在:先复制再读剪贴板,保证粘入的正是刚选中的文本
                    if let Some(sel) = t.term.selection_text()
                        && !sel.is_empty()
                    {
                        clipboard::set_text(&sel);
                    }
                    if let Some(text) = clipboard::get_text() {
                        paste_text(t, &text);
                    }
                }
            });
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            // WT 语义:中键点标签关闭。strip 外:应用捕获则上报中键
            let (px, py) = mouse_xy(lparam);
            match strip_hit(px, py) {
                Some(mica_render::frame::StripTabHit::Tab(idx)) => close_tab(idx),
                Some(_) => {}
                None => {
                    let _ = forward_mouse(px, py, 1, true, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (px, py) = mouse_xy(lparam);
            if search_release(px, py) {
                return LRESULT(0);
            }
            MOUSE_DOWN.with(|m| m.set(false));
            // 拖拽换序提交:被拖标签从原位拔出插入目标槽,活跃跟过去
            if let Some(di) = DRAG_TAB.with(std::cell::Cell::take) {
                let (x, y) = mouse_xy(lparam);
                let external = !tab_drag::owns(di) || tab_drag::drop_external(hwnd, di, x, y);
                if external {
                    tab_drag::cancel();
                    let _ = ReleaseCapture();
                    draw_frame();
                    return LRESULT(0);
                }
                let slot = DRAG_SLOT.with(std::cell::Cell::get);
                if slot != di {
                    TABS.with(|tabs| {
                        let mut guard = tabs.borrow_mut();
                        if di < guard.len() {
                            let t = guard.remove(di);
                            let at = slot.min(guard.len());
                            guard.insert(at, t);
                            ACTIVE.with(|a| a.set(at));
                        }
                    });
                }
                TAB_DX.with(|d| d.borrow_mut().clear()); // 各标签已在视觉终位
                draw_frame();
            }
            let _ = ReleaseCapture();
            if MOUSE_CAPTURED.with(std::cell::Cell::take) {
                let (px, py) = mouse_xy(lparam);
                let _ = forward_mouse(px, py, 0, false, false);
                return LRESULT(0);
            }
            // copy-on-select(copy-on-select 配置键,默认 true):选区完成
            // 即入剪贴板;空选区(单击清除)不覆盖;false 时只走显式 Ctrl+C
            TABS.with(|tabs| {
                let mut guard = tabs.borrow_mut();
                if let Some(t) = guard
                    .get_mut(ACTIVE.get())
                    .and_then(|tab| tab.active_pane())
                    && let Some(text) = t.term.selection_text()
                    && !text.is_empty()
                    && COPY_ON_SELECT.with(std::cell::Cell::get)
                {
                    clipboard::set_text(&text);
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let (px, py) = mouse_xy(lparam);
            if search_press(hwnd, px, py) {
                return LRESULT(0);
            }
            let (px, py) = mouse_xy(lparam);
            // 双击记位(供随后 450ms 内第三下 WM_LBUTTONDOWN 升格行选择);
            // 连续第二次双击也兼容升格行选择
            let ty = LAST_DBLCLK.with_borrow_mut(|l| {
                let triple = l.as_ref().is_some_and(|(t, lx, ly)| {
                    t.elapsed().as_millis() < 450 && (px - *lx).abs() <= 4 && (py - *ly).abs() <= 4
                });
                if triple {
                    *l = None;
                    SelectionType::Lines
                } else {
                    *l = Some((std::time::Instant::now(), px, py));
                    SelectionType::Semantic
                }
            });
            let rects = active_pane_rects();
            if let Some((pane_id, rect)) = rects.into_iter().find(|(_, r)| {
                px as f32 >= r.x
                    && (px as f32) < r.x + r.w
                    && py as f32 >= r.y
                    && (py as f32) < r.y + r.h
            }) {
                TABS.with(|tabs| {
                    let mut tabs = tabs.borrow_mut();
                    if let Some(tab) = tabs.get_mut(ACTIVE.get())
                        && let Some(index) = tab.panes.iter().position(|p| p.id == pane_id)
                    {
                        tab.focused = index;
                        let t = &mut tab.panes[index].terminal;
                        let (point, side) = px_to_buffer_point(
                            px as f32 - rect.x,
                            py as f32 - rect.y,
                            &t.metrics,
                            t.term.display_offset(),
                        );
                        t.term.selection_begin(ty, point, side);
                        t.force_full = true;
                    }
                });
            }
            MOUSE_DOWN.with(|m| m.set(true));
            let _ = SetCapture(hwnd);
            draw_frame();
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            HOVER_PATH.with(|h| h.borrow_mut().take());
            HOVER_URL.with(|h| h.borrow_mut().take());
            // 高位有符号 delta,120/格;WT 惯例 3 行/格(=delta/40)。
            // delta 正 = 滚轮向上 = 看历史(上游 Scroll::Delta 正值增 offset)
            let delta = ((wparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
            // strip 上的滚轮 = 标签列表滚动(T8 溢出);lparam 是屏幕坐标
            {
                let mut pt = windows::Win32::Foundation::POINT {
                    x: (lparam.0 & 0xffff) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xffff) as i16 as i32,
                };
                // SAFETY: 坐标换算,输出写回本侧 POINT;失败仅停留在屏幕系
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
                }
                if pt.y >= 0 && (pt.y as f32) < strip_h() {
                    // 滚轮向上 = 看左侧标签(减小 TAB_SCROLL),向下 = 看右侧标签(增大)
                    if tab_scroll_adjust(if delta > 0 { -1 } else { 1 }) {
                        draw_frame();
                    }
                    return LRESULT(0);
                }
            }
            // 应用捕获鼠标:滚轮上报(64=上/65=下,每 120 一事件)优先
            // 于滚回——yazi/TUI 用它滚列表
            {
                let mut pt = windows::Win32::Foundation::POINT {
                    x: (lparam.0 & 0xffff) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xffff) as i16 as i32,
                };
                // SAFETY: 坐标换算,输出写回本侧 POINT
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
                }
                let notches = (delta.abs() / 120).max(1);
                let btn = if delta > 0 { 64 } else { 65 };
                let mut sent = false;
                for _ in 0..notches {
                    if forward_mouse(pt.x, pt.y, btn, true, false) {
                        sent = true;
                    }
                }
                if sent {
                    return LRESULT(0);
                }
            }
            let mut scrolled = false;
            TABS.with(|tabs| {
                if let Some(t) = tabs
                    .borrow_mut()
                    .get_mut(ACTIVE.get())
                    .and_then(|tab| tab.active_pane())
                {
                    t.term.scroll_display(ScrollCommand::Delta(delta / 40));
                    // 拖选接续滚轮(T6):按住拖选中滚轮,选区端点跟视口边缘走
                    if MOUSE_DOWN.with(std::cell::Cell::get) {
                        t.term.selection_extend_edge(delta > 0);
                    }
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
        WM_DPICHANGED => {
            // 跨屏拖动/系统缩放变更(M5b/T4):wparam 高 16 位 = 新 DPI;
            // lparam = 建议矩形指针(按新 DPI 的同逻辑尺寸,系统惯例采纳)
            let dpi = ((wparam.0 >> 16) & 0xffff) as u32;
            // SAFETY: lparam 指向系统栈上的 RECT,本消息期间有效
            let suggested: &RECT = unsafe { &*(lparam.0 as *const RECT) };
            eprintln!(
                "[dpi] CHANGED →{dpi} suggested {}x{} @{},{}",
                suggested.right - suggested.left,
                suggested.bottom - suggested.top,
                suggested.left,
                suggested.top
            );
            // SAFETY: 按建议矩形重定尺寸(无激活无置顶)
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                        | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                );
            }
            reload_dpi(hwnd, dpi);
            draw_frame();
            LRESULT(0)
        }
        WM_SIZE => {
            // lparam 低位 = 客户区宽,高位 = 客户区高(像素)
            let width = (lparam.0 & 0xffff) as u32;
            let height = ((lparam.0 >> 16) & 0xffff) as u32;
            if std::env::var_os("MICA_DEBUG_DPI").is_some() {
                eprintln!("[dpi] WM_SIZE {width}x{height} wparam={}", wparam.0);
            }
            if wparam.0 == windows::Win32::UI::WindowsAndMessaging::SIZE_MINIMIZED as usize
                || width == 0
                || height == 0
            {
                return LRESULT(0); // 最小化
            }
            let mut resized = false;
            resized |= resize_all_grids(width as f32, height as f32);
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
                // T08 批预算:单批 ≤256 KiB,余量补投自唤醒续批(让出
                // 消息循环,DSR 应答/输入不被长批饿死)
                // T08 批预算:单批 ≤256 KiB;drain_batch 内部完成
                // "清标记→复查→补通知"的防丢唤醒契约(含续批自唤醒)
                let Some(bytes) = t.pty_buf.drain_batch(pty_bridge::UI_BATCH_BYTES) else {
                    return; // 积压的重复唤醒:缓冲已被上一条取空,免重绘
                };
                t.term.feed(&bytes);
                flush_input(t);

                // 钉在历史区时仍有输出:保守全量(滚动期间通常无输出,量小)
                if t.term.display_offset() > 0 {
                    t.force_full = true;
                }
                // 终端对查询的应答(DSR/OSC)必须写回 pty,否则 shell 会卡在等待
                for reply in t.term.take_pty_writes() {
                    let _ = stage_input(t, reply.as_bytes());
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
                    // 锁定标题(Ghostty `title`):OSC 0/2 忽略;窗口标题
                    // 保持锁定值(draw_frame titles 已同源,无需另写)
                    let shown = pretty_title(&title);
                    guard[tab_idx].title = shown.clone();
                    if is_active {
                        let win_title = TITLE_LOCK.with(|t| t.borrow().clone()).unwrap_or(shown);
                        let _ = SetWindowTextW(hwnd, &HSTRING::from(win_title));
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
            // 合帧去抖(2026-10-07):应用重绘是几十次独立小写入,逐块
            // 立绘 = 肉眼可见的"一块一块长出来"(用户实测 Claude Code)。
            // 数据到 → 5ms 定时器,窗口期内的新块同帧合并——5ms 对打字
            // 回显无感,对突发重绘是整数量级的合并。2026 的持帧路径
            // ConPTY 场景不可达(边界清单),这是其通用替代。
            // 2026-11-10:改"只武装一次"——旧版每批重置定时器,持续输出
            // 快于 5ms/批时绘制被无限顺延;距上帧超 16ms 直绘兜底
            // (WM_TIMER 低优先级,自唤醒链灌满队列时排不上)
            if drew {
                let since = LAST_FRAME.with(|f| f.get().map(|t| t.elapsed()));
                match frame_sched(DRAW_ARMED.with(std::cell::Cell::get), since) {
                    FrameSched::DrawNow => {
                        DRAW_ARMED.with(|a| a.set(false));
                        windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 4).ok();
                        draw_frame();
                    }
                    FrameSched::Arm => {
                        DRAW_ARMED.with(|a| a.set(true));
                        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
                            Some(hwnd),
                            4,
                            COALESCE_FRAME_MS,
                            None,
                        );
                    }
                    FrameSched::Pending => {}
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            // id 分流:2 = 2026 同步安全阀(D29);1 = 光标闪烁(D20);
            // 3 = 子进程退出轮询(M5b/T3);4 = 合帧去抖到期(WM_APP_RENDER);
            // 5 = strip 动画步进(focus 相位/胶囊滑移,2026-10-12)
            match wparam.0 {
                8 => {
                    // T08 三次返工:输入续灌心跳(真实接线——shell 静默时
                    // WM_APP_RENDER 不来,这是欠账唯一推进者;覆盖全部 pane)
                    flush_all_panes();
                }
                3 => {
                    // 子进程退出轮询(M5b/T3):无论是否有新退出均直接收尾,不得落入闪烁分支
                    if poll_exits() {
                        draw_frame();
                    }
                }
                7 => {
                    // #59 非关键初始化补做(一拍定时器;主线程 = TLS 安全)
                    windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 7).ok();
                    unsafe { deferred_init(hwnd) };
                }
                5 => {
                    if strip_anim_step() {
                        ensure_anim_timer();
                    } else {
                        windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 5).ok();
                    }
                    draw_frame();
                }
                4 => {
                    DRAW_ARMED.with(|a| a.set(false));
                    windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 4).ok();
                    draw_frame();
                }
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
                1 => {
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
                _ => {}
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
            let sm = strip_metrics();
            let sh = strip_h() as i32; // 动态:单标签 = 仅标题行高
            // 边缘 8 向 resize(优先于一切)
            let rb = resize_border();
            let (near_l, near_r) = (cx < rb, cx >= w - rb);
            let (near_t, near_b) = (cy < rb, cy >= h - rb);
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
            // 双行条(2026-10-12):第一行 = 标题 + caption 三键(右端;
            // 其余 = 拖拽);第二行 = 标签与右端 "+"(HTCLIENT,点击走
            // WM_LBUTTONDOWN 的切换/新建/× 分支),空档 = 拖拽区——
            // T8 教训:整条判 HTCAPTION 会把标签点击变成拖窗
            if cy < sh {
                if cy < sm.title_h as i32 {
                    let btn_left = w - (sm.caption_w * 3.0) as i32;
                    if cx >= btn_left {
                        if ((cx - btn_left) as f32) < sm.caption_w {
                            return LRESULT(HTMINBUTTON as isize);
                        }
                        if ((cx - btn_left) as f32) < sm.caption_w * 2.0 {
                            return LRESULT(HTMAXBUTTON as isize);
                        }
                        return LRESULT(HTCLOSE as isize);
                    }
                    return LRESULT(HTCAPTION as isize);
                }
                if strip_hit(cx, cy).is_some() {
                    return LRESULT(HTCLIENT as isize);
                }
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }
        WM_NCLBUTTONDOWN => {
            // 最大化态拖 caption = 还原并继续拖(#34):无边框窗 DefWindowProc
            // 不自动还原,手动 SC_RESTORE 后进入移动循环(SC_MOVE|HTCAPTION,
            // Chrome 同款手法)。还原位置暂取旧矩形(光标跟随重定位留打磨)
            if wparam.0 as u32 == HTCAPTION
                && windows::Win32::UI::WindowsAndMessaging::IsZoomed(hwnd).as_bool()
            {
                let _ = SendMessageW(
                    hwnd,
                    WM_SYSCOMMAND,
                    Some(WPARAM(SC_RESTORE as usize)),
                    Some(LPARAM(0)),
                );
                let _ = SendMessageW(hwnd, WM_SYSCOMMAND, Some(WPARAM(0xF012)), Some(lparam));
                return LRESULT(0);
            }
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
                    let cmd = if windows::Win32::UI::WindowsAndMessaging::IsZoomed(hwnd).as_bool() {
                        SC_RESTORE
                    } else {
                        SC_MAXIMIZE
                    };
                    let _ = SendMessageW(
                        hwnd,
                        WM_SYSCOMMAND,
                        Some(WPARAM(cmd as usize)),
                        Some(LPARAM(0)),
                    );
                }
                HTCLOSE => {
                    // T04:统一关闭入口——投 WM_CLOSE(wndproc 里走真
                    // DestroyWindow)。旧实现投伪 WM_DESTROY:计数与退出
                    // 序照跑但 HWND 不销毁,副窗场景留僵尸窗口
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
            }
            LRESULT(0)
        }
        WM_NCLBUTTONDBLCLK => {
            // 双击顶栏 = 最大化/还原(WT 同款)
            if wparam.0 as u32 == HTCAPTION {
                let cmd = if windows::Win32::UI::WindowsAndMessaging::IsZoomed(hwnd).as_bool() {
                    SC_RESTORE
                } else {
                    SC_MAXIMIZE
                };
                let _ = SendMessageW(
                    hwnd,
                    WM_SYSCOMMAND,
                    Some(WPARAM(cmd as usize)),
                    Some(LPARAM(0)),
                );
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_APP_IPC => {
            // IPC 客户端请求(M3a → T06):主线程排空消息队列,逐条应用并
            // **应答**(请求带单槽回复通道,服务端在管道上等这个响应)
            loop {
                let incoming = {
                    let mut slot = IPC_RX.lock().unwrap_or_else(|e| e.into_inner());
                    slot.as_mut().and_then(|rx| rx.try_recv().ok())
                };
                let Some(incoming) = incoming else { break };
                let resp = unsafe { apply_ipc_request(hwnd, &incoming.req) };
                if let Some(resp) = resp {
                    incoming.reply(resp);
                }
            }
            // activate 语义:前置既有窗口
            let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd) };
            draw_frame();
            LRESULT(0)
        }
        WM_PAINT => {
            // 绘制节奏由消息循环控制;这里只清掉无效区积压
            let _ = ValidateRect(Some(hwnd), None);
            LRESULT(0)
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED => {
            if config::system_theme_changed() {
                let _ = PostMessageW(Some(hwnd), WM_APP_CONFIG, WPARAM(0), LPARAM(0));
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_APP_TRANSFER => {
            transfer::receive(hwnd, wparam.0, lparam.0 as usize);
            LRESULT(0)
        }
        WM_APP_CLOSE_EMPTY => {
            if TABS.with(|t| t.borrow().is_empty()) {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_APP_DETACH_READY => {
            window::finish_detach();
            LRESULT(0)
        }
        WM_APP_CONFIG => {
            // 配置热重载(watch 线程去抖后投递);主线程执行,失败保旧
            reload_config(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            // T04 统一关闭入口:一切关闭(Alt+F4 走 DefWindowProc 同途、
            // caption ×、最后标签)都到达这里,真实销毁 HWND;WM_DESTROY
            // 才减计数/发退出——禁止再伪造 WM_DESTROY
            // SAFETY: 本线程拥有该窗口,销毁合法
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // 真实销毁(WM_CLOSE → DestroyWindow)必经;注销窗口表在
            // 计数递减前,此后 IPC/配置通知不再寻址本窗
            let slot = SHARED_HWND
                .with(|s| s.borrow().clone())
                .unwrap_or_else(|| Arc::new(Mutex::new(Some(hwnd.0 as isize))));
            window_registry::unregister(&slot);
            WINDOW_COUNT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// T06:应用一条 IPC 请求并生成应答。参数校验在这里(服务端只挡未知
/// op);send-text 成功仅承诺字节写入 PTY——不承诺 shell 执行完毕。
unsafe fn apply_ipc_request(hwnd: HWND, req: &IpcRequest) -> Option<IpcResponse> {
    match req.op.as_str() {
        "activate" => Some(IpcResponse::applied(req.id)),
        // new-window 是异步派发(窗口线程创建),此刻无法确知创建结果——
        // 如实回 Queued(复核 3:仅派发线程不得回 Applied)
        "new-window" => {
            spawn_window(None);
            Some(IpcResponse::queued(req.id))
        }
        "new-tab" => {
            if let Some(fs) = req.font_size
                && (!fs.is_finite() || !(6.0..=72.0).contains(&fs))
            {
                return Some(IpcResponse::error(req.id, IpcError::BadFontSize));
            }
            if let Some(profile) = req.profile.as_deref() {
                let known = mica_core::profile::cached_profiles()
                    .iter()
                    .any(|p| p.name.eq_ignore_ascii_case(profile));
                if !known {
                    return Some(IpcResponse::error(req.id, IpcError::BadProfile));
                }
            }
            // 真实结果(复核 3):创建失败(DEGRADED/spawn 失败)回错误,
            // 不再假报 Applied
            Some(
                match start_tab_with_profile(hwnd, req.profile.as_deref(), req.font_size) {
                    true => IpcResponse::applied(req.id),
                    false if !lifecycle::spawn_capacity_ok() => {
                        IpcResponse::error(req.id, IpcError::Degraded)
                    }
                    false => IpcResponse::error(req.id, IpcError::SpawnFailed),
                },
            )
        }
        "send-text" => send_text_ipc(req),
        // 服务端已挡未知 op;防御性兜底
        _ => Some(IpcResponse::error(req.id, IpcError::UnknownOp)),
    }
}

/// send-text 的 IPC 语义(四轮返工复核 1/5):**UI 只做一次非阻塞接收**
/// (stage_into,零等待)——窗口线程绝不阻塞在排水上;期间输入、关闭、
/// 绘制与续灌定时器照常(生产入口回归见 input_flow_gui.rs 的
/// saturated_send_keeps_window_responsive)。
/// - Accepted → Applied(字节全部在写队列,writer 将写出)。
/// - Debt → **Queued**:字节已接收、仍会随 writer 排水写出(心跳
///   续灌推进,与客户端是否等待无关);不承诺 Applied,也绝不取消
///   ——CLI 文案明示"已接收仍在推进,勿盲目重发"。等待/完成不在
///   窗口线程上发生。
/// - RejectedOverflow/Closed → 精确错误码(input_overflow/pty_write),
///   事务零副作用(未接收任何字节)。
fn send_text_ipc(req: &IpcRequest) -> Option<IpcResponse> {
    let bytes = req.text.clone().unwrap_or_default().into_bytes();
    let payload_for = |t: &mut Terminal| -> Vec<u8> {
        if t.term.bracketed_paste_active() {
            let mut w = b"\x1b[200~".to_vec();
            w.extend_from_slice(&bytes);
            w.extend_from_slice(b"\x1b[201~");
            w
        } else {
            bytes.clone()
        }
    };
    match req.tab {
        Some(-1) => {
            // QT:所属续灌调度 = QT id 7;错误码精确区分三种拒绝
            if !quickterm::exists() {
                return Some(IpcResponse::error(req.id, IpcError::BadTarget));
            }
            match quickterm::accept_qt(&bytes) {
                pty_bridge::InputFlow::Accepted => Some(IpcResponse::applied(req.id)),
                pty_bridge::InputFlow::Debt(_) => Some(IpcResponse::queued(req.id)),
                pty_bridge::InputFlow::RejectedOverflow => {
                    Some(IpcResponse::error(req.id, IpcError::InputOverflow))
                }
                pty_bridge::InputFlow::RejectedClosed => {
                    Some(IpcResponse::error(req.id, IpcError::PtyWrite))
                }
            }
        }
        target => {
            let flow = TABS.with(|tabs| {
                let mut guard = tabs.borrow_mut();
                let t = match target {
                    Some(n) if n >= 0 => {
                        guard.get_mut(n as usize).and_then(|tab| tab.active_pane())
                    }
                    _ => guard
                        .get_mut(ACTIVE.get())
                        .and_then(|tab| tab.active_pane()),
                };
                let t = t?;
                let payload = payload_for(t);
                Some(stage_input(t, &payload)) // stage_input 内武装主窗 id 8
            });
            let Some(flow) = flow else {
                return Some(IpcResponse::error(req.id, IpcError::BadTarget));
            };
            Some(match flow {
                pty_bridge::InputFlow::Accepted => IpcResponse::applied(req.id),
                pty_bridge::InputFlow::Debt(_) => IpcResponse::queued(req.id),
                pty_bridge::InputFlow::RejectedOverflow => {
                    IpcResponse::error(req.id, IpcError::InputOverflow)
                }
                pty_bridge::InputFlow::RejectedClosed => {
                    IpcResponse::error(req.id, IpcError::PtyWrite)
                }
            })
        }
    }
}

/// 粘贴入口(复核 3/4):**与提交同一个事务组件**——`stage_into`
/// 自身就是整体接受/整体拒绝(预判与提交同源,FIFO 与容量规则内在),
#[cfg(test)]
mod tests {
    use super::{
        FrameSched, detect_path_token, fit_outer_size, frame_sched, normalize_path_token,
        pretty_title,
    };

    #[test]
    fn fit_outer_size_constrains_only_what_exceeds_work_area() {
        // 大屏:请求原样通过(本机 2032×1041 精确回归的纯函数面)
        assert_eq!(fit_outer_size((2032, 1041), (2560, 1440)), (2032, 1041));
        // 小屏(1024×768 runner 实测 work):整体收到工作区
        assert_eq!(fit_outer_size((2032, 1041), (1024, 728)), (1024, 728));
        // 工作区内请求不受影响
        assert_eq!(fit_outer_size((640, 480), (1024, 728)), (640, 480));
    }

    #[test]
    fn exact_pixel_window_size_overrides_cell_dimensions() {
        let settings = mica_core::config::settings::resolve(
            "window-size-pixels = 2032,1041\nwindow-width = 10\nwindow-height = 4",
        )
        .unwrap();
        let metrics = mica_render::font::metrics::FontMetrics {
            cell_width: 10.0,
            line_height: 20.0,
            ascent: 15.0,
            descent: 5.0,
        };
        let rect = super::initial_window_rect(&settings, &metrics);
        assert_eq!((rect.right, rect.bottom), (2032, 1041));
    }

    #[test]
    fn footer_scissor_includes_last_terminal_row_below_header() {
        let rect = mica_core::layout::Rect::new(3.0, 51.0, 794.0, 746.0);
        let (x, y, w, h) = super::pane_scissor(rect, (800.0, 752.0), 48.0);
        assert_eq!((x, y, w, h), (3, 51, 794, 746));
        assert_eq!(
            y + h,
            797,
            "last row must not be clipped by a second header subtraction"
        );
    }

    #[test]
    fn hovered_path_span_keeps_nonzero_start_column() {
        let text = "open C:/src/main.rs:12 now";
        let columns: Vec<usize> = (0..text.chars().count()).collect();
        let (start, cells) = super::span_of_token(text, &columns, 10).unwrap();
        assert_eq!((start, cells), (5, 17));
    }

    #[test]
    fn pane_local_pointer_does_not_subtract_header_and_history_is_negative() {
        let metrics = mica_render::font::metrics::FontMetrics {
            cell_width: 10.0,
            line_height: 20.0,
            ascent: 15.0,
            descent: 5.0,
        };
        let (point, _) = super::px_to_buffer_point(25.0, 25.0, &metrics, 5);
        assert_eq!(point.column.0, 2);
        assert_eq!(point.line.0, -4);
    }

    #[test]
    fn path_span_and_hit_testing_follow_wide_cells_and_scrolled_pane() {
        let mut surface =
            mica_core::surface::Surface::new(mica_core::surface::ScreenSize::new(32, 3));
        surface.feed("你好 C:/文件/main.rs".as_bytes());
        let (text, columns) = mica_render::frame::row_string(surface.grid(), 0);
        let (start, cells) = super::span_of_token(&text, &columns, 9).unwrap();
        assert_eq!(start, 5);
        assert_eq!(cells, 15);
        // Both columns of the final wide filename character are part of the path.
        assert_eq!(
            super::detect_path_token(&text, &columns, 11).as_deref(),
            Some("C:\\文件\\main.rs")
        );
        let metrics = mica_render::font::metrics::FontMetrics {
            cell_width: 10.0,
            line_height: 20.0,
            ascent: 15.0,
            descent: 5.0,
        };
        let pane = mica_core::layout::Rect::new(200.5, 90.5, 320.0, 60.0);
        let hover = super::PathHover {
            pane_id: 42,
            line: -2,
            column: start,
            cells,
        };
        let rect = super::path_hover_rect(hover, 42, pane, &metrics, 3, 3).unwrap();
        assert_eq!(
            (rect.x, rect.y, rect.w, rect.h),
            (250.5, 110.5, 150.0, 20.0)
        );
        assert!(super::path_hover_rect(hover, 99, pane, &metrics, 3, 3).is_none());
        let (point, _) =
            super::px_to_buffer_point(rect.x - pane.x + 1.0, rect.y - pane.y + 1.0, &metrics, 3);
        assert_eq!((point.line.0, point.column.0), (-2, start));
        assert_eq!(
            super::normalize_path_token("https://example.test/a.rs"),
            None
        );
    }

    #[test]
    fn path_spans_do_not_include_wrap_placeholder_and_keep_combining_marks() {
        let mut surface =
            mica_core::surface::Surface::new(mica_core::surface::ScreenSize::new(10, 2));
        surface.feed("C:/foobar中".as_bytes());
        let (text, columns) = mica_render::frame::row_string(surface.grid(), 0);
        assert_eq!(super::all_path_spans(&text, &columns), vec![(0, 9)]);
        assert!(super::detect_path_token(&text, &columns, 9).is_none());
        let mut surface =
            mica_core::surface::Surface::new(mica_core::surface::ScreenSize::new(24, 2));
        surface.feed("C:/cafe\u{0301}.rs".as_bytes());
        let (text, columns) = mica_render::frame::row_string(surface.grid(), 0);
        assert_eq!(
            super::detect_path_token(&text, &columns, 6).as_deref(),
            Some("C:\\cafe\u{0301}.rs")
        );
        assert_eq!(super::span_of_token(&text, &columns, 6), Some((0, 10)));
    }

    #[test]
    fn clearing_keymap_does_not_leave_a_shift_insert_paste_fallback() {
        let mods = mica_core::input::Mods {
            shift: true,
            ..Default::default()
        };
        let settings = mica_core::config::settings::resolve("keybind = clear").unwrap();
        assert_eq!(
            settings
                .keymap
                .lookup(mods, mica_core::keymap::TriggerKey::Insert),
            None
        );
        assert!(!super::contextual_paste(mods, 0x2d));
        let ctrl = mica_core::input::Mods {
            ctrl: true,
            ..Default::default()
        };
        assert!(super::contextual_paste(ctrl, 'V' as u32));
    }

    #[test]
    fn explicit_profile_overrides_configured_command_and_reset_restores_default() {
        let previous = super::SESSION_COMMAND.with(|s| s.replace(Some("pwsh.exe -NoLogo".into())));
        assert_eq!(super::launch_command(None), "pwsh.exe -NoLogo");
        let profile = mica_core::profile::Profile {
            name: "Command Prompt".into(),
            command: "cmd.exe".into(),
        };
        assert_eq!(super::launch_command(Some(&profile)), "cmd.exe");
        super::SESSION_COMMAND.with(|s| s.replace(None));
        assert_eq!(super::launch_command(None), "powershell.exe -NoLogo");
        super::SESSION_COMMAND.with(|s| s.replace(previous));
    }

    /// 标题呈现:exe 路径 basename 去扩展(Ghostty direct-command argv[0]
    /// 的 Windows 适配);普通 OSC 标题原样(nvim / user@host:cwd)
    #[test]
    fn pretty_title_basenames_exe_paths_only() {
        assert_eq!(
            pretty_title(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
            "powershell"
        );
        assert_eq!(pretty_title(r"C:\Windows\system32\cmd.exe"), "cmd");
        assert_eq!(pretty_title("nvim"), "nvim");
        assert_eq!(pretty_title("user@host: ~/mica"), "user@host: ~/mica");
    }

    /// #58 端到端检测回归网(2026-10-22 用户实报不能跳转,检测层定责)
    #[test]
    fn detect_path_token_e2e() {
        let text = r"PS C:\x> echo C:\proj\src\main.rs:3:10";
        let cell_of: Vec<usize> = (0..text.chars().count()).collect();
        assert_eq!(
            detect_path_token(text, cell_of.as_slice(), 20).as_deref(),
            Some(r"C:\proj\src\main.rs")
        );
        assert_eq!(
            detect_path_token(text, cell_of.as_slice(), 0),
            None,
            "提示符区无路径"
        );
        let text2 = "error: found at src/lib.rs:42:7 here";
        let cell_of2: Vec<usize> = (0..text2.chars().count()).collect();
        assert!(
            detect_path_token(text2, cell_of2.as_slice(), 20).is_some(),
            "rustc 风格"
        );
    }

    /// #58 路径 token 判定:吃真路径、不吃纯词/时间戳/URL 前缀
    #[test]
    fn path_token_detection() {
        // 真路径(绝对/相对/rustc 风格带行列)
        assert_eq!(
            normalize_path_token(r"C:\proj\src\main.rs:3:10"),
            Some(r"C:\proj\src\main.rs".into())
        );
        assert_eq!(
            normalize_path_token("src/main.rs:42"),
            Some(r"src\main.rs".into())
        );
        assert_eq!(
            normalize_path_token("./relative/file.txt"),
            Some(r".\relative\file.txt".into())
        );
        assert_eq!(normalize_path_token("../up/dir"), Some(r"..\up\dir".into()));
        assert_eq!(
            normalize_path_token("/unix/abs/path"),
            Some(r"\unix\abs\path".into())
        );
        // 误报防线:纯词 / 版本号 / 时间戳 / 单段文件名
        assert_eq!(normalize_path_token("hello"), None);
        assert_eq!(normalize_path_token("v1.2.3"), None);
        assert_eq!(normalize_path_token("12:34:56"), None);
        assert_eq!(
            normalize_path_token("main.rs"),
            None,
            "无目录的单文件名不吃(太宽)"
        );
        assert_eq!(normalize_path_token("a/b"), None, "单斜杠无扩展名不吃");
        assert_eq!(normalize_path_token("::"), None);
    }

    /// 合帧决策表:只武装一次(续批不顺延,旧版每批重置可无限推迟
    /// 绘制)+ 距上帧超 16ms 直绘兜底(WM_TIMER 低优先级饥饿)
    #[test]
    fn frame_sched_arm_once_and_deadline() {
        use std::time::Duration;
        // 首批:武装 5ms 合帧窗(从未绘帧亦武装)
        assert_eq!(frame_sched(false, None), FrameSched::Arm);
        assert_eq!(
            frame_sched(false, Some(Duration::from_millis(1))),
            FrameSched::Arm
        );
        // 已武装 + 新批:并帧,不顺延截止时间
        assert_eq!(
            frame_sched(true, Some(Duration::from_millis(1))),
            FrameSched::Pending
        );
        assert_eq!(frame_sched(true, None), FrameSched::Pending);
        // 距上帧 ≥16ms:绕过定时器直绘(武装与否)
        assert_eq!(
            frame_sched(true, Some(Duration::from_millis(16))),
            FrameSched::DrawNow
        );
        assert_eq!(
            frame_sched(false, Some(Duration::from_millis(50))),
            FrameSched::DrawNow
        );
    }
}
