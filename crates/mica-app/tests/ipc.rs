//! T06 IPC 集成回归:真实 mica 实例 + 裸管道客户端(自定义帧,绕过
//! 生产客户端,专测服务端协议行为)。只操作自己启动的实例。

#![cfg(windows)]

use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use mica_core::ipc::{
    FrameDecode, IpcRequest, IpcResponse, IpcStatus, MAX_FRAME_BYTES, decode_frame, encode_frame,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile};
use windows::Win32::System::Pipes::{PeekNamedPipe, WaitNamedPipeW};
use windows::core::{HSTRING, PCWSTR};

static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
fn guard() -> &'static Mutex<()> {
    GUARD.get_or_init(|| Mutex::new(()))
}

/// 本用例的隔离身份:管道名由 USERNAME 派生——每个用例独占一条管道,
/// 绝不触碰真实用户实例的 \.\pipe\mica-<用户名>(复核 6)。
static TEST_USER: OnceLock<Mutex<String>> = OnceLock::new();

fn test_user() -> String {
    TEST_USER
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

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

fn launch_isolated() -> ChildGuard {
    launch_isolated_config("")
}

fn launch_isolated_config(extra: &str) -> ChildGuard {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let iso = std::env::temp_dir().join(format!("mica-ipc-{unique}"));
    std::fs::create_dir_all(&iso).unwrap();
    write_test_config(&iso);
    if !extra.is_empty() {
        let path = iso.join("mica/config");
        let source = std::fs::read_to_string(&path).unwrap();
        std::fs::write(path, format!("{source}{extra}")).unwrap();
    }
    // 唯一管道身份:USERNAME 派生管道名;LOCALAPPDATA 隔离状态目录
    // (pty 注册表不再写用户真实文件)
    let user = format!("micatest{unique}");
    *TEST_USER
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = user.clone();
    let log_path = iso.join("stderr.log");
    eprintln!("IPC instance diagnostics: {}", log_path.display());
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_mica"))
            .arg("--multi")
            .env("APPDATA", &iso)
            .env("LOCALAPPDATA", &iso)
            .env("USERNAME", &user)
            .env("MICA_CRASH_NOPROMPT", "1")
            .env("MICA_DEBUG_TIMING", "1")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log_path).unwrap())
            .spawn()
            .expect("launch mica"),
    )
}

fn write_test_config(iso: &std::path::Path) {
    std::fs::create_dir_all(iso.join("mica")).unwrap();
    std::fs::write(
        iso.join("mica/config"),
        "command = cmd.exe /Q /K\nquick-terminal-key = none\n",
    )
    .unwrap();
}

fn pipe_name() -> String {
    let user = {
        let t = test_user();
        if t.is_empty() {
            std::env::var("USERNAME").unwrap_or_default()
        } else {
            t
        }
    };
    let safe: String = user
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!(
        r"\\.\pipe\mica-{}",
        if safe.is_empty() {
            "default".to_string()
        } else {
            safe
        }
    )
}

/// 就绪探测:CLI send 走新协议,收到 applied 才算就绪(连不上 exit 1
/// 不自启,probe 安全)。
fn wait_ready(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let ok = Command::new(env!("CARGO_BIN_EXE_mica"))
            .args(["send", "--literal", ""])
            .env("USERNAME", test_user())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// 裸管道连接(独占;PIPE_BUSY 时短暂重试)。
fn connect(timeout: Duration) -> Option<HANDLE> {
    let name: HSTRING = pipe_name().into();
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: 按各 API 契约;句柄由调用方 RAII 关闭
        unsafe {
            if WaitNamedPipeW(PCWSTR(name.as_ptr()), 500).as_bool()
                && let Ok(h) = CreateFileW(
                    PCWSTR(name.as_ptr()),
                    windows::Win32::Storage::FileSystem::FILE_GENERIC_READ.0
                        | windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE.0,
                    windows::Win32::Storage::FileSystem::FILE_SHARE_NONE,
                    None,
                    windows::Win32::Storage::FileSystem::OPEN_EXISTING,
                    Default::default(),
                    None,
                )
            {
                return Some(h);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 裸写(整帧)。
fn write_bytes(h: HANDLE, bytes: &[u8]) -> bool {
    let mut written = 0u32;
    // SAFETY: 缓冲与计数对应
    let ok = unsafe { WriteFile(h, Some(bytes), Some(&mut written), None) }.is_ok();
    ok && written as usize == bytes.len()
}

/// 读响应帧(带期限;返回原始解码结果)。
fn read_response(h: HANDLE, timeout: Duration) -> Option<IpcResponse> {
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match decode_frame::<IpcResponse>(&buf) {
            FrameDecode::NeedMore => {}
            FrameDecode::Complete(r, _) => return Some(r),
            FrameDecode::Invalid(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        let mut avail = 0u32;
        // SAFETY: Peek 判可读量
        if unsafe { PeekNamedPipe(h, None, 0, None, Some(&mut avail), None) }.is_err() {
            return None;
        }
        if avail == 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let mut read = 0u32;
        // SAFETY: 只读 want 字节
        let want = (avail as usize).min(chunk.len());
        let ok = unsafe { ReadFile(h, Some(&mut chunk[..want]), Some(&mut read), None) }.is_ok();
        if !ok || read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read as usize]);
    }
}

/// EnumWindows 的收集上下文(回调与调用方共用)。
struct EnumCtx {
    pid: u32,
    out: Vec<windows::Win32::Foundation::HWND>,
}

/// 枚举属于指定 PID 的可见 mica 主窗(验证派发兑现)。
fn windows_of(pid: u32) -> Vec<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;

    let mut ctx = EnumCtx {
        pid,
        out: Vec::new(),
    };
    let ctx_ptr = &mut ctx as *mut EnumCtx;
    // SAFETY: 回调只写调用方栈上的 ctx;EnumWindows 同步完成
    unsafe {
        let _ = EnumWindows(
            Some(enumerate_proc as unsafe extern "system" fn(HWND, LPARAM) -> windows::core::BOOL),
            LPARAM(ctx_ptr as isize),
        );
    }
    ctx.out
}

unsafe extern "system" fn enumerate_proc(
    hwnd: windows::Win32::Foundation::HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowThreadProcessId, IsWindowVisible,
    };
    // SAFETY(Rust 2024):lparam 指向调用方栈上的 EnumCtx;按契约调用
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
    windows::core::BOOL(1)
}

/// 正常 activate 请求 → applied,响应回带请求 id。
#[test]
fn activate_gets_applied_with_matching_id() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)), "实例未就绪");
    let h = connect(Duration::from_secs(5)).expect("连接管道");
    let req = IpcRequest::new(4242, "activate");
    assert!(write_bytes(h, &encode_frame(&req)));
    let resp = read_response(h, Duration::from_secs(5)).expect("应答");
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
    assert_eq!(resp.id, 4242, "响应必须回带请求 id");
    assert_eq!(resp.status, IpcStatus::Applied);
}

/// 坏 JSON → 立即错误应答(bad_json),服务不死。
#[test]
fn bad_json_gets_error_response() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let h = connect(Duration::from_secs(5)).expect("连接");
    let mut frame = (8u32).to_le_bytes().to_vec();
    frame.extend_from_slice(b"not json");
    assert!(write_bytes(h, &frame));
    let resp = read_response(h, Duration::from_secs(8)).expect("坏帧也要应答");
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
    assert_eq!(resp.status, IpcStatus::Error);
    assert_eq!(resp.detail.as_deref(), Some("bad_json"));
    // 服务未死:后续正常请求照常
    let h2 = connect(Duration::from_secs(5)).expect("再连");
    let ok = write_bytes(h2, &encode_frame(&IpcRequest::new(9, "activate")));
    let resp2 = read_response(h2, Duration::from_secs(5));
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(h2).ok() };
    assert!(ok && resp2.is_some_and(|r| r.status == IpcStatus::Applied));
}

/// 超长帧(长度头 > 1 MiB)→ frame_too_large,不无限累积。
#[test]
fn overlong_frame_rejected() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let h = connect(Duration::from_secs(5)).expect("连接");
    let mut frame = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(b"{\"id\":1,\"op\":\"activate\"}");
    assert!(write_bytes(h, &frame));
    let resp = read_response(h, Duration::from_secs(8)).expect("超长帧要立即应答");
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
    assert_eq!(resp.status, IpcStatus::Error);
    assert_eq!(resp.detail.as_deref(), Some("frame_too_large"));
}

/// 未知 op → unknown_op,不入队。
#[test]
fn unknown_op_rejected() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let h = connect(Duration::from_secs(5)).expect("连接");
    assert!(write_bytes(
        h,
        &encode_frame(&IpcRequest::new(31, "explode"))
    ));
    let resp = read_response(h, Duration::from_secs(5)).expect("应答");
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
    assert_eq!(resp.id, 31);
    assert_eq!(resp.detail.as_deref(), Some("unknown_op"));
}

/// send-text 目标不存在 → bad_target(id 回带)。
#[test]
fn wrong_send_text_target_rejected() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let h = connect(Duration::from_secs(5)).expect("连接");
    let mut req = IpcRequest::new(77, "send-text");
    req.text = Some("hi".into());
    req.tab = Some(99);
    assert!(write_bytes(h, &encode_frame(&req)));
    let resp = read_response(h, Duration::from_secs(5)).expect("应答");
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
    assert_eq!(resp.id, 77);
    assert_eq!(resp.detail.as_deref(), Some("bad_target"));
}

/// 半帧延迟补齐 → 仍正常服务(NeedMore 路径)。
#[test]
fn partial_frame_completed_later() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let h = connect(Duration::from_secs(5)).expect("连接");
    let frame = encode_frame(&IpcRequest::new(55, "activate"));
    assert!(write_bytes(h, &frame[..frame.len() / 2]));
    std::thread::sleep(Duration::from_millis(300));
    assert!(write_bytes(h, &frame[frame.len() / 2..]));
    let resp = read_response(h, Duration::from_secs(5)).expect("补齐后应答");
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
    assert_eq!(resp.id, 55);
    assert_eq!(resp.status, IpcStatus::Applied);
}

/// 客户端半途断开 → 服务存活,后续请求正常(读期限兜底)。
#[test]
fn client_disconnect_mid_frame_keeps_server() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let h = connect(Duration::from_secs(5)).expect("连接");
    let frame = encode_frame(&IpcRequest::new(1, "activate"));
    assert!(write_bytes(h, &frame[..4])); // 只写长度头
    // SAFETY: 模拟客户端暴毙
    unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
    // 读期限(5s)内服务被占;期限后恢复。给足预算再探测
    std::thread::sleep(Duration::from_secs(6));
    let h2 = connect(Duration::from_secs(5)).expect("期限后应能再连");
    assert!(write_bytes(
        h2,
        &encode_frame(&IpcRequest::new(2, "activate"))
    ));
    let resp = read_response(h2, Duration::from_secs(5)).expect("应答");
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(h2).ok() };
    assert_eq!(resp.status, IpcStatus::Applied);
}

/// 100 并发客户端:全部被服务(单实例管道排队逐个处理),无饥饿。
/// 连接期限包含前方客户端的排队时间;单次响应收紧到 5s,不放宽
/// 生产端的 3s UI 应答期限。失败保留客户端 id、阶段和实测耗时。
#[test]
fn hundred_concurrent_clients_all_served() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    const CLIENTS: u32 = 100;
    let queue_budget = Duration::from_secs(20) + Duration::from_millis(250) * CLIENTS;
    let started = Instant::now();
    let deadline = started + queue_budget;
    let mut handles = Vec::new();
    for i in 0..u64::from(CLIENTS) {
        handles.push(std::thread::spawn(move || {
            let Some(h) = connect(deadline.saturating_duration_since(Instant::now())) else {
                return Err(format!("id={} connect timed out after {:?}", 1000 + i, started.elapsed()));
            };
            let connected_after = started.elapsed();
            let ok = write_bytes(h, &encode_frame(&IpcRequest::new(1000 + i, "activate")));
            let resp = if ok {
                read_response(h, Duration::from_secs(5).min(deadline.saturating_duration_since(Instant::now())))
            } else {
                None
            };
            // SAFETY: 线程内句柄
            unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
            match resp {
                Some(r) if r.id == 1000 + i && r.status == IpcStatus::Applied => Ok(()),
                other => Err(format!(
                    "id={} connected_after={connected_after:?} elapsed={:?} write_ok={ok} response={other:?}",
                    1000 + i, started.elapsed()
                )),
            }
        }));
    }
    let failures: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter_map(Result::err)
        .collect();
    eprintln!(
        "{CLIENTS} concurrent clients completed in {:?} (queue budget {queue_budget:?})",
        started.elapsed()
    );
    assert!(
        failures.is_empty(),
        "全部 {CLIENTS} 个并发客户端都应被服务; failures={failures:#?}"
    );
}

/// CLI 级:send 错目标 → 非零退出;新协议下错误真实可见。
#[test]
fn cli_reports_errors_nonzero() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let bad = Command::new(env!("CARGO_BIN_EXE_mica"))
        .args(["send", "--tab=99", "--literal", "x"])
        .env("USERNAME", test_user())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(bad.code(), Some(1), "错目标必须非零退出");
    let good = Command::new(env!("CARGO_BIN_EXE_mica"))
        .args(["send", "--literal", "x"])
        .env("USERNAME", test_user())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(good.code(), Some(0));
}

/// 复核 1:发请求后**保持连接但不读**的客户端不能卡死服务——写响应
/// 走有界收尾(宽限 ≤2s),后续正常客户端照常受理。
#[test]
fn non_reading_client_does_not_block_server() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    // 恶意客户端 A:发完请求就挂着,永不读响应
    let ha = connect(Duration::from_secs(5)).expect("A 连接");
    assert!(write_bytes(
        ha,
        &encode_frame(&IpcRequest::new(601, "activate"))
    ));
    // 不读;给服务端一点时间进入收尾宽限
    std::thread::sleep(Duration::from_millis(300));
    // 正常客户端 B:在宽限 + 服务能力范围内必须被受理(服务被 A 占用
    // 最多 = A 的收尾宽限 2s + B 自身服务时间)
    let start = Instant::now();
    let hb = connect(Duration::from_secs(15)).expect("B 应能连上(A 的宽限有限)");
    assert!(write_bytes(
        hb,
        &encode_frame(&IpcRequest::new(602, "activate"))
    ));
    let resp = read_response(hb, Duration::from_secs(10)).expect("B 必须得到应答");
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(hb).ok() };
    assert_eq!(resp.id, 602);
    assert_eq!(resp.status, IpcStatus::Applied);
    assert!(
        start.elapsed() < Duration::from_secs(12),
        "A(不读)最多占用其收尾宽限,B 不应被长期挡住"
    );
    // A 的连接在服务端 Disconnect 后读端断裂;清理
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(ha).ok() };
}

/// Creation now acknowledges the actual pane, including its stable identity.
#[test]
fn new_window_responds_applied_with_pane_metadata() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let h = connect(Duration::from_secs(5)).expect("连接");
    assert!(write_bytes(
        h,
        &encode_frame(&IpcRequest::new(701, "new-window"))
    ));
    let resp = read_response(h, Duration::from_secs(5)).expect("应答");
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
    assert_eq!(resp.id, 701);
    assert_eq!(resp.status, IpcStatus::Applied);
    let pane: mica_core::ipc::PaneInfo =
        serde_json::from_value(resp.data.expect("new pane metadata")).unwrap();
    assert!(pane.pane_id > 0);
    assert!(pane.pid.is_some());
    // 等副窗真的出现(派发兑现),再关掉它收尾:关最后窗口进程退出
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut two = false;
    while Instant::now() < deadline {
        let ws = windows_of(_app.0.id());
        if ws.len() >= 2 {
            two = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(two, "Applied must mean the second window exists");
}

/// 复核 6:连接的管道必须属于**自己启动的实例**——GetNamedPipeServerProcessId
/// 与子进程 PID 一致(防止探测/操作到用户已有实例)。
#[test]
fn server_pid_is_the_launched_instance() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let h = connect(Duration::from_secs(5)).expect("连接");
    let mut server_pid = 0u32;
    // SAFETY: 句柄为本测试连接
    let ok =
        unsafe { windows::Win32::System::Pipes::GetNamedPipeServerProcessId(h, &mut server_pid) }
            .is_ok();
    // SAFETY: 测试侧句柄
    unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
    assert!(ok, "GetNamedPipeServerProcessId 失败");
    assert_eq!(server_pid, app.0.id(), "管道服务端必须是本测试启动的实例");
}

fn exchange(request: &IpcRequest) -> IpcResponse {
    let pipe = connect(Duration::from_secs(5)).expect("connect isolated control pipe");
    assert!(write_bytes(pipe, &encode_frame(request)));
    let response = read_response(pipe, Duration::from_secs(6));
    unsafe { windows::Win32::Foundation::CloseHandle(pipe).ok() };
    response.expect("bounded control response")
}

fn pane_list() -> Vec<mica_core::ipc::PaneInfo> {
    let response = exchange(&IpcRequest::new(800, "list"));
    assert_eq!(response.status, IpcStatus::Applied, "{response:?}");
    serde_json::from_value(response.data.unwrap()).unwrap()
}

fn ordinary_pane_count() -> usize {
    pane_list()
        .into_iter()
        .filter(|pane| !pane.is_quick_terminal)
        .count()
}

#[test]
fn reopened_tab_restores_launch_env_and_registers_new_pane_owner() {
    use mica_core::ipc::{PaneInfo, ReadTextResult};
    use std::io::BufRead;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_KEYDOWN};
    let _guard = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated_config("keybind = insert=close_tab\nkeybind = home=reopen_tab\n");
    assert!(wait_ready(Duration::from_secs(20)));
    let mut create = IpcRequest::new(901, "new-tab");
    create.command = Some("cmd.exe /D /Q /K".into());
    create.env = Some(std::collections::BTreeMap::from([(
        "MICA_REOPEN_PROOF".into(),
        "retained".into(),
    )]));
    let response = exchange(&create);
    assert_eq!(response.status, IpcStatus::Applied);
    let original: PaneInfo = serde_json::from_value(response.data.unwrap()).unwrap();
    let mut watch = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_mica"))
            .args(["watch", "--events", "badge,lifecycle"])
            .env("USERNAME", test_user())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let output = watch.0.stdout.take().unwrap();
    let (events, received) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines() {
            let Ok(line) = line else {
                break;
            };
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            if events.send(event).is_err() {
                break;
            }
        }
    });
    // A real event is the subscription-ready barrier, not a startup sleep.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut badge = IpcRequest::new(900, "set-badge");
        badge.pane_id = Some(original.pane_id);
        badge.badge_state = Some(mica_core::ipc::BadgeState::Idle);
        assert_eq!(exchange(&badge).status, IpcStatus::Applied);
        if received
            .recv_timeout(Duration::from_millis(100))
            .is_ok_and(|e| e["type"] == "badge_changed")
        {
            break;
        }
        assert!(Instant::now() < deadline, "watch did not subscribe");
    }
    let hwnd = HWND(original.window_id as usize as _);
    unsafe {
        SendMessageW(hwnd, WM_KEYDOWN, Some(WPARAM(0x2d)), Some(LPARAM(0)));
    }
    assert!(!pane_list().iter().any(|p| p.pane_id == original.pane_id));
    unsafe {
        SendMessageW(hwnd, WM_KEYDOWN, Some(WPARAM(0x24)), Some(LPARAM(0)));
    }
    let reopened = pane_list()
        .into_iter()
        .find(|p| !p.is_quick_terminal && p.is_active_tab)
        .expect("reopened pane");
    assert_ne!(reopened.pane_id, original.pane_id);
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut closed, mut created) = (false, false);
    while !(closed && created) {
        if let Ok(event) = received.recv_timeout(Duration::from_millis(100)) {
            closed |= event["type"] == "pane_closed" && event["pane_id"] == original.pane_id;
            created |= event["type"] == "pane_created" && event["pane_id"] == reopened.pane_id;
        }
        assert!(
            Instant::now() < deadline,
            "closed={closed}, created={created}"
        );
    }
    let mut send = IpcRequest::new(902, "send-text");
    send.pane_id = Some(reopened.pane_id);
    send.text = Some("echo REOPEN_%MICA_PANE_ID%_%MICA_REOPEN_PROOF%\r".into());
    assert_eq!(
        exchange(&send).status,
        IpcStatus::Applied,
        "reopened pane must be routable"
    );
    let expected = format!("REOPEN_{}_retained", reopened.pane_id);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let mut read = IpcRequest::new(903, "read-text");
        read.pane_id = Some(reopened.pane_id);
        let response = exchange(&read);
        assert_eq!(response.status, IpcStatus::Applied);
        let snapshot: ReadTextResult = serde_json::from_value(response.data.unwrap()).unwrap();
        if snapshot.text.contains(&expected) {
            break;
        }
        assert!(Instant::now() < deadline, "{:?}", snapshot.text);
        std::thread::sleep(Duration::from_millis(75));
    }
    drop(watch);
    drop(received);
    reader.join().unwrap();
}

#[test]
fn restarting_pane_keeps_id_and_explicit_environment() {
    use mica_core::ipc::{PaneInfo, ReadTextResult};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_KEYDOWN};
    let _guard = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated_config("keybind = insert=restart_pane\n");
    assert!(wait_ready(Duration::from_secs(20)));
    let response = cli_output(&[
        "new-tab",
        "--command",
        "cmd.exe /D /Q /K",
        "--env",
        "MICA_RESTART_PROOF=kept",
        "--json",
    ]);
    assert!(response.status.success());
    let original: PaneInfo = serde_json::from_slice(&response.stdout).unwrap();
    unsafe {
        SendMessageW(
            HWND(original.window_id as usize as _),
            WM_KEYDOWN,
            Some(WPARAM(0x2d)),
            Some(LPARAM(0)),
        );
    }
    let restarted = pane_list()
        .into_iter()
        .find(|p| p.pane_id == original.pane_id)
        .unwrap();
    assert_ne!(restarted.pid, original.pid);
    assert_eq!(restarted.cwd, original.cwd);
    let target = format!("--pane={}", original.pane_id);
    assert!(
        cli_output(&[
            "send",
            &target,
            "echo RESTART_%MICA_PANE_ID%_%MICA_RESTART_PROOF%\\r"
        ])
        .status
        .success()
    );
    let expected = format!("RESTART_{}_kept", original.pane_id);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let output = cli_output(&["read", &target, "--json"]);
        assert!(output.status.success());
        let read: ReadTextResult = serde_json::from_slice(&output.stdout).unwrap();
        if read.text.contains(&expected) {
            break;
        }
        assert!(Instant::now() < deadline, "{:?}", read.text);
        std::thread::sleep(Duration::from_millis(75));
    }
}

fn cli_output(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mica"))
        .args(args)
        .env("USERNAME", test_user())
        .output()
        .expect("CLI invocation")
}

#[test]
fn pane_control_round_trip_preserves_identity_env_cwd_and_badges() {
    use mica_core::ipc::{BadgeState, PaneInfo, ReadTextResult};
    let _guard = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let cwd = std::env::temp_dir();
    let mut create = IpcRequest::new(801, "new-tab");
    create.command = Some("cmd.exe /Q /K".into());
    create.cwd = Some(cwd.to_string_lossy().into());
    // Raw clients cannot override the reserved name, including Windows casing.
    create.env = Some(std::collections::BTreeMap::from([
        ("MICA_TEST_VAR".into(), "42".into()),
        ("mica_pane_id".into(), "spoofed".into()),
    ]));
    let response = exchange(&create);
    assert_eq!(response.status, IpcStatus::Applied, "{response:?}");
    let pane: PaneInfo = serde_json::from_value(response.data.unwrap()).unwrap();
    assert_eq!(pane.cwd.as_deref(), create.cwd.as_deref());
    assert!(pane.pid.is_some());
    assert_eq!(ordinary_pane_count(), 2);
    let id = pane.pane_id;
    let mut send = IpcRequest::new(802, "send-text");
    send.pane_id = Some(id);
    send.text = Some("echo MICA_CTRL_%MICA_PANE_ID%_%MICA_TEST_VAR%\r".into());
    assert!(matches!(
        exchange(&send).status,
        IpcStatus::Applied | IpcStatus::Queued
    ));
    let expected = format!("MICA_CTRL_{id}_42");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let mut read = IpcRequest::new(803, "read-text");
        read.pane_id = Some(id);
        read.lines = Some(usize::MAX);
        read.include_scrollback = Some(true);
        let response = exchange(&read);
        assert_eq!(response.status, IpcStatus::Applied, "{response:?}");
        let snapshot: ReadTextResult = serde_json::from_value(response.data.unwrap()).unwrap();
        if snapshot.text.contains(&expected) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "missing {expected:?} in {:?}",
            snapshot.text
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut badge = IpcRequest::new(804, "set-badge");
    badge.pane_id = Some(id);
    badge.badge_state = Some(BadgeState::Attention);
    badge.badge_text = Some("等待审批".repeat(20));
    assert_eq!(exchange(&badge).status, IpcStatus::Applied);
    let marked = pane_list()
        .into_iter()
        .find(|p| p.pane_id == id)
        .unwrap()
        .badge
        .unwrap();
    assert_eq!(marked.text.unwrap().chars().count(), 32);
    let mut focus = IpcRequest::new(805, "focus");
    focus.pane_id = Some(id);
    assert_eq!(exchange(&focus).status, IpcStatus::Applied);
    assert!(
        pane_list()
            .into_iter()
            .find(|p| p.pane_id == id)
            .unwrap()
            .badge
            .is_none()
    );
    badge.badge_sticky = Some(true);
    assert_eq!(exchange(&badge).status, IpcStatus::Applied);
    assert_eq!(exchange(&focus).status, IpcStatus::Applied);
    assert!(
        pane_list()
            .into_iter()
            .find(|p| p.pane_id == id)
            .unwrap()
            .badge
            .unwrap()
            .sticky
    );
    // List all windows and address the original pane even when another window exists.
    let mut second = IpcRequest::new(806, "new-window");
    second.command = Some("cmd.exe /Q /K".into());
    let response = exchange(&second);
    assert_eq!(response.status, IpcStatus::Applied, "{response:?}");
    let other: PaneInfo = serde_json::from_value(response.data.unwrap()).unwrap();
    assert_ne!(other.window_id, pane.window_id);
    assert_ne!(other.pane_id, id);
    assert_eq!(ordinary_pane_count(), 3);
    assert_eq!(exchange(&focus).status, IpcStatus::Applied);
    let list = cli_output(&["list", "--json"]);
    assert!(list.status.success());
    assert_eq!(
        serde_json::from_slice::<Vec<PaneInfo>>(&list.stdout)
            .unwrap()
            .into_iter()
            .filter(|pane| !pane.is_quick_terminal)
            .count(),
        3
    );
    for op in ["focus", "send-text", "read-text", "set-badge"] {
        let mut bad = IpcRequest::new(807, op);
        bad.pane_id = Some(u64::MAX);
        bad.text = Some("x".into());
        bad.badge_state = Some(BadgeState::Idle);
        assert_eq!(exchange(&bad).detail.as_deref(), Some("bad_target"));
    }
    let before = ordinary_pane_count();
    create.cwd = Some(
        cwd.join("mica-directory-that-does-not-exist-9a3f")
            .to_string_lossy()
            .into(),
    );
    assert_eq!(exchange(&create).status, IpcStatus::Error);
    assert_eq!(ordinary_pane_count(), before);
}

#[test]
fn watch_delivers_ndjson_without_occupying_control_pipe() {
    use std::io::BufRead;
    let _guard = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let id = pane_list()[0].pane_id;
    let pane_arg = format!("--pane={id}");
    let mut watch = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_mica"))
            .args(["watch", &pane_arg, "--events", "badge,prompt,cwd"])
            .env("USERNAME", test_user())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = watch.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut received = None;
    while Instant::now() < deadline {
        let output = cli_output(&[
            "badge",
            &pane_arg,
            "--state",
            "running",
            "--text",
            "watch-test",
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!pane_list().is_empty());
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(150)) {
            received = Some(line);
            break;
        }
        // Toggle to guarantee a state change after the subscriber has connected.
        let _ = cli_output(&["badge", &pane_arg, "--state", "idle"]);
    }
    let value: serde_json::Value =
        serde_json::from_str(&received.expect("live badge event")).unwrap();
    assert_eq!(value["type"], "badge_changed");
    assert_eq!(value["pane_id"], id);
    // Emit an entire fast command lifecycle in a single write. The final idle
    // state alone must not erase its CommandEnd exit event.
    let script = "powershell.exe -NoLogo -NoProfile -Command \"[Console]::Write(([char]27)+']133;C'+[char]7+([char]27)+']133;D;0'+[char]7+([char]27)+']133;A'+[char]7+([char]27)+']133;B'+[char]7+([char]27)+']7;file:///C:/'+[char]7)\"\\r";
    assert!(cli_output(&["send", &pane_arg, script]).status.success());
    let deadline = Instant::now() + Duration::from_secs(12);
    let (mut exit, mut cwd) = (false, false);
    while Instant::now() < deadline && !(exit && cwd) {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            exit |= event["type"] == "prompt_state_changed" && event["exit_code"] == 0;
            cwd |= event["type"] == "cwd_changed";
        }
    }
    assert!(exit && cwd, "fast completion={exit}, cwd={cwd}");
    drop(watch);
    drop(rx);
    reader.join().unwrap();
}

#[test]
fn quick_terminal_has_identity_and_destroying_it_keeps_list_working() {
    use mica_core::ipc::{PaneInfo, ReadTextResult};
    let _guard = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let deadline = Instant::now() + Duration::from_secs(15);
    let qt: PaneInfo = loop {
        if let Some(pane) = pane_list().into_iter().find(|p| p.is_quick_terminal) {
            break pane;
        }
        assert!(Instant::now() < deadline, "QT must initialize");
        std::thread::sleep(Duration::from_millis(150));
    };
    let pane_arg = format!("--pane={}", qt.pane_id);
    assert!(
        cli_output(&["send", &pane_arg, "echo QT_%MICA_PANE_ID%\\r"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let output = cli_output(&["read", "--qt", "--json"]);
        assert!(output.status.success());
        let snapshot: ReadTextResult = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(snapshot.pane_id, qt.pane_id);
        if snapshot.text.contains(&format!("QT_{}", qt.pane_id)) {
            break;
        }
        assert!(Instant::now() < deadline, "{:?}", snapshot.text);
        std::thread::sleep(Duration::from_millis(100));
    }
    let hwnd = windows::Win32::Foundation::HWND(qt.window_id as usize as _);
    let created = cli_output(&["new-window", "--command", "cmd.exe /Q /K", "--json"]);
    assert!(created.status.success());
    let secondary: PaneInfo = serde_json::from_slice(&created.stdout).unwrap();
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(windows::Win32::Foundation::HWND(
                secondary.window_id as usize as _,
            )),
            windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        )
        .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let panes = pane_list();
        assert!(
            panes.iter().any(|p| p.pane_id == qt.pane_id),
            "closing a secondary window must preserve QT's owner"
        );
        if !panes.iter().any(|p| p.pane_id == secondary.pane_id) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    for _ in 0..2 {
        assert!(cli_output(&["focus", &pane_arg]).status.success());
        assert!(
            unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd) }.as_bool()
        );
    }
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        )
        .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !pane_list().iter().any(|p| p.pane_id == qt.pane_id) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "closed QT must leave the owner index"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(ordinary_pane_count(), 1);
}

#[test]
fn read_only_cli_without_instance_exits_instead_of_starting_gui() {
    let _guard = guard().lock().unwrap_or_else(|e| e.into_inner());
    let user = format!("micaabsent{}", std::process::id());
    for args in [
        vec!["list"],
        vec!["read"],
        vec!["watch"],
        vec!["focus", "--pane=1"],
        vec!["send", "x"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_mica"))
            .args(&args)
            .env("USERNAME", &user)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(1), "{args:?}");
    }
}

#[test]
fn first_instance_launch_returns_json_and_propagates_options() {
    let _guard = guard().lock().unwrap_or_else(|e| e.into_inner());
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let user = format!("micastart{unique}");
    *TEST_USER
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = user.clone();
    let iso = std::env::temp_dir().join(format!("mica-first-{unique}"));
    std::fs::create_dir_all(&iso).unwrap();
    write_test_config(&iso);
    let output = Command::new(env!("CARGO_BIN_EXE_mica"))
        .args(["new-tab", "--command", "cmd.exe /Q /K", "--cwd"])
        .arg(".")
        .current_dir(&iso)
        .args([
            "--env",
            "MICA_TEST_VAR=first",
            "--font-size",
            "12",
            "--json",
        ])
        .env("USERNAME", &user)
        .env("APPDATA", &iso)
        .env("LOCALAPPDATA", &iso)
        .env("MICA_CRASH_NOPROMPT", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "CLI status {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    // Own and clean up the detached server process too, even when assertions fail.
    let pipe = connect(Duration::from_secs(3)).expect("new first instance pipe");
    let mut pid = 0;
    unsafe {
        windows::Win32::System::Pipes::GetNamedPipeServerProcessId(pipe, &mut pid).unwrap();
        windows::Win32::Foundation::CloseHandle(pipe).ok();
    }
    struct StartedGui(u32);
    impl Drop for StartedGui {
        fn drop(&mut self) {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.0.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _gui = StartedGui(pid);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pane: mica_core::ipc::PaneInfo =
        serde_json::from_slice(&output.stdout).expect("first pane JSON");
    assert_eq!(pane.cwd.as_deref(), iso.to_str());
    assert_eq!(
        ordinary_pane_count(),
        1,
        "do not create an extra default pane"
    );
    let id = format!("--pane={}", pane.pane_id);
    assert!(
        cli_output(&["send", &id, "echo START_%MICA_PANE_ID%_%MICA_TEST_VAR%\\r"])
            .status
            .success()
    );
    let expected = format!("START_{}_first", pane.pane_id);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let result = cli_output(&["read", &id]);
        assert!(result.status.success());
        if String::from_utf8_lossy(&result.stdout).contains(&expected) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn concurrent_first_launches_share_an_instance_without_losing_a_pane() {
    let _guard = guard().lock().unwrap_or_else(|e| e.into_inner());
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let user = format!("micacold{unique}");
    *TEST_USER
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = user.clone();
    let iso = std::env::temp_dir().join(format!("mica-cold-{unique}"));
    std::fs::create_dir_all(&iso).unwrap();
    write_test_config(&iso);
    let mut clients = Vec::new();
    for label in ["one", "two"] {
        clients.push(
            Command::new(env!("CARGO_BIN_EXE_mica"))
                .args([
                    "new-tab",
                    "--command",
                    "cmd.exe /Q /K",
                    "--env",
                    &format!("MICA_TEST_VAR={label}"),
                    "--json",
                ])
                .env("USERNAME", &user)
                .env("APPDATA", &iso)
                .env("LOCALAPPDATA", &iso)
                .env("MICA_CRASH_NOPROMPT", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let outputs: Vec<_> = clients
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    let pipe = connect(Duration::from_secs(3)).expect("cold instance exists");
    let mut pid = 0;
    unsafe {
        windows::Win32::System::Pipes::GetNamedPipeServerProcessId(pipe, &mut pid).unwrap();
        windows::Win32::Foundation::CloseHandle(pipe).ok();
    }
    struct ColdGui(u32);
    impl Drop for ColdGui {
        fn drop(&mut self) {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.0.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _gui = ColdGui(pid);
    for output in &outputs {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let panes: Vec<mica_core::ipc::PaneInfo> = outputs
        .into_iter()
        .map(|o| serde_json::from_slice(&o.stdout).unwrap())
        .collect();
    assert_ne!(panes[0].pane_id, panes[1].pane_id);
    assert_eq!(panes[0].window_id, panes[1].window_id);
    assert_eq!(ordinary_pane_count(), 2);
}
