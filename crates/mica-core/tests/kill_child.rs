//! P2-4(1.0.9 大审查):泄漏前杀子链的集成锁——退出路径靠它保证
//! powershell 不漏成孤儿(1.0.4/1.0.5 的核心修复,此前无测试)。
#![cfg(windows)]

use std::time::{Duration, Instant};

use mica_core::pty::PtySession;
use portable_pty::CommandBuilder;

#[test]
fn kill_child_terminates_and_reaps() {
    let (mut session, reader) =
        PtySession::spawn(CommandBuilder::new("powershell.exe"), 80, 24).expect("spawn");
    // 启动即发 ESC[6n 等应答:替终端应答,防 powershell 阻塞(契约见 pty.rs)
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut buf = Vec::new();
    while Instant::now() < deadline {
        if let Some(chunk) = reader.recv_timeout(Duration::from_millis(500)) {
            buf.extend_from_slice(&chunk);
            let pending = buf.windows(4).filter(|w| *w == b"\x1b[6n").count();
            let answered = buf.windows(2).filter(|w| *w == b"\x1b[1").count();
            for _ in answered..pending {
                session.write(b"\x1b[1;1R").expect("reply DSR");
            }
            buf.clear();
            buf.extend_from_slice(&chunk); // 简化:重复无妨
            if pending > 0 {
                break;
            }
        }
    }
    session.kill_child();
    // 收割:try_wait 应在宽限内报告已退出(非阻塞语义)
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match session.try_wait() {
            Ok(Some(_)) => break,
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            _ => panic!("kill_child 后 try_wait 未收割"),
        }
    }
}
