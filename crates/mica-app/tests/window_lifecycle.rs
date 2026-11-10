//! T04 窗口生命周期集成回归:真实 mica 子进程 + 真实窗口消息。
//! 只操作**自己启动的实例**的窗口(按 PID 过滤),绝不触碰用户窗口;
//! 单实例管道全局唯一 → 全部用例串行(GUARD)。

#![cfg(windows)]

use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindow,
    IsWindowVisible, PostMessageW, SendMessageW, WM_CLOSE, WM_NCLBUTTONDOWN,
};
use windows::core::BOOL;

/// 串行锁:单实例管道唯一,并行用例会互抢实例。
static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
fn guard() -> &'static Mutex<()> {
    GUARD.get_or_init(|| Mutex::new(()))
}

/// 关闭守卫:无论如何都树杀子进程(绝不留孤儿实例占管道)。
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().map(|s| s.is_none()).unwrap_or(false) {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// 本用例的隔离身份(唯一管道名 + 状态目录,复核 6)。
static TEST_USER: OnceLock<Mutex<String>> = OnceLock::new();

fn test_user() -> String {
    TEST_USER
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn launch_isolated() -> ChildGuard {
    // 配置隔离:空 APPDATA = 全默认;USERNAME 派生唯一管道;
    // LOCALAPPDATA 隔离状态目录(pty 注册表)
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let iso = std::env::temp_dir().join(format!("mica-winlife-{unique}"));
    std::fs::create_dir_all(&iso).unwrap();
    let user = format!("micawin{unique}");
    *TEST_USER
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = user.clone();
    let child = Command::new(env!("CARGO_BIN_EXE_mica"))
        .arg("--multi")
        .env("APPDATA", &iso)
        .env("LOCALAPPDATA", &iso)
        .env("USERNAME", &user)
        .env("MICA_CRASH_NOPROMPT", "1")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(iso.join("stderr.log")).unwrap())
        .spawn()
        .expect("launch mica");
    ChildGuard(child)
}

/// 预置(空但存在的)config 文件启动:热重载 watcher 只在文件存在时
/// 建立(spawn_config_watcher 的契约)。
fn launch_with_config() -> (ChildGuard, std::path::PathBuf) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let iso = std::env::temp_dir().join(format!("mica-winlife-cfg-{unique}"));
    let dir = iso.join("mica");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("config");
    std::fs::write(&cfg, b"").unwrap(); // 空文件 = 全默认且 watcher 启用
    let user = format!("micawincfg{unique}");
    *TEST_USER
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = user.clone();
    let log_path = iso.join("stderr.log");
    eprintln!("Window lifecycle diagnostics: {}", log_path.display());
    let child = Command::new(env!("CARGO_BIN_EXE_mica"))
        .arg("--multi")
        .env("APPDATA", &iso)
        .env("LOCALAPPDATA", &iso)
        .env("USERNAME", &user)
        .env("MICA_CRASH_NOPROMPT", "1")
        .env("MICA_DEBUG_TIMING", "1")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log_path).unwrap())
        .spawn()
        .expect("launch mica");
    let mut app = ChildGuard(child);
    // IPC ready / visible HWND 都早于 timer 7 的延迟初始化。先等待真实
    // 初始化完成日志,再测试配置变更;否则可能在 watcher 注册之前写入,
    // 或在 watcher 建立者完成初始化前关闭它。延长 reload 超时救不回丢失事件。
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if log.contains("[timing] 延迟段 ") {
            break;
        }
        let status = app.0.try_wait().unwrap();
        assert!(
            status.is_none() && Instant::now() < deadline,
            "等待配置 watcher 初始化失败; status={status:?}; log={}; stderr:\n{log}",
            log_path.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    (app, cfg)
}

/// 读窗口标题(热重载 title 即时应用的观测面)。
fn window_title(hwnd: windows::Win32::Foundation::HWND) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: 缓冲与长度对应
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// 复核 4:主窗先关后改配置——watcher 是进程级服务,热重载必须继续
/// 工作(标题从默认变为配置值 = reload 真实执行的外部证据)。
#[test]
fn config_reload_survives_main_close() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, cfg) = launch_with_config();
    assert!(wait_ready(Duration::from_secs(20)), "IPC 未就绪");
    let first = wait_windows(app.0.id(), 1, Duration::from_secs(10));
    assert!(cli(&["new-window"]), "开副窗失败");
    let ws = wait_windows(app.0.id(), 2, Duration::from_secs(10));
    let other = ws.iter().find(|h| **h != first[0]).copied().unwrap();
    // 关主窗(watcher 的建立者)
    // SAFETY: 本测试的子进程窗口
    unsafe {
        let _ = PostMessageW(Some(first[0]), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { IsWindow(Some(first[0])) }.as_bool() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!unsafe { IsWindow(Some(first[0])) }.as_bool(), "主窗销毁");
    assert!(app.0.try_wait().unwrap().is_none(), "副窗在,进程存活");

    // 改配置 → 去抖 300ms → 广播 → 副窗 reload → title 即时应用
    std::fs::write(&cfg, b"title = ReloadAfterMainClose").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while window_title(other) != "ReloadAfterMainClose" && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        window_title(other),
        "ReloadAfterMainClose",
        "主窗关闭后热重载必须仍然工作"
    );

    // 收尾
    // SAFETY: 本测试的子进程窗口
    unsafe {
        let _ = PostMessageW(Some(other), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    assert!(wait_exit(&mut app.0, Duration::from_secs(5)));
}

/// 复核 4:多窗同时更新——广播到全部存活窗口,无漂移。
#[test]
fn both_windows_reload_from_broadcast() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, cfg) = launch_with_config();
    assert!(wait_ready(Duration::from_secs(20)), "IPC 未就绪");
    let _first = wait_windows(app.0.id(), 1, Duration::from_secs(10));
    assert!(cli(&["new-window"]), "开副窗失败");
    let ws = wait_windows(app.0.id(), 2, Duration::from_secs(10));

    std::fs::write(&cfg, b"title = BothReloaded").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while ws.iter().any(|h| window_title(*h) != "BothReloaded") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    for h in &ws {
        assert_eq!(window_title(*h), "BothReloaded", "广播必须覆盖全部窗口");
    }

    // SAFETY: 本测试的子进程窗口
    unsafe {
        for h in &ws {
            let _ = PostMessageW(Some(*h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    assert!(wait_exit(&mut app.0, Duration::from_secs(5)));
}

/// CLI 过 IPC 驱动实例(activate/new-tab/new-window)。
/// 注意:除 send 外的 IPC 命令在管道不通时会**自启 GUI**(dispatch_ipc
/// 兜底),就绪探测只能用 send——它失败时干净退出 1,绝不自启。
fn cli(args: &[&str]) -> bool {
    Command::new(env!("CARGO_BIN_EXE_mica"))
        .args(args)
        .env("USERNAME", test_user())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 等 IPC 管道就绪:send 受理(exit 0)即就绪;连不上 exit 1 不自启。
fn wait_ready(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cli(&["send", "--literal", "x"]) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// EnumWindows 的收集上下文(回调与调用方共用)。
struct EnumCtx {
    pid: u32,
    out: Vec<HWND>,
}

/// 枚举属于指定 PID 的可见 mica 主窗。
fn windows_of(pid: u32) -> Vec<HWND> {
    let mut ctx = EnumCtx {
        pid,
        out: Vec::new(),
    };
    let ctx_ptr = &mut ctx as *mut EnumCtx;
    // SAFETY: 回调里只写调用方栈上的 ctx;EnumWindows 同步完成
    unsafe {
        let _ = EnumWindows(
            Some(win32_enum_proc as unsafe extern "system" fn(HWND, LPARAM) -> BOOL),
            LPARAM(ctx_ptr as isize),
        );
    }
    ctx.out
}

/// EnumWindows 回调:收集属于目标 PID 的 mica 主窗。
unsafe extern "system" fn win32_enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY(Rust 2024 unsafe_op_in_unsafe_fn):lparam 指向调用方栈上的
    // Ctx,EnumWindows 同步完成;Win32 调用按各自契约
    unsafe {
        let ctx = &mut *(lparam.0 as *mut EnumCtx);
        let mut wpid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut wpid));
        if wpid == ctx.pid && IsWindowVisible(hwnd).as_bool() {
            let mut name = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut name);
            let class = String::from_utf16_lossy(&name[..n.max(0) as usize]);
            if class == "mica_app_class" {
                ctx.out.push(hwnd);
            }
        }
    }
    BOOL(1)
}

fn wait_windows(pid: u32, want: usize, timeout: Duration) -> Vec<HWND> {
    let deadline = Instant::now() + timeout;
    loop {
        let ws = windows_of(pid);
        if ws.len() >= want {
            return ws;
        }
        assert!(
            Instant::now() < deadline,
            "{timeout:?} 内未见 {want} 个窗口"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_exit(child: &mut std::process::Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().map(|s| s.is_some()).unwrap_or(false) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 模拟 caption ×:向 HTCLOSE 区投 NCLBUTTONDOWN(应用据此走关闭路径)。
/// 坐标取窗口右上角内侧(× 按钮区,NCHITTEST 的 HTCLOSE 带)。
fn caption_close(hwnd: HWND) {
    let mut rect = RECT::default();
    // SAFETY: 只读窗口矩形
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rect);
        let x = (rect.right - 40).max(rect.left + 1);
        let y = rect.top + 12;
        let lp = LPARAM((((y as isize) & 0xFFFF) << 16) | ((x as isize) & 0xFFFF));
        let _ = SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(20 /* HTCLOSE */)),
            Some(lp),
        );
    }
}

/// 关闭最后窗口:进程及时退出,无挂死(F05 修前:close_tab 只
/// PostQuitMessage,计数不归零,run() 尾部永久 park)。
#[test]
fn last_window_close_exits_process() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let mut app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)), "IPC 未就绪");
    let ws = wait_windows(app.0.id(), 1, Duration::from_secs(10));
    // SAFETY: 目标是本测试启动的子进程窗口
    unsafe {
        let _ = PostMessageW(Some(ws[0]), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    assert!(
        wait_exit(&mut app.0, Duration::from_secs(5)),
        "关最后窗口后 5s 内进程必须退出"
    );
}

/// 核心回归:双窗口下 caption × 关闭 A → A 的 HWND **真实销毁**
/// (IsWindow=false),进程与窗口 B 存活。修前投伪 WM_DESTROY:A 计数
/// 递减但 HWND 不销毁——僵尸窗口留在桌面直到进程退出。
#[test]
fn caption_close_destroys_window_keeps_process() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let mut app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)), "IPC 未就绪");
    let first = wait_windows(app.0.id(), 1, Duration::from_secs(10));
    assert!(cli(&["new-window"]), "开副窗失败");
    let ws = wait_windows(app.0.id(), 2, Duration::from_secs(10));
    let other = ws.iter().find(|h| **h != first[0]).copied().unwrap();

    caption_close(first[0]);
    // A 销毁 + 进程/B 存活
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { IsWindow(Some(first[0])) }.as_bool() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !unsafe { IsWindow(Some(first[0])) }.as_bool(),
        "caption × 必须真实销毁 HWND(修前是僵尸窗口)"
    );
    assert!(app.0.try_wait().unwrap().is_none(), "进程必须存活(还有 B)");
    assert!(
        unsafe { IsWindow(Some(other)) }.as_bool(),
        "副窗 B 必须存活"
    );

    // 收尾:关 B → 进程退出
    // SAFETY: 同上
    unsafe {
        let _ = PostMessageW(Some(other), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    assert!(
        wait_exit(&mut app.0, Duration::from_secs(5)),
        "关 B 后应退出"
    );
}

/// 主窗先关:进程存活,IPC 仍被服务(窗口表寻址副窗),副窗关闭才退出。
#[test]
fn ipc_survives_main_window_close() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let mut app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)), "IPC 未就绪");
    let first = wait_windows(app.0.id(), 1, Duration::from_secs(10));
    assert!(cli(&["new-window"]), "开副窗失败");
    let ws = wait_windows(app.0.id(), 2, Duration::from_secs(10));
    let other = ws.iter().find(|h| **h != first[0]).copied().unwrap();

    // 关主窗(首个窗口;IPC/配置 watcher 由它建立)
    // SAFETY: 本测试的子进程窗口
    unsafe {
        let _ = PostMessageW(Some(first[0]), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { IsWindow(Some(first[0])) }.as_bool() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!unsafe { IsWindow(Some(first[0])) }.as_bool(), "主窗销毁");
    assert!(app.0.try_wait().unwrap().is_none(), "副窗在,进程必须存活");

    // IPC 仍被服务:管道活着、命令被受理(exit 0)——通知由窗口表
    // 寻址到副窗(ACK 语义在 T06 落地,此处先锁"不拒连、不挂死")
    assert!(cli(&["new-tab"]), "主窗关闭后 IPC 必须仍受理");
    assert!(app.0.try_wait().unwrap().is_none(), "受理后进程仍存活");

    // SAFETY: 同上
    unsafe {
        let _ = PostMessageW(Some(other), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    assert!(
        wait_exit(&mut app.0, Duration::from_secs(5)),
        "最后窗口关闭应退出"
    );
}

/// 快速连关双窗:无挂死、无残留窗口(进程在超时内退出)。
#[test]
fn rapid_close_both_windows_exits() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let mut app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)), "IPC 未就绪");
    let _ = wait_windows(app.0.id(), 1, Duration::from_secs(10));
    assert!(cli(&["new-window"]), "开副窗失败");
    let ws = wait_windows(app.0.id(), 2, Duration::from_secs(10));
    // SAFETY: 本测试的子进程窗口
    unsafe {
        for h in &ws {
            let _ = PostMessageW(Some(*h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    assert!(
        wait_exit(&mut app.0, Duration::from_secs(5)),
        "快速连关后进程必须退出"
    );
}
