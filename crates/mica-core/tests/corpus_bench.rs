//! #60 解析侧基准:asciinema v2(.cast)回放语料喂 Surface::feed。
//! 真实录像混合 TUI 重绘/颜色切换/宽字符/快速滚动,比合成基准更能暴露
//! print 循环真实瓶颈;与渲染侧 bench_build_rows_throughput 互补。
//!
//! 两步用法(release 跑,先录后量):
//! ```text
//! cargo test -p mica-core --test corpus_bench record_corpus -- --ignored --nocapture
//! cargo test -p mica-core --test corpus_bench feed_corpus   -- --ignored --nocapture
//! ```
//! 录制:record_corpus 起真 ConPTY PowerShell 驱动一批真实命令
//! (目录树/git log/帮助文本/彩色输出),落 target/corpus.cast;
//! 回放:feed_corpus 读文件连续喂 Surface(3 轮取最好,perf 定档同款),
//! 报 bytes/s 与 cells/s(cells = bytes / 4,四字节格近似)。
#![cfg(windows)]

use std::io::Write;
use std::time::Instant;

use mica_core::pty::{PtyReader, PtySession};
use mica_core::surface::{ScreenSize, Surface};

const CORPUS: &str = "target/corpus.cast";
const COLS: u16 = 120;
const ROWS: u16 = 40;

/// asciinema v2 录制:JSONL 头 + 事件帧 ["time","o","base64"]。
/// 命令集取真实负载画像:全屏 TUI(/colors)、大量路径与文本(ls/tree)、
/// git log(色彩+分页)、Unicode、长行截断
#[test]
#[ignore]
fn record_corpus() {
    let cmd = mica_core::pty::command_from_str("powershell.exe -NoLogo");
    let (mut session, reader) = PtySession::spawn(cmd, COLS, ROWS).expect("spawn");
    let script = [
        r"Get-ChildItem -Recurse -Depth 3 C:\Windows\System32 | Select-Object -First 4000 | Format-Table -AutoSize",
        "git -C . log --oneline --graph --decorate -2000",
        "Get-Help Set-Item -Full",
        r#"Write-Host (1..5000 | ForEach-Object { "line $_ 中文 unicode $([char]27)[31mRED$([char]27)[0m" })"#,
        "Get-ChildItem Env: | Format-List",
        "[char]27 + '[2J' + (1..40 | ForEach-Object { $i=$_; -join (1..118 | ForEach-Object { [char](65+($_+$i)%26) }) })",
    ];
    let mut all: Vec<u8> = Vec::new();
    fn pump(_session: &mut PtySession, reader: &PtyReader, all: &mut Vec<u8>) {
        // 读到静默或超时:批量收流
        let deadline = Instant::now() + std::time::Duration::from_secs(8);
        while Instant::now() < deadline {
            match reader.recv_timeout(std::time::Duration::from_millis(200)) {
                Some(chunk) => all.extend_from_slice(&chunk),
                None => break,
            }
        }
    }
    for c in script {
        let _ = session.write(format!("{c}\r\n").as_bytes());
        pump(&mut session, &reader, &mut all);
    }
    let _ = session.write(b"exit\r\n");
    pump(&mut session, &reader, &mut all);
    session.kill_child();

    let out = std::path::Path::new(CORPUS);
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let mut f = std::fs::File::create(out).unwrap();
    writeln!(
        f,
        r#"{{"version":2,"width":{COLS},"height":{ROWS},"env":{{"SHELL":"powershell"}}}}"#
    )
    .unwrap();
    // 单大帧(时间轴不是基准的输入,压成起始 0)
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&all);
    writeln!(f, r#"[0.0,"o","{b64}"]"#).unwrap();
    eprintln!("[corpus] 录制 {} 字节 → {}", all.len(), CORPUS);
}

/// 回放基准:3 轮取最好(perf 定档同款),报 MB/s + cells/s
#[test]
#[ignore]
fn feed_corpus() {
    let raw = std::fs::read_to_string(CORPUS)
        .unwrap_or_else(|e| panic!("先跑 record_corpus 生成语料:{e}"));
    let mut payload: Vec<u8> = Vec::new();
    for line in raw.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('[') {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line).expect("cast 事件 JSON");
        if v[1] == "o" {
            use base64::Engine as _;
            let b = base64::engine::general_purpose::STANDARD
                .decode(v[2].as_str().unwrap())
                .unwrap();
            payload.extend_from_slice(&b);
        }
    }
    assert!(!payload.is_empty(), "语料为空");
    let mut best = 0.0f64;
    for round in 0..3 {
        let mut s = Surface::new(ScreenSize::new(COLS as usize, ROWS as usize));
        let t = Instant::now();
        // 分块喂(64KB/块):比一次性大 feed 更接近真实转发线程节奏,
        // 也给 damage/行缓存路径真实压力
        for chunk in payload.chunks(64 * 1024) {
            s.feed(chunk);
        }
        let secs = t.elapsed().as_secs_f64();
        let bps = payload.len() as f64 / secs;
        if bps > best {
            best = bps;
        }
        eprintln!(
            "[corpus] 轮{}:{} 字节 / {:.3}s = {:.1} MB/s({:.0}万 cells/s)",
            round + 1,
            payload.len(),
            secs,
            bps / 1e6,
            bps / 4.0 / 1e4
        );
    }
    eprintln!("[corpus] BEST {:.1} MB/s", best / 1e6);
}
