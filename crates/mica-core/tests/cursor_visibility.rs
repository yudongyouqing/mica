//! A real ConPTY stream must preserve hide/move/show transitions.
#![cfg(windows)]

use mica_core::pty::{PtyReader, PtySession};
use mica_core::surface::{ScreenSize, Surface};
use portable_pty::CommandBuilder;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct PhaseFile(PathBuf);
impl Drop for PhaseFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("phase2"));
    }
}

fn receive_marker(session: &PtySession, reader: &PtyReader, surface: &mut Surface, marker: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut bytes = Vec::new();
    while Instant::now() < deadline {
        if let Some(chunk) = reader.recv_timeout(Duration::from_millis(100)) {
            surface.feed(&chunk);
            bytes.extend_from_slice(&chunk);
            for reply in surface.take_pty_writes() {
                session
                    .write(reply.as_bytes())
                    .expect("terminal query reply");
            }
            if bytes.windows(marker.len()).any(|part| part == marker) {
                return;
            }
        }
    }
    panic!(
        "missing marker {:?}: {}",
        String::from_utf8_lossy(marker),
        String::from_utf8_lossy(&bytes)
    );
}

#[test]
fn conpty_keeps_thinking_cursor_hidden_until_explicit_show() {
    use windows_sys::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, SetDefaultDllDirectories,
    };
    // This integration-test executable owns its DLL policy. Exclude PATH/CWD
    // overrides so portable-pty cannot pick a third-party conpty.dll there.
    // SAFETY: called before the only PTY spawn in this isolated test process.
    assert_ne!(
        unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) },
        0,
        "DLL search policy: {}",
        std::io::Error::last_os_error()
    );
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let phase = PhaseFile(
        std::env::temp_dir().join(format!("mica-cursor-{}-{unique}.phase", std::process::id())),
    );
    let mut command = CommandBuilder::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "$e=[string][char]27; $p=$env:MICA_CURSOR_PHASE_FILE; $p2=[IO.Path]::ChangeExtension($p,'phase2'); \
         [Console]::Write($e+'[?25l'+$e+'[2;2H'+'HIDDEN_A'); \
         while(-not [IO.File]::Exists($p)){Start-Sleep -Milliseconds 5}; \
         [Console]::Write($e+'[4;5H'+'HIDDEN_B'); \
         while(-not [IO.File]::Exists($p2)){Start-Sleep -Milliseconds 5}; \
         [Console]::Write($e+'[?25h'+$e+'[1;1H'+'READY_C')",
    ]);
    command.env("MICA_CURSOR_PHASE_FILE", &phase.0);
    let (session, reader) = PtySession::spawn(command, 60, 8).expect("isolated ConPTY");
    let mut surface = Surface::new(ScreenSize::new(60, 8));
    receive_marker(&session, &reader, &mut surface, b"HIDDEN_A");
    assert!(!surface.cursor_visible(), "hide must survive ConPTY");
    let first_position = surface.grid().cursor.point;
    std::fs::write(&phase.0, "1").unwrap();
    receive_marker(&session, &reader, &mut surface, b"HIDDEN_B");
    assert!(
        !surface.cursor_visible(),
        "a hidden repaint must not reveal the cursor"
    );
    assert_ne!(
        surface.grid().cursor.point,
        first_position,
        "fixture must really move the cursor"
    );
    std::fs::write(phase.0.with_extension("phase2"), "2").unwrap();
    receive_marker(&session, &reader, &mut surface, b"READY_C");
    assert!(
        surface.cursor_visible(),
        "explicit show must restore the cursor"
    );
}
