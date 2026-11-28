//! Event CLI transport checks use their own pipe identity and never start a GUI session.
#![cfg(windows)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, WriteFile};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{HSTRING, PCWSTR};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write(pipe: HANDLE, bytes: &[u8]) {
    let event = Handle(unsafe { CreateEventW(None, true, false, None) }.unwrap());
    let mut operation = OVERLAPPED {
        hEvent: event.0,
        ..Default::default()
    };
    let result = unsafe { WriteFile(pipe, Some(bytes), None, Some(&mut operation)) };
    if let Err(error) = result {
        assert_eq!(
            error.code(),
            windows::Win32::Foundation::ERROR_IO_PENDING.to_hresult()
        );
    }
    let mut written = 0;
    unsafe { GetOverlappedResult(pipe, &operation, &mut written, true) }.unwrap();
    assert_eq!(written as usize, bytes.len());
}

fn watch_pair(label: &str) -> (Handle, ChildGuard) {
    let user = format!("eventwatch{label}{}", std::process::id());
    let name: HSTRING = format!(r"\\.\pipe\mica-events-{user}").into();
    let server = Handle(unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            None,
        )
    });
    assert!(!server.0.is_invalid());
    let child = ChildGuard(
        // Also exercise the GUI-subsystem release binary without replacing the
        // installed application or the debug test executable.
        Command::new(
            std::env::var_os("MICA_TEST_WATCH_BINARY")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_mica").into()),
        )
        .arg("watch")
        .env("USERNAME", &user)
        .env("APPDATA", std::env::temp_dir().join(&user))
        .env("LOCALAPPDATA", std::env::temp_dir().join(&user))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(),
    );
    // The first successful client open may precede ConnectNamedPipe.
    let event = Handle(unsafe { CreateEventW(None, true, false, None) }.unwrap());
    let mut operation = OVERLAPPED {
        hEvent: event.0,
        ..Default::default()
    };
    match unsafe { ConnectNamedPipe(server.0, Some(&mut operation)) } {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
        Err(error) => {
            assert_eq!(
                error.code(),
                windows::Win32::Foundation::ERROR_IO_PENDING.to_hresult()
            );
            let mut bytes = 0;
            if unsafe { WaitForSingleObject(event.0, 3000) } != WAIT_OBJECT_0 {
                unsafe {
                    let _ = CancelIoEx(server.0, Some(&operation));
                    let _ = GetOverlappedResult(server.0, &operation, &mut bytes, true);
                }
                panic!("watch did not connect to its isolated event pipe");
            }
            unsafe { GetOverlappedResult(server.0, &operation, &mut bytes, true) }.unwrap();
        }
    }
    (server, child)
}

#[test]
fn server_disconnect_mid_record_is_a_clean_cli_end() {
    assert_partial_disconnect_is_clean(
        "partial",
        br#"{"type":"title_changed","pane_id":1,"title":"partial"#,
    );
}

#[test]
fn server_disconnect_mid_utf8_is_a_clean_cli_end() {
    let mut fragment = br#"{"type":"title_changed","pane_id":1,"title":""#.to_vec();
    fragment.extend_from_slice(&[0xf0, 0x9f]); // Half of a four-byte UTF-8 scalar.
    assert_partial_disconnect_is_clean("utf8", &fragment);
}

fn assert_partial_disconnect_is_clean(label: &str, fragment: &[u8]) {
    let (server, mut child) = watch_pair(label);
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if send.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let complete = r#"{"type":"title_changed","pane_id":1,"title":"complete"}"#;
    write(server.0, format!("{complete}\n").as_bytes());
    assert_eq!(
        receive.recv_timeout(Duration::from_secs(3)).unwrap(),
        complete
    );
    write(server.0, fragment);
    // Let Reader consume the fragment before closing its underlying pipe.
    std::thread::sleep(Duration::from_millis(300));
    drop(server);
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "watch must finish after server disconnect"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    reader.join().unwrap();
    assert!(
        status.success(),
        "watch failed on server shutdown: {status}"
    );
    assert!(
        receive.try_recv().is_err(),
        "partial record must not reach stdout"
    );
}

#[test]
fn ctrl_c_exits_even_when_stdout_is_blocked() {
    use windows::Win32::System::Threading::{
        CREATE_NEW_CONSOLE, CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION,
        STARTF_USESHOWWINDOW, STARTUPINFOW, TerminateProcess,
    };
    use windows::core::PWSTR;

    // Never broadcast a console event in the test runner's/user's console.
    // A hidden helper gets a new private console, verifies it owns that
    // console, and starts only its watch child there.
    let executable = std::env::current_exe().unwrap();
    let command = format!(
        "\"{}\" --exact ctrl_c_private_console_helper --ignored --nocapture",
        executable.display()
    );
    let mut command: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: 0, // SW_HIDE
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            None,
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE,
            None,
            None,
            &startup,
            &mut process,
        )
    }
    .unwrap();
    let process_handle = Handle(process.hProcess);
    let _thread_handle = Handle(process.hThread);
    if unsafe { WaitForSingleObject(process_handle.0, 12_000) } != WAIT_OBJECT_0 {
        unsafe {
            let _ = TerminateProcess(process_handle.0, 99);
        }
        panic!("private-console Ctrl+C regression timed out");
    }
    let mut exit = 99;
    unsafe { GetExitCodeProcess(process_handle.0, &mut exit) }.unwrap();
    assert_eq!(exit, 0, "private-console Ctrl+C regression failed");
}

#[test]
#[ignore = "runs only in the hidden private-console helper process"]
fn ctrl_c_private_console_helper() {
    use windows::Win32::System::Console::{
        CTRL_C_EVENT, GenerateConsoleCtrlEvent, GetConsoleProcessList, SetConsoleCtrlHandler,
    };
    unsafe extern "system" fn ignore_signal(_: u32) -> windows::core::BOOL {
        true.into()
    }
    let mut console_members = [0u32; 16];
    let count = unsafe { GetConsoleProcessList(&mut console_members) };
    assert_eq!(count, 1, "refuse to signal a shared console");
    assert_eq!(console_members[0], std::process::id());
    unsafe { SetConsoleCtrlHandler(Some(ignore_signal), true) }.unwrap();

    let (server, mut child) = watch_pair("ctrl");
    // Keep stdout open but never read it. A maximum-length Unicode title fits
    // the server's 4096-character limit but exceeds the stdout pipe buffer.
    let line = format!(
        "{{\"type\":\"title_changed\",\"pane_id\":1,\"title\":\"{}\"}}\n",
        "界".repeat(4096)
    );
    write(server.0, line.as_bytes());
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.0.try_wait().unwrap().is_none());
    let count = unsafe { GetConsoleProcessList(&mut console_members) };
    assert_eq!(
        count, 2,
        "private console must contain only helper and watch"
    );
    assert!(console_members[..2].contains(&child.0.id()));
    assert!(console_members[..2].contains(&std::process::id()));
    unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) }.unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "Ctrl+C must interrupt blocked stdout"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(0));
    unsafe { SetConsoleCtrlHandler(Some(ignore_signal), false) }.unwrap();
}
