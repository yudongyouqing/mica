//! 诊断(临时):应答 XTVERSION/kitty 后 claude 的 logo 字节是否变体
use mica_core::pty::{PtySession, command_from_str};
use std::io::Write;
use std::time::{Duration, Instant};

#[test]
fn capture_claude_startup_bytes() {
    let exe = std::env::var("CLAUDE_EXE").unwrap_or_else(|_| "claude.cmd".into());
    let reply_xtversion = std::env::var("XT_REPLY").is_ok();
    let cmd = command_from_str(&exe);
    let (mut session, reader) = PtySession::spawn(cmd, 110, 30).expect("spawn");
    let start = Instant::now();
    let deadline = start + Duration::from_secs(20);
    let mut all = Vec::new();
    let mut answered6 = 0usize;
    let mut sent_enter = false;
    let xt_done = std::cell::Cell::new(false);
    let kit_done = std::cell::Cell::new(false);
    while Instant::now() < deadline {
        if let Some(chunk) = reader.recv_timeout(Duration::from_millis(300)) {
            all.extend_from_slice(&chunk);
        }
        // ESC[6n 光标查询
        let pending = all.windows(4).filter(|w| *w == b"\x1b[6n").count();
        for _ in answered6..pending {
            let _ = session.write(b"\x1b[1;1R");
        }
        answered6 = pending;
        if reply_xtversion {
            // XTVERSION(>0q)应答一次:DCS >|name ST
            if all.windows(5).any(|w| w == b"\x1b[>0q") && !xt_done.replace(true) {
                let _ = session.write(b"\x1bP>|ghostty 1.2.1\x1b\\");
            }
            // kitty ?u 应答一次:CSI ?flags u
            if all.windows(3).any(|w| w == b"\x1b[?u") && !kit_done.replace(true) {
                let _ = session.write(b"\x1b[?1u");
            }
        }
        if start.elapsed() > Duration::from_secs(6) && !sent_enter {
            sent_enter = true;
            let _ = session.write(b"\x1b[B\r");
        }
        if all.len() > 60_000 {
            break;
        }
    }
    let out = std::env::temp_dir().join("claude-capture.bin");
    std::fs::File::create(&out)
        .unwrap()
        .write_all(&all)
        .unwrap();
    eprintln!("captured {} bytes", all.len());
    drop(session);
}
