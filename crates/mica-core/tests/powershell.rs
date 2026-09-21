//! Windows-only: real PowerShell over ConPTY, end to end.
#![cfg(windows)]

use std::time::{Duration, Instant};

use mica_core::pty::PtySession;
use portable_pty::CommandBuilder;

#[test]
fn powershell_echo_roundtrip() {
    let (mut session, reader) =
        PtySession::spawn(CommandBuilder::new("powershell.exe"), 80, 24).expect("spawn powershell");
    session.write(b"echo psmarker_42\r\n").expect("write");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut all = Vec::new();
    while Instant::now() < deadline {
        if let Some(chunk) = reader.recv_timeout(Duration::from_millis(500)) {
            all.extend_from_slice(&chunk);
            if all.windows(11).any(|w| w == b"psmarker_42") {
                return;
            }
        }
    }
    panic!("never saw marker; got {}", String::from_utf8_lossy(&all));
}
