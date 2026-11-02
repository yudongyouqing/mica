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
    let iso = std::env::temp_dir().join(format!(
        "mica-ipc-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&iso).unwrap();
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_mica"))
            .arg("--multi")
            .env("APPDATA", &iso)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch mica"),
    )
}

fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
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
            .args(["send", "--literal", "x"])
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
#[test]
fn hundred_concurrent_clients_all_served() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let mut handles = Vec::new();
    for i in 0..100u64 {
        handles.push(std::thread::spawn(move || {
            let Some(h) = connect(Duration::from_secs(20)) else {
                return false;
            };
            let ok = write_bytes(h, &encode_frame(&IpcRequest::new(1000 + i, "activate")));
            let resp = read_response(h, Duration::from_secs(20));
            // SAFETY: 线程内句柄
            unsafe { windows::Win32::Foundation::CloseHandle(h).ok() };
            ok && resp.is_some_and(|r| r.id == 1000 + i && r.status == IpcStatus::Applied)
        }));
    }
    let served = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|&ok| ok)
        .count();
    assert_eq!(served, 100, "全部并发客户端都应被服务");
}

/// CLI 级:send 错目标 → 非零退出;新协议下错误真实可见。
#[test]
fn cli_reports_errors_nonzero() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let _app = launch_isolated();
    assert!(wait_ready(Duration::from_secs(20)));
    let bad = Command::new(env!("CARGO_BIN_EXE_mica"))
        .args(["send", "--tab=99", "--literal", "x"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(bad.code(), Some(1), "错目标必须非零退出");
    let good = Command::new(env!("CARGO_BIN_EXE_mica"))
        .args(["send", "--literal", "x"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(good.code(), Some(0));
}
