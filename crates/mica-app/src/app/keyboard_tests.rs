//! Native key messages -> production handlers -> real ConPTY raw-input probe.
//! SetKeyboardState affects only this test thread; no global SendInput/focus.
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use std::io::Write;
use std::time::{Duration, Instant};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, GWLP_WNDPROC, SetWindowLongPtrW, UnregisterClassW,
};

#[path = "../../tests/support/process.rs"]
mod process;

pub(super) fn isolated(test: &str) -> bool {
    if std::env::var_os("MICA_KEY_TEST_WORKER").is_some() {
        return false;
    }
    use std::os::windows::process::CommandExt;
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("mica-key-routing-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("MICA_KEY_TEST_WORKER", "1")
        .env("USERNAME", format!("micakey{id}"))
        .env("APPDATA", &root)
        .env("LOCALAPPDATA", &root)
        .env("MICA_CRASH_NOPROMPT", "1")
        .creation_flags(0x08000000);
    let result = process::run_with_deadline(&mut command, Duration::from_secs(45), &root).unwrap();
    assert!(
        result.status.success(),
        "native key worker failed: {}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    true
}

unsafe extern "system" fn default_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, w, l)
}

pub(super) struct KeyboardState([u8; 256]);
impl KeyboardState {
    pub(super) fn new() -> Self {
        let mut keys = [0; 256];
        unsafe {
            GetKeyboardState(&mut keys).unwrap();
        }
        Self(keys)
    }
    pub(super) fn set(&self, shift: bool, ctrl: bool) {
        let mut keys = [0; 256];
        keys[VK_SHIFT.0 as usize] = if shift { 0x80 } else { 0 };
        keys[VK_CONTROL.0 as usize] = if ctrl { 0x80 } else { 0 };
        unsafe {
            SetKeyboardState(&keys).unwrap();
        }
    }
}
impl Drop for KeyboardState {
    fn drop(&mut self) {
        unsafe {
            let _ = SetKeyboardState(&self.0);
        }
    }
}

pub(super) fn probe_command() -> String {
    format!(
        "\"{}\" --exact app::keyboard_tests::tab_input_child --nocapture --test-threads=1",
        std::env::current_exe().unwrap().display()
    )
}

#[test]
fn tab_input_child() {
    if !std::env::args().any(|a| a == "app::keyboard_tests::tab_input_child") {
        return;
    }
    use windows::Win32::Storage::FileSystem::ReadFile;
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetStdHandle,
        STD_INPUT_HANDLE, SetConsoleMode,
    };
    unsafe {
        let input = GetStdHandle(STD_INPUT_HANDLE).unwrap();
        let mut old = CONSOLE_MODE::default();
        // An ordinary cargo invocation is not the owned ConPTY child.
        if GetConsoleMode(input, &mut old).is_err() {
            return;
        }
        SetConsoleMode(input, ENABLE_VIRTUAL_TERMINAL_INPUT).unwrap();
        print!("\r\nMICA_KEY_READY\r\n");
        std::io::stdout().flush().unwrap();
        let mut captured = Vec::new();
        'input: loop {
            let mut buffer = [0u8; 64];
            let mut count = 0;
            ReadFile(input, Some(&mut buffer), Some(&mut count), None).unwrap();
            assert!(count > 0);
            for byte in &buffer[..count as usize] {
                if *byte == 4 {
                    break 'input;
                }
                captured.push(*byte);
                assert!(captured.len() <= 64);
            }
        }
        SetConsoleMode(input, old).unwrap();
        let hex: String = captured.iter().map(|b| format!("{b:02X}")).collect();
        print!("\r\nMICA_KEY_BYTES={hex}\r\nMICA_KEY_DONE\r\n");
        std::io::stdout().flush().unwrap();
    }
}

struct MainFixture {
    hwnd: HWND,
    original_proc: isize,
    class: HSTRING,
    instance: HINSTANCE,
    reader: mica_core::pty::PtyReader,
    output: Vec<u8>,
}
impl MainFixture {
    unsafe fn new() -> Self {
        let settings = Settings::default();
        seed_tls_for_tests(&settings);
        KEYMAP.with(|k| *k.borrow_mut() = settings.keymap.clone());
        MOUSE_HIDE_ON_TYPE.set(false);
        KEYMAP_ATE_CHAR.set(false);
        SEARCH_OPEN.set(false);
        RENAME_OPEN.set(false);
        ACTIVE.set(0);
        let class: HSTRING = "mica-key-message-fixture".into();
        let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            hInstance: instance,
            lpszClassName: PCWSTR(class.as_ptr()),
            lpfnWndProc: Some(default_proc),
            ..Default::default()
        };
        assert_ne!(RegisterClassExW(&wc), 0);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &class,
            w!("owned keyboard fixture"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            800,
            500,
            None,
            None,
            Some(instance),
            None,
        )
        .unwrap();
        let (pane, reader) = create_pane(hwnd, &probe_command()).unwrap();
        let id = pane.id;
        TABS.with(|tabs| {
            *tabs.borrow_mut() = vec![TabState {
                title: "input probe".into(),
                color: None,
                dirty: false,
                panes: vec![pane],
                layout: mica_core::layout::Layout::leaf(id),
                focused: 0,
            }]
        });
        let original_proc =
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, super::wndproc as *const () as isize);
        assert_ne!(original_proc, 0);
        Self {
            hwnd,
            original_proc,
            class,
            instance,
            reader,
            output: Vec::new(),
        }
    }
    fn until(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !String::from_utf8_lossy(&self.output).contains(marker) {
            assert!(
                Instant::now() < deadline,
                "ConPTY probe timeout: {}",
                String::from_utf8_lossy(&self.output)
            );
            match self.reader.recv_event(Duration::from_millis(50)) {
                mica_core::pty::RecvEvent::Data(data) => {
                    self.output.extend_from_slice(&data);
                    TABS.with(|tabs| {
                        let mut tabs = tabs.borrow_mut();
                        let terminal = &mut tabs[0].panes[0].terminal;
                        terminal.term.feed(&data);
                        for reply in terminal.term.take_pty_writes() {
                            let _ = stage_input(terminal, reply.as_bytes());
                        }
                    });
                }
                mica_core::pty::RecvEvent::Timeout => {}
                mica_core::pty::RecvEvent::Closed => panic!(
                    "ConPTY probe closed: {}",
                    String::from_utf8_lossy(&self.output)
                ),
            }
        }
    }
}
impl Drop for MainFixture {
    fn drop(&mut self) {
        let tabs = TABS.with(|tabs| tabs.replace(Vec::new()));
        for tab in tabs {
            for pane in tab.panes {
                lifecycle::close_terminal(pane.terminal);
            }
        }
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_WNDPROC, self.original_proc);
            let _ = DestroyWindow(self.hwnd);
            let _ = UnregisterClassW(&self.class, Some(self.instance));
        }
        MOUSE_HIDE_ON_TYPE.set(true);
    }
}

pub(super) unsafe fn send_tab(hwnd: HWND, keyboard: &KeyboardState, shift: bool, repeat: bool) {
    keyboard.set(shift, false);
    SendMessageW(
        hwnd,
        WM_KEYDOWN,
        Some(WPARAM(0x09)),
        Some(LPARAM(1 | if repeat { 1 << 30 } else { 0 })),
    );
    // Character delivery can follow a rapid Shift release. Dedup must not
    // depend on the current modifier state at WM_CHAR time.
    keyboard.set(false, false);
    SendMessageW(hwnd, WM_CHAR, Some(WPARAM(0x09)), Some(LPARAM(1)));
}

#[test]
fn main_tab_messages_reach_real_pty_once_with_backtab_modifiers() {
    if isolated("app::keyboard_tests::main_tab_messages_reach_real_pty_once_with_backtab_modifiers")
    {
        return;
    }
    let keyboard = KeyboardState::new();
    let mut fixture = unsafe { MainFixture::new() };
    fixture.until("MICA_KEY_READY");
    unsafe {
        send_tab(fixture.hwnd, &keyboard, false, false);
        send_tab(fixture.hwnd, &keyboard, true, false);
        send_tab(fixture.hwnd, &keyboard, true, true);
    }
    TABS.with(|tabs| {
        let _ = stage_input(&mut tabs.borrow_mut()[0].panes[0].terminal, b"\x04");
    });
    fixture.until("MICA_KEY_DONE");
    let received = String::from_utf8_lossy(&fixture.output);
    assert!(
        received.contains("MICA_KEY_BYTES=091B5B5A1B5B5A"),
        "wrong native Tab stream: {received}"
    );
    // The real ConPTY phase is complete. A queue-only sink now verifies modes
    // ConPTY may translate, without claiming those bytes reached a child.
    TABS.with(|tabs| {
        let mut tabs = tabs.borrow_mut();
        let terminal = &mut tabs[0].panes[0].terminal;
        let previous = std::mem::replace(&mut terminal.writeq, WriteQueue::new());
        previous.close();
    });
    keyboard.set(true, true);
    unsafe {
        SendMessageW(
            fixture.hwnd,
            WM_KEYDOWN,
            Some(WPARAM(0x09)),
            Some(LPARAM(1)),
        );
        SendMessageW(fixture.hwnd, WM_CHAR, Some(WPARAM(0x09)), Some(LPARAM(1)));
    }
    let pending = || TABS.with(|tabs| tabs.borrow()[0].panes[0].terminal.writeq.pending_for_test());
    assert!(
        pending().is_empty(),
        "default Ctrl+Shift+Tab must not leak Tab bytes: {:?}",
        pending()
    );
    keyboard.set(false, true);
    unsafe {
        SendMessageW(
            fixture.hwnd,
            WM_KEYDOWN,
            Some(WPARAM(0x09)),
            Some(LPARAM(1)),
        );
        SendMessageW(fixture.hwnd, WM_CHAR, Some(WPARAM(0x09)), Some(LPARAM(1)));
    }
    assert!(
        pending().is_empty(),
        "default Ctrl+Tab must not leak Tab bytes"
    );
    TABS.with(|tabs| {
        let mut tabs = tabs.borrow_mut();
        let terminal = &mut tabs[0].panes[0].terminal;
        terminal.term.feed(b"\x1b[>1u");
        assert!(
            terminal
                .term
                .kitty_flags()
                .contains(mica_core::protocol::kitty::KittyFlags::DISAMBIGUATE)
        );
    });
    unsafe {
        send_tab(fixture.hwnd, &keyboard, true, false);
    }
    assert_eq!(pending(), b"\x1b[9;2u");
    SEARCH_OPEN.set(true);
    unsafe {
        send_tab(fixture.hwnd, &keyboard, false, false);
    }
    SEARCH_OPEN.set(false);
    RENAME_OPEN.set(true);
    unsafe {
        send_tab(fixture.hwnd, &keyboard, false, false);
    }
    RENAME_OPEN.set(false);
    assert_eq!(
        pending(),
        b"\x1b[9;2u",
        "Tab must remain owned by focused search/rename editors"
    );
}

#[test]
fn production_virtual_key_route_encodes_backtab() {
    let shift = Mods {
        shift: true,
        ..Default::default()
    };
    assert_eq!(
        vkey_bytes(0x09, shift, false).as_deref(),
        Some(b"\x1b[Z".as_slice())
    );
}
