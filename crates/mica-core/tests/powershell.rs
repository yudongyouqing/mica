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
    let mut answered = 0; // 已应答的 DSR 查询数(同 pty.rs read_until 的契约)
    while Instant::now() < deadline {
        if let Some(chunk) = reader.recv_timeout(Duration::from_millis(500)) {
            all.extend_from_slice(&chunk);
            // powershell 启动即发 ESC[6n 查光标并阻塞等回包,替终端应答
            let pending = all.windows(4).filter(|w| *w == b"\x1b[6n").count();
            for _ in answered..pending {
                session.write(b"\x1b[1;1R").expect("reply to DSR query");
            }
            answered = pending;
            if all.windows(11).any(|w| w == b"psmarker_42") {
                return;
            }
        }
    }
    panic!("never saw marker; got {}", String::from_utf8_lossy(&all));
}

/// ConPTY 当前**剥离** APC(M4c 预研字节级实证,2026-10-03):kitty graphics
/// 走 `ESC _ G ... ST`,输出流中 `ESC _ G` 全序列不出现,ST 的 `\` 以孤立
/// 可见字符泄漏。本测试是**金丝雀**:若未来 Windows 的 ConPTY 加入 VT 透传,
/// 此测试变红 = 边界移动,该重估 kitty graphics 可达性(见
/// docs/superpowers/research/2026-10-03-kitty-graphics.md)。
/// 注意断言必须匹配完整 `\x1b_G`——命令回显文本含 "_G" 字面量,是假阳性源。
#[test]
fn conpty_strips_apc_today_canary() {
    let (mut session, reader) =
        PtySession::spawn(CommandBuilder::new("powershell.exe"), 80, 24).expect("spawn powershell");
    // kitty graphics 探测序列原样输出(1x1 RGB,t=d,带应答请求 i=31)
    let cmd = concat!(
        "$e=[char]27; ",
        "[Console]::Write(\"$e\"+'_Gq=1,i=31,s=1,v=1,t=d,f=24;AAAA'+\"$e\"+'\\\\'); ",
        "Write-Output PROBE-DONE\r\n"
    );
    session.write(cmd.as_bytes()).expect("write");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut all = Vec::new();
    let mut answered = 0;
    while Instant::now() < deadline {
        if let Some(chunk) = reader.recv_timeout(Duration::from_millis(500)) {
            all.extend_from_slice(&chunk);
            let pending = all.windows(4).filter(|w| *w == b"\x1b[6n").count();
            for _ in answered..pending {
                session.write(b"\x1b[1;1R").expect("reply to DSR query");
            }
            answered = pending;
            if all.windows(10).any(|w| w == b"PROBE-DONE") {
                let passed = all.windows(3).any(|w| w == b"\x1b_G");
                assert!(
                    !passed,
                    "ConPTY 开始透传 APC 了!边界移动——重估 kitty graphics \
                     可达性并更新预研文档;{}",
                    String::from_utf8_lossy(&all)
                );
                return;
            }
        }
    }
    panic!(
        "probe never finished; got {}",
        String::from_utf8_lossy(&all)
    );
}

/// ConPTY 当前**消费** OSC 133(M5a 冒烟实证,2026-10-03):OSC 133 标记
/// 不透传(与 2026/APC 同类;OSC 8 是例外可透传)。金丝雀:未来 ConPTY
/// 若透传 OSC 133,此测试变红 = 边界移动,OSC 133 UI(jump/exit 状态点)
/// 即刻可用,重估并更新边界文档。
#[test]
fn conpty_strips_osc133_today_canary() {
    let (mut session, reader) =
        PtySession::spawn(CommandBuilder::new("powershell.exe"), 80, 24).expect("spawn powershell");
    let cmd = concat!(
        "$e=[char]27; ",
        "[Console]::Write(\"$e\"+'_Gq=1'+\"$e\"+'\\\\'); ", // APC 探测先行(参照)
        "[Console]::Write(\"$e\"+']133;D;err=0'+[char]7); ",
        "Write-Output OSC133-DONE\r\n"
    );
    session.write(cmd.as_bytes()).expect("write");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut all = Vec::new();
    let mut answered = 0;
    while Instant::now() < deadline {
        if let Some(chunk) = reader.recv_timeout(Duration::from_millis(500)) {
            all.extend_from_slice(&chunk);
            let pending = all.windows(4).filter(|w| *w == b"\x1b[6n").count();
            for _ in answered..pending {
                session.write(b"\x1b[1;1R").expect("reply to DSR query");
            }
            answered = pending;
            if all.windows(11).any(|w| w == b"OSC133-DONE") {
                // 透传判据 = 完整六字节 `ESC ] 1 3 3 ;`(实测 ConPTY 把
                // 引导符 ESC] 毁成 ST(ESC\),载荷 ]133;... 以纯文本泄漏,
                // BEL 被吃——与 APC 的 `\` 泄漏同款)
                let passed = all.windows(6).any(|w| w[0] == 0x1b && &w[1..] == b"]133;");
                assert!(
                    !passed,
                    "ConPTY 开始透传 OSC 133 了!边界移动——jump/exit 状态点 \
                     即刻可用,更新边界文档;{}",
                    String::from_utf8_lossy(&all)
                );
                return;
            }
        }
    }
    panic!(
        "canary never finished; got {}",
        String::from_utf8_lossy(&all)
    );
}
