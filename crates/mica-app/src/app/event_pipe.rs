//! Windows named-pipe transport for the event bus.

use std::io::{BufRead, BufReader, Write};
use std::sync::Once;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, GetLastError, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
    FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, PeekNamedPipe,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{HSTRING, PCWSTR};

use super::{Bus, Filter, IpcError, MAX_WATCHERS, Ordering, Subscription, WatchEvent, bus};
use crate::app::ipc;

const WRITE_DEADLINE_MS: u32 = 1000;
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

fn pipe_name() -> String {
    ipc::pipe_name().replacen("mica-", "mica-events-", 1)
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this object exclusively owns the kernel handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Start only after the primary control-pipe instance has been acquired.
pub(crate) fn serve() {
    static START: Once = Once::new();
    START.call_once(|| {
        let Some(security) = ipc::build_user_acl() else {
            eprintln!("events: cannot build user ACL");
            return;
        };
        let name: HSTRING = pipe_name().into();
        for index in 0..MAX_WATCHERS {
            let first = if index == 0 {
                FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                Default::default()
            };
            // Duplex permits a non-consuming liveness probe. Clients open only
            // GENERIC_READ and no received data is ever dispatched as commands.
            // Eight fixed instances also bound writer/acceptor thread count.
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | first,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    MAX_WATCHERS as u32,
                    4096,
                    4096,
                    0,
                    Some(&security.sa),
                )
            };
            if handle.is_invalid() {
                eprintln!("events: pipe creation failed: {:?}", unsafe {
                    GetLastError()
                });
                break;
            }
            let raw = handle.0 as isize;
            if let Err(error) = std::thread::Builder::new()
                .name(format!("events-{index}"))
                .spawn(move || {
                    let handle = OwnedHandle(HANDLE(raw as _));
                    accept_loop(handle.0);
                })
            {
                // The closure was never started; retain ownership on failure.
                unsafe {
                    let _ = CloseHandle(handle);
                }
                eprintln!("events: cannot start writer: {error}");
                break;
            }
        }
    });
}

fn accept_loop(pipe: HANDLE) {
    loop {
        let Ok(event) = (unsafe { CreateEventW(None, true, false, None) }) else {
            return;
        };
        let event = OwnedHandle(event);
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        // SAFETY: OVERLAPPED/event remain alive until completion.
        let connection = unsafe { ConnectNamedPipe(pipe, Some(&mut overlapped)) };
        let connected = match connection {
            Ok(()) => true,
            Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => true,
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                let mut bytes = 0;
                unsafe { GetOverlappedResult(pipe, &overlapped, &mut bytes, true) }.is_ok()
            }
            Err(_) => false,
        };
        if !connected {
            unsafe {
                let _ = DisconnectNamedPipe(pipe);
            }
            std::thread::sleep(PROBE_INTERVAL);
            continue;
        }
        if let Some(subscription) = Bus::subscribe(bus()) {
            write_events(pipe, &subscription);
        }
        // No FlushFileBuffers: it would wait indefinitely for a reader.
        unsafe {
            let _ = DisconnectNamedPipe(pipe);
        }
    }
}

fn write_events(pipe: HANDLE, subscription: &Subscription) {
    loop {
        if subscription.cancelled.load(Ordering::Acquire) {
            return;
        }
        match subscription.receiver.recv_timeout(PROBE_INTERVAL) {
            Ok(line) => {
                if !write_line(pipe, line.as_bytes(), subscription) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                // Reclaim a disconnected idle subscriber even if no new event
                // is ever published. Peek never waits for incoming bytes.
                if unsafe { PeekNamedPipe(pipe, None, 0, None, None, None) }.is_err() {
                    return;
                }
            }
        }
    }
}

fn write_line(pipe: HANDLE, line: &[u8], subscription: &Subscription) -> bool {
    let Ok(event) = (unsafe { CreateEventW(None, true, false, None) }) else {
        return false;
    };
    let event = OwnedHandle(event);
    let mut overlapped = OVERLAPPED {
        hEvent: event.0,
        ..Default::default()
    };
    // SAFETY: buffer and OVERLAPPED live through completion/cancellation.
    let result = unsafe { WriteFile(pipe, Some(line), None, Some(&mut overlapped)) };
    match result {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
            let start = std::time::Instant::now();
            loop {
                if unsafe { WaitForSingleObject(event.0, 50) } == WAIT_OBJECT_0 {
                    break;
                }
                if start.elapsed().as_millis() >= WRITE_DEADLINE_MS as u128
                    || subscription.cancelled.load(Ordering::Acquire)
                {
                    // Per Microsoft's cancellation contract, cancelling is not
                    // completion. Retain the buffer/OVERLAPPED until the named
                    // pipe driver reports the cancelled operation complete.
                    unsafe {
                        let _ = CancelIoEx(pipe, Some(&overlapped));
                        let mut bytes = 0;
                        let _ = GetOverlappedResult(pipe, &overlapped, &mut bytes, true);
                    }
                    return false;
                }
            }
        }
        Err(_) => return false,
    }
    let mut written = 0;
    unsafe { GetOverlappedResult(pipe, &overlapped, &mut written, false) }.is_ok()
        && written as usize == line.len()
}

/// Read a stream without starting an application instance. A server shutdown
/// or a closed stdout (for example `mica watch | head`) is a clean end.
pub(crate) fn watch(pane_id: Option<u64>, events: Option<&str>) -> Result<(), IpcError> {
    let filter = Filter::new(pane_id, events)?;
    let name: HSTRING = pipe_name().into();
    let pipe = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        )
    }
    .map_err(|error| {
        if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
            IpcError::NoInstance
        } else if error.code() == ERROR_ACCESS_DENIED.to_hresult() {
            IpcError::AccessDenied
        } else if error.code() == ERROR_PIPE_BUSY.to_hresult() {
            IpcError::Busy
        } else {
            IpcError::Transport
        }
    })?;
    let pipe = OwnedHandle(pipe);
    static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    unsafe extern "system" fn interrupt(kind: u32) -> windows::core::BOOL {
        if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
            INTERRUPTED.store(true, Ordering::Release);
            true.into()
        } else {
            false.into()
        }
    }
    struct InterruptGuard;
    impl Drop for InterruptGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = SetConsoleCtrlHandler(Some(interrupt), false);
            }
        }
    }
    INTERRUPTED.store(false, Ordering::Release);
    // A detached GUI invocation may have no console; that should still read
    // the stream normally, with handle cleanup on process termination.
    let _interrupt = unsafe { SetConsoleCtrlHandler(Some(interrupt), true) }
        .ok()
        .map(|_| InterruptGuard);
    struct Reader(HANDLE);
    impl std::io::Read for Reader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            loop {
                if INTERRUPTED.load(Ordering::Acquire) {
                    return Ok(0);
                }
                let mut available = 0;
                if unsafe { PeekNamedPipe(self.0, None, 0, None, Some(&mut available), None) }
                    .is_err()
                {
                    return Ok(0);
                }
                if available == 0 {
                    std::thread::sleep(PROBE_INTERVAL);
                    continue;
                }
                let mut read = 0;
                let wanted = buffer.len().min(available as usize);
                if unsafe { ReadFile(self.0, Some(&mut buffer[..wanted]), Some(&mut read), None) }
                    .is_err()
                {
                    return Ok(0);
                }
                return Ok(read as usize);
            }
        }
    }
    let mut reader = BufReader::new(Reader(pipe.0));
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .map_err(|_| IpcError::Transport)?
            == 0
            || INTERRUPTED.load(Ordering::Acquire)
        {
            return Ok(());
        }
        let event: WatchEvent = serde_json::from_str(&line).map_err(|_| IpcError::BadJson)?;
        if filter.matches(&event)
            && let Err(error) = output
                .write_all(line.as_bytes())
                .and_then(|_| output.flush())
        {
            return if error.kind() == std::io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(IpcError::Transport)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn pair() -> (OwnedHandle, OwnedHandle) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let name: HSTRING = format!(
            r"\\.\pipe\mica-test-events-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
        .into();
        let security = ipc::build_user_acl().unwrap();
        let server = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                1024,
                1024,
                0,
                Some(&security.sa),
            )
        };
        assert!(!server.is_invalid());
        let client = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                FILE_GENERIC_READ.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        .unwrap();
        (OwnedHandle(server), OwnedHandle(client))
    }

    #[test]
    fn disconnected_idle_reader_releases_subscription_without_new_events() {
        let (server, client) = pair();
        let bus = Arc::new(Mutex::new(Bus::default()));
        let subscription = Bus::subscribe(&bus).unwrap();
        let (done, completion) = std::sync::mpsc::channel();
        let raw = server.0.0 as isize;
        let worker = std::thread::spawn(move || {
            write_events(HANDLE(raw as _), &subscription);
            drop(subscription);
            done.send(()).unwrap();
        });
        drop(client);
        completion
            .recv_timeout(Duration::from_secs(2))
            .expect("idle disconnect must reclaim");
        worker.join().unwrap();
        assert!(bus.lock().unwrap().subscribers.is_empty());
    }

    #[test]
    fn stalled_reader_write_is_cancelled_without_more_events() {
        let (server, _client) = pair();
        let bus = Arc::new(Mutex::new(Bus::default()));
        let subscription = Bus::subscribe(&bus).unwrap();
        let (done, completion) = std::sync::mpsc::channel();
        let raw = server.0.0 as isize;
        let worker = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            assert!(!write_line(
                HANDLE(raw as _),
                &vec![b'x'; 256 * 1024],
                &subscription
            ));
            drop(subscription);
            done.send(start.elapsed()).unwrap();
        });
        let elapsed = completion
            .recv_timeout(Duration::from_secs(4))
            .expect("write deadline must release stalled client");
        worker.join().unwrap();
        assert!(elapsed >= Duration::from_millis(WRITE_DEADLINE_MS as u64));
        assert!(bus.lock().unwrap().subscribers.is_empty());
    }
}
