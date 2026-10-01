//! named pipe IPC 服务端与客户端(M3a,spec §4)。
//!
//! 单实例语义:首实例以 `FILE_FLAG_FIRST_PIPE_INSTANCE` 建管道;后来者
//! 建不成 → 转客户端(发 activate/new-tab 后退出)。协议见 mica-core::ipc。

use std::sync::mpsc;

use mica_core::ipc::{IpcMessage, encode, take_frame};
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::IO::OVERLAPPED;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT, WaitNamedPipeW,
};
use windows::core::{HSTRING, PCWSTR};

use crate::app::SharedHwnd;

/// 管道名(单用户单机)。
pub const PIPE_NAME: &str = r"\\.\pipe\mica";

/// 服务端启动结果:Ok(句柄+接收线程) = 本进程是首实例;
/// Err(true) = 管道已被占(已有实例);Err(false) = 其他错误(降级单开,不致命)。
pub fn serve(hwnd_slot: SharedHwnd) -> Result<mpsc::Receiver<IpcMessage>, bool> {
    let wide: HSTRING = PIPE_NAME.into();
    // SAFETY: 名字常量;首实例标志排他
    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1, // 单实例
            4 * 1024,
            4 * 1024,
            0,
            None,
        )
    };
    // windows-rs 0.62 的 CreateNamedPipeW 直返 HANDLE(无 Result);
    // 失败 = INVALID_HANDLE + GetLastError
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
    let (tx, rx) = mpsc::channel::<IpcMessage>();
    // HANDLE 不是 Send(包 *mut c_void)——裸 isize 过线程,同 SharedHwnd 纪律
    let raw = pipe.0 as isize;
    // accept 循环线程:阻塞 ConnectNamedPipe,逐连接读帧
    std::thread::Builder::new()
        .name("ipc-server".into())
        .spawn(move || {
            let pipe = HANDLE(raw as *mut std::ffi::c_void);
            accept_loop(pipe, tx, hwnd_slot)
        })
        .expect("spawn ipc server");
    Ok(rx)
}

fn accept_loop(pipe: HANDLE, tx: mpsc::Sender<IpcMessage>, hwnd_slot: SharedHwnd) {
    loop {
        // SAFETY: 阻塞等待连接;句柄由本线程独占复用
        if unsafe { ConnectNamedPipe(pipe, None::<*mut OVERLAPPED>) }.is_err() {
            eprintln!("ipc: ConnectNamedPipe 失败,服务停止");
            return;
        }
        let msg = read_one_message(pipe);
        if let Some(msg) = msg {
            // 唤醒主线程:哨兵拿 HWND 投递 WM_APP_IPC(profile 字符串经 Box
            // 转移所有权,主线程必须收——见 wndproc 的 WM_APP_IPC)
            let hwnd = *hwnd_slot.lock().expect("hwnd slot poisoned");
            if let Some(raw) = hwnd {
                let hwnd = windows::Win32::Foundation::HWND(raw as *mut std::ffi::c_void);
                let _ = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                        Some(hwnd),
                        crate::app::WM_APP_IPC,
                        windows::Win32::Foundation::WPARAM(0),
                        windows::Win32::Foundation::LPARAM(0),
                    )
                };
            }
            if tx.send(msg).is_err() {
                // 主线程已收摊(退出序):停止服务
                // SAFETY: 句柄收尾
                unsafe { CloseHandle(pipe).ok() };
                return;
            }
        }
        // SAFETY: 断开当前客户端,回到 accept
        unsafe { DisconnectNamedPipe(pipe).ok() };
    }
}

/// 一个连接内读一帧(长度前缀 + payload)。
fn read_one_message(pipe: HANDLE) -> Option<IpcMessage> {
    use windows::Win32::Storage::FileSystem::ReadFile;
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 512];
    loop {
        let mut read = 0u32;
        // SAFETY: 缓冲与计数严格对应
        let ok = unsafe {
            ReadFile(pipe, Some(&mut chunk), Some(&mut read), None)
                .map_err(|e| {
                    use windows::Win32::Foundation::WIN32_ERROR;
                    let code = WIN32_ERROR(e.code().0 as u32);
                    if code == ERROR_BROKEN_PIPE || code == ERROR_ACCESS_DENIED {
                        // 客户端断开:解析已收到的部分(半帧自然被 take_frame 拒)
                    } else {
                        eprintln!("ipc: ReadFile 失败 {e}");
                    }
                })
                .is_ok()
        };
        if read > 0 {
            buf.extend_from_slice(&chunk[..read as usize]);
            if let Some((msg, _)) = take_frame(&buf) {
                return Some(msg);
            }
        }
        if !ok || read == 0 {
            return None; // 对端关闭/错误:本连接结束
        }
    }
}

/// 客户端:连管道发一帧收一帧。超时毫秒。
/// Ok = 服务端已受理;Err = 连不上(调用方决定自己启动还是报错)。
pub fn send(msg: &IpcMessage, timeout_ms: u32) -> Result<(), ()> {
    let name: HSTRING = PIPE_NAME.into();
    // SAFETY: WaitNamedPipeW 探测就绪(超时 → Err 自己启动)
    if !unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), timeout_ms) }.as_bool() {
        return Err(());
    }
    let wide: HSTRING = PIPE_NAME.into();
    // SAFETY: 管道名打开(独占;首实例独占不受影响——连接与创建分道)
    let pipe = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        )
    }
    .map_err(|_| ())?;
    use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    let frame = encode(msg);
    // SAFETY: 帧写全
    unsafe { WriteFile(pipe, Some(&frame), None, None).map_err(|_| ())? };
    // 读响应(8 字节 {"ok":true} 级别;内容不敏感,读到即可)
    let mut resp = [0u8; 64];
    let _ = unsafe { ReadFile(pipe, Some(&mut resp), None, None) };
    // SAFETY: 句柄关闭
    unsafe { CloseHandle(pipe).ok() };
    Ok(())
}
