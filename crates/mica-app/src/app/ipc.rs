//! named pipe IPC 服务端与客户端(M3a → 2026-11-02 T06 重造)。
//!
//! 单实例语义:首实例以 `FILE_FLAG_FIRST_PIPE_INSTANCE` 建管道;后来者
//! 建不成 → 转客户端(发请求收响应后退出)。
//!
//! T06 修复:
//! - **先入队再通知**:旧序先 Post 后 `tx.send`,UI 可能在消息入队前
//!   醒来读到空——现在请求先入通道再投递唤醒,投递失败由服务端自己
//!   回 `no_window` 错误。
//! - **真请求-响应**:每请求带 id,UI 处理后经单槽回复通道应答,服务端
//!   把响应写回管道(3s 应答期限);旧客户端"读到任意字节即成功"、
//!   服务端从不写 ACK 的假成功不复存在。
//! - **有界协议**:帧上限 1 MiB(core `MAX_FRAME_BYTES`),长度越界/
//!   坏 JSON/未知 op 立即拒绝并回错误帧;慢客户端读有 5s 期限,单个
//!   连接不能占死单实例服务。
//! - **句柄 RAII**:客户端所有早退路径不再泄漏管道句柄(旧 `?` 直接
//!   返回不 CloseHandle)。
//! - `ERROR_PIPE_CONNECTED`(客户端抢在 Connect 前连上)按成功处理,
//!   旧实现把它当错误直接停服。
//! - 管道名含用户名、DACL 显式仅本用户:同机他用户不能读写。

use std::sync::mpsc;
use std::time::{Duration, Instant};

use mica_core::ipc::{
    FrameDecode, IpcError, IpcRequest, IpcResponse, MAX_FRAME_BYTES, decode_frame, encode_frame,
};
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::FlushFileBuffers;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::IO::OVERLAPPED;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT, PeekNamedPipe, WaitNamedPipeW,
};
use windows::core::{HSTRING, PCWSTR};

use crate::app::window_registry;

/// 单客户端读期限:半帧慢客户端最长占用服务这么久。
const READ_DEADLINE: Duration = Duration::from_secs(5);
/// UI 应答期限:入队后 UI 未在此期限内回话,回 timeout(不承诺执行)。
const REPLY_DEADLINE: Duration = Duration::from_secs(3);

/// 管道名:按用户隔离(同机多用户互不干扰;单实例判定仍靠
/// FILE_FLAG_FIRST_PIPE_INSTANCE 的排他性)。
pub fn pipe_name() -> String {
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

/// 服务端启动结果:Ok(接收通道) = 本进程是首实例;
/// Err(true) = 管道已被占(已有实例);Err(false) = 其他错误(降级单开)。
pub fn serve() -> Result<mpsc::Receiver<IncomingRequest>, bool> {
    let wide: HSTRING = pipe_name().into();
    let sec = match build_user_acl() {
        Some(v) => v,
        None => {
            eprintln!("ipc: 本用户安全属性构建失败,拒绝以默认描述符开管(降级单开)");
            return Err(false);
        }
    };
    // SAFETY: 名字受控;首实例标志排他;DACL 仅本用户;sec 存活至建管返回
    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1, // 单实例:并发客户端在管道上排队,逐个快速服务
            4 * 1024,
            4 * 1024,
            0,
            Some(&sec.sa),
        )
    };
    drop(sec); // SD/DACL 内存随建管完成可释(SA 已被系统拷贝)
    // windows-rs 0.62 的 CreateNamedPipeW 直返 HANDLE;失败 = INVALID + GetLastError
    use windows::Win32::Foundation::GetLastError;
    let pipe = match pipe {
        h if h.is_invalid() => {
            let err = unsafe { GetLastError() };
            if err == ERROR_ACCESS_DENIED {
                return Err(true); // 已有实例
            }
            eprintln!("ipc: 管道创建失败({err:?});单实例降级关闭");
            return Err(false);
        }
        h => h,
    };
    let (tx, rx) = mpsc::channel::<IncomingRequest>();
    // HANDLE 不是 Send(包 *mut c_void)——裸 isize 过线程,同 SharedHwnd 纪律
    let raw = pipe.0 as isize;
    std::thread::Builder::new()
        .name("ipc-server".into())
        .spawn(move || {
            let pipe = HANDLE(raw as *mut std::ffi::c_void);
            accept_loop(pipe, tx);
            // SAFETY: 服务收尾关句柄
            unsafe { CloseHandle(pipe).ok() };
        })
        .expect("spawn ipc server");
    Ok(rx)
}

/// UI 侧拿到的请求:消息体 + 单槽回复通道。
pub struct IncomingRequest {
    pub req: IpcRequest,
    reply: mpsc::SyncSender<IpcResponse>,
}

impl IncomingRequest {
    /// UI 应答(非阻塞:服务端已断开时静默丢弃)。
    pub fn reply(&self, resp: IpcResponse) {
        let _ = self.reply.try_send(resp);
    }
}

fn accept_loop(pipe: HANDLE, tx: mpsc::Sender<IncomingRequest>) {
    loop {
        // SAFETY: 阻塞等待连接;ERROR_PIPE_CONNECTED = 客户端在 Connect
        // 之前已连上(合法竞态,当成功;旧实现当错误直接停服)
        let connected = unsafe { ConnectNamedPipe(pipe, None::<*mut OVERLAPPED>) };
        if connected.is_err()
            && unsafe { windows::Win32::Foundation::GetLastError() } != ERROR_PIPE_CONNECTED
        {
            eprintln!("ipc: ConnectNamedPipe 失败,服务停止");
            return;
        }
        serve_one_connection(pipe, &tx);
        // SAFETY: 断开当前客户端,回到 accept;句柄复用
        unsafe { DisconnectNamedPipe(pipe).ok() };
    }
}

/// 服务单个连接:读帧(带期限)→ 校验 → 入队+唤醒 → 等应答 → 写响应。
fn serve_one_connection(pipe: HANDLE, tx: &mpsc::Sender<IncomingRequest>) {
    // ---- 读一帧(增量 + 期限)----
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let req = match read_request(pipe, &mut buf) {
        Ok(r) => r,
        Err(e) => {
            write_response(pipe, &IpcResponse::error(0, e));
            return;
        }
    };

    // ---- 校验(未知 op 不入队,立即拒绝)----
    if !IpcRequest::is_known_op(&req.op) {
        write_response(pipe, &IpcResponse::error(req.id, IpcError::UnknownOp));
        return;
    }

    // ---- 入队 → 唤醒(顺序即修复:先 send 后 Post)----
    let (reply_tx, reply_rx) = mpsc::sync_channel::<IpcResponse>(1);
    if tx
        .send(IncomingRequest {
            req: req.clone(),
            reply: reply_tx,
        })
        .is_err()
    {
        write_response(pipe, &IpcResponse::error(req.id, IpcError::NoWindow));
        return;
    }
    let posted = window_registry::post_to_alive(
        crate::app::WM_APP_IPC,
        windows::Win32::Foundation::WPARAM(0),
        windows::Win32::Foundation::LPARAM(0),
    );
    if !posted {
        // 无存活窗口:已入队的请求稍后被 UI 排空时丢弃(reply 通道已断)
        eprintln!("ipc: 无存活窗口接收请求");
        write_response(pipe, &IpcResponse::error(req.id, IpcError::NoWindow));
        return;
    }

    // ---- 等应答(UI 处理后 reply;超时只承诺"未确认",不谎报成功)----
    let resp = match reply_rx.recv_timeout(REPLY_DEADLINE) {
        Ok(r) => r,
        Err(_) => IpcResponse::error(req.id, IpcError::Timeout),
    };
    write_response(pipe, &resp);
}

/// 增量读请求帧到完整/坏帧;期限到 → Transport(慢客户端不占死服务)。
fn read_request(pipe: HANDLE, buf: &mut Vec<u8>) -> Result<IpcRequest, IpcError> {
    let deadline = Instant::now() + READ_DEADLINE;
    let mut chunk = [0u8; 4096];
    loop {
        match decode_frame::<IpcRequest>(buf) {
            FrameDecode::NeedMore => {}
            FrameDecode::Complete(req, _) => return Ok(req),
            FrameDecode::Invalid(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            return Err(IpcError::Transport);
        }
        // SAFETY: Peek 判可读量,避免阻塞读卡期限
        let mut avail = 0u32;
        if unsafe { PeekNamedPipe(pipe, None, 0, None, Some(&mut avail), None) }.is_err() {
            return Err(IpcError::Transport); // 客户端已断开
        }
        if avail == 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let mut read = 0u32;
        let want = (avail as usize).min(chunk.len());
        // SAFETY: 只读 want 字节
        let ok = unsafe { ReadFile(pipe, Some(&mut chunk[..want]), Some(&mut read), None) }.is_ok();
        if !ok || read == 0 {
            return Err(IpcError::Transport);
        }
        buf.extend_from_slice(&chunk[..read as usize]);
        if buf.len() > MAX_FRAME_BYTES + 4 {
            return Err(IpcError::FrameTooLarge); // 长度头合法但发个不停的兜底
        }
    }
}

/// 写响应帧并冲刷(客户端立刻能读,不等断开)。
fn write_response(pipe: HANDLE, resp: &IpcResponse) {
    let frame = encode_frame(resp);
    let mut written = 0u32;
    // SAFETY: 缓冲与计数严格对应
    let _ = unsafe { WriteFile(pipe, Some(&frame), Some(&mut written), None) };
    // SAFETY: 冲刷让客户端读端立即见到
    let _ = unsafe { FlushFileBuffers(pipe) };
}

/// RAII 管道句柄(客户端):任何早退路径都不泄漏。
struct ClientPipe(HANDLE);
impl Drop for ClientPipe {
    fn drop(&mut self) {
        // SAFETY: 本侧打开的句柄
        unsafe { CloseHandle(self.0).ok() };
    }
}

/// 客户端:发请求收响应。超时毫秒(连接与应答共用预算)。
/// Ok = 服务端应答且 id 对应;Err = 传输失败/超时/坏帧。
pub fn send_request(req: &IpcRequest, timeout_ms: u32) -> Result<IpcResponse, IpcError> {
    let name: HSTRING = pipe_name().into();
    // SAFETY: WaitNamedPipeW 探测就绪(超时 → Transport,调用方决定自启)
    if !unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), timeout_ms) }.as_bool() {
        return Err(IpcError::Transport);
    }
    // SAFETY: 独占打开;RAII 保证早退不泄漏
    let pipe = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        )
    }
    .map_err(|_| IpcError::Transport)?;
    let _guard = ClientPipe(pipe);

    let frame = encode_frame(req);
    let mut written = 0u32;
    // SAFETY: 整帧写出
    unsafe { WriteFile(pipe, Some(&frame), Some(&mut written), None) }
        .map_err(|_| IpcError::Transport)?;
    if written as usize != frame.len() {
        return Err(IpcError::Transport); // 部分写按失败
    }

    // 读响应到完整帧(期限 = 预算)
    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; 4096];
    loop {
        match decode_frame::<IpcResponse>(&buf) {
            FrameDecode::NeedMore => {}
            FrameDecode::Complete(resp, _) => {
                if resp.id != req.id {
                    return Err(IpcError::Transport); // 应答与请求不对应
                }
                return Ok(resp);
            }
            FrameDecode::Invalid(_) => return Err(IpcError::BadJson),
        }
        if Instant::now() >= deadline {
            return Err(IpcError::Timeout);
        }
        let mut avail = 0u32;
        // SAFETY: Peek 判可读量
        if unsafe { PeekNamedPipe(pipe, None, 0, None, Some(&mut avail), None) }.is_err() {
            return Err(IpcError::Transport);
        }
        if avail == 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let mut read = 0u32;
        let want = (avail as usize).min(chunk.len());
        // SAFETY: 只读 want 字节
        let ok = unsafe { ReadFile(pipe, Some(&mut chunk[..want]), Some(&mut read), None) }.is_ok();
        if !ok || read == 0 {
            // 对端写完就断是合法收尾:拿已收字节判一次完整性再定论
            return match decode_frame::<IpcResponse>(&buf) {
                FrameDecode::Complete(resp, _) if resp.id == req.id => Ok(resp),
                _ => Err(IpcError::Transport),
            };
        }
        buf.extend_from_slice(&chunk[..read as usize]);
    }
}

/// 本用户专用 DACL:取进程令牌的用户 SID,仅授予该 SID 管道读写;
/// 其他任何主体(含隐式继承)不授。返回的 SECURITY_ATTRIBUTES 引用的
/// ACL 内存**有意随进程保留**:管道是进程级单例,只建一次,保留量 =
/// 一条 ACL(几十字节),换取指针无悬垂。
/// 本用户专用安全属性:进程令牌用户 SID → 仅授该 SID 读写权限的 DACL
/// → 装进合法 SECURITY_DESCRIPTOR(带版本头;裸 ACL 指针会被
/// CreateNamedPipeW 以 1305/unknown revision 拒绝,2026-11-02 实证)。
/// 返回值(sd/acl 的 Box + 借用它们的 SECURITY_ATTRIBUTES)必须整体
/// 存活到 CreateNamedPipeW 返回——管道是进程级单例,只建一次。
struct UserSecurity {
    _sd: Box<windows::Win32::Security::SECURITY_DESCRIPTOR>,
    _acl: *mut windows::Win32::Security::ACL,
    sa: windows::Win32::Security::SECURITY_ATTRIBUTES,
}

// ACL 原始指针经 Box 持有;仅主线程建管路径使用
unsafe impl Send for UserSecurity {}

fn build_user_acl() -> Option<UserSecurity> {
    use windows::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
    };
    use windows::Win32::Security::{
        ACL, GetTokenInformation, InitializeSecurityDescriptor, NO_INHERITANCE,
        PSECURITY_DESCRIPTOR, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, TOKEN_QUERY,
        TokenUser,
    };
    use windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: 全程按各 API 契约;令牌句柄用后即关
    unsafe {
        let mut sd = Box::new(SECURITY_DESCRIPTOR::default());
        if InitializeSecurityDescriptor(
            PSECURITY_DESCRIPTOR(sd.as_mut() as *mut _ as *mut core::ffi::c_void),
            SECURITY_DESCRIPTOR_REVISION,
        )
        .is_err()
        {
            return None;
        }
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return None;
        }
        // 两段式取 TokenUser:先问长度再取数据
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        if len == 0 || len as usize > 1024 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        if GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            len,
            &mut len,
        )
        .is_err()
        {
            let _ = CloseHandle(token);
            return None;
        }
        let _ = CloseHandle(token);
        let user = buf.as_ptr() as *const windows::Win32::Security::TOKEN_USER;
        let sid = (*user).User.Sid;

        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: windows::Win32::Security::Authorization::TRUSTEE_W {
                ptstrName: windows::core::PWSTR(sid.0 as *mut u16),
                TrusteeType: TRUSTEE_IS_USER,
                TrusteeForm: TRUSTEE_IS_SID,
                ..Default::default()
            },
        };
        let mut acl: *mut ACL = std::ptr::null_mut();
        if SetEntriesInAclW(Some(&[ea]), None, &mut acl).0 != 0 || acl.is_null() {
            return None;
        }
        if SetSecurityDescriptorDacl(
            PSECURITY_DESCRIPTOR(sd.as_mut() as *mut _ as *mut core::ffi::c_void),
            true,
            Some(acl),
            false,
        )
        .is_err()
        {
            windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
                acl as _,
            )));
            return None;
        }
        let sa = windows::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.as_mut() as *mut _ as *mut core::ffi::c_void,
            bInheritHandle: windows::Win32::Foundation::FALSE,
        };
        Some(UserSecurity {
            _sd: sd,
            _acl: acl,
            sa,
        })
    }
}
