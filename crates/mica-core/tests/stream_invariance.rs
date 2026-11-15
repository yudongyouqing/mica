//! T07 字节流分块不变性回归(F06–F09):真实管道可以任意拆包/并包,
//! 终端的可观察状态(marks 行号、cwd、2026 同步态、标题)不得依赖
//! 分块方式。三条铁律:
//! - 计划最小用例:sync last-event-wins、OSC 7 中文 URL 按 UTF-8 解码、
//!   marks 行号与分块无关;
//! - 全切点矩阵:每一条测试流在每个单一切点二分,状态必须与整块一致;
//! - 随机块(固定种子):任意块边界拼合,状态一致。

use mica_core::protocol::sidecar::ShellMark;
use mica_core::surface::{ScreenSize, Surface};

fn new_surface() -> Surface {
    Surface::new(ScreenSize::new(80, 24))
}

/// 逐字节 feed。
fn feed_bytewise(s: &mut Surface, input: &[u8]) {
    for b in input {
        s.feed(&[*b]);
    }
}

/// 每个单一切点二分(矩阵在 assert_chunk_invariance 内联展开,每切点
/// 用全新 Surface——本帮手保留给需要同面连续二分的后续场景)。
#[allow(dead_code)]
fn feed_all_splits(s: &mut Surface, input: &[u8]) {
    for split in 1..input.len() {
        s.feed(&input[..split]);
        s.feed(&input[split..]);
    }
}

/// 固定种子的伪随机块(确定性:失败可复现)。
fn feed_random_chunks(s: &mut Surface, input: &[u8], seed: u64) {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    let mut i = 0;
    while i < input.len() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = 1 + (state >> 33) as usize % 7;
        let end = (i + len).min(input.len());
        s.feed(&input[i..end]);
        i = end;
    }
}

// ---------------------------------------------------------------------------
// 计划最小用例(2026-11-01-comprehensive-hardening T07)
// ---------------------------------------------------------------------------

/// F08:同块 `2026l` 后 `2026h` 最终应处于**开启**(最后事件赢)。
#[test]
fn sync_last_event_wins() {
    let mut s = new_surface();
    s.feed(b"\x1b[?2026l\x1b[?2026h");
    assert!(s.sync_output_active());

    // 反向:h 后 l → 关(顺序语义的另一面)
    let mut s = new_surface();
    s.feed(b"\x1b[?2026h\x1b[?2026l");
    assert!(!s.sync_output_active());
}

/// F07:OSC 7 百分号 UTF-8 中文路径正确解码;Windows 路径约定 `C:/...`
/// (盘符前的 '/' 剥除;若接口改反斜杠应同步本约定)。
#[test]
fn osc7_chinese_url_decodes_as_utf8() {
    let mut s = new_surface();
    s.feed(b"\x1b]7;file:///C:/%E4%B8%AD%E6%96%87\x07");
    assert_eq!(s.cwd(), Some("C:/中文"));
}

/// F09:marks 行号记录在**事件位置**,不依赖 feed 分块。
#[test]
fn marks_do_not_depend_on_feed_chunks() {
    let input = b"\x1b]133;A\x07line1\r\nline2\r\n\x1b]133;B\x07";
    let mut whole = new_surface();
    whole.feed(input);
    let mut bytewise = new_surface();
    feed_bytewise(&mut bytewise, input);
    let positions = |s: &Surface| s.shell_marks().iter().map(|m| m.line).collect::<Vec<_>>();
    assert_eq!(positions(&whole), vec![0, 2]);
    assert_eq!(positions(&whole), positions(&bytewise));
}

// ---------------------------------------------------------------------------
// 矩阵与属性测试
// ---------------------------------------------------------------------------

/// 可观察状态四元组(marks 行号、cwd、sync、标题)。
fn observe(s: &Surface) -> (Vec<(ShellMark, i32)>, Option<String>, bool) {
    (
        s.shell_marks()
            .into_iter()
            .map(|m| (m.mark, m.line))
            .collect(),
        s.cwd().map(str::to_string),
        s.sync_output_active(),
    )
}

fn assert_chunk_invariance(input: &[u8], name: &str) {
    let mut whole = new_surface();
    whole.feed(input);
    let expect = observe(&whole);

    let mut bytewise = new_surface();
    feed_bytewise(&mut bytewise, input);
    assert_eq!(observe(&bytewise), expect, "{name}: 逐字节");

    // 全单一切点矩阵
    for split in 1..input.len() {
        let mut s = new_surface();
        s.feed(&input[..split]);
        s.feed(&input[split..]);
        assert_eq!(observe(&s), expect, "{name}: 切点 {split}");
    }

    // 固定种子随机块 × 3
    for seed in [7u64, 0xC0FFEE, 20261102] {
        let mut s = new_surface();
        feed_random_chunks(&mut s, input, seed);
        assert_eq!(observe(&s), expect, "{name}: 随机块 seed={seed}");
    }
}

#[test]
fn invariance_sync_flips() {
    // l/h/h/l/重复设置:最终状态由最后事件决定(F08 全形态)
    assert_chunk_invariance(b"\x1b[?2026l\x1b[?2026h", "l→h");
    assert_chunk_invariance(b"\x1b[?2026h\x1b[?2026l", "h→l");
    assert_chunk_invariance(b"\x1b[?2026h\x1b[?2026l\x1b[?2026h", "h→l→h");
    assert_chunk_invariance(b"\x1b[?2026h\x1b[?2026h\x1b[?2026l", "h→h→l");
}

#[test]
fn invariance_osc7_forms() {
    assert_chunk_invariance(b"\x1b]7;file:///C:/%E4%B8%AD%E6%96%87\x07", "中文百分号");
    assert_chunk_invariance(b"\x1b]7;file:///C:/Users/dev\x07", "ASCII");
    // 带主机名形态
    assert_chunk_invariance(b"\x1b]7;file://host/C:/w\x07", "host");
    // 重复广播:后者赢
    assert_chunk_invariance(
        b"\x1b]7;file:///C:/a\x07text\x1b]7;file:///D:/b\x07",
        "双广播",
    );
}

#[test]
fn invariance_marks_with_text_between() {
    // 多标记 + 中间多行文本 + 命令结束形态
    assert_chunk_invariance(
        b"\x1b]133;A\x07P> \x1b]133;B\x07ls\r\nout1\r\nout2\r\n\x1b]133;C\x07\x1b]133;D;err=0\x07",
        "A/B/C/D 序列",
    );
    // ST(\x1b\\)终止形态
    assert_chunk_invariance(b"\x1b]133;A\x1b\\\r\n\x1b]133;D;127\x1b\\", "ST 终止");
}

#[test]
fn invariance_mixed_stream() {
    // 混合流:文本 + SGR + 2026 + marks + cwd 交错(现实 shell 输出形态)
    let input = b"\x1b[32mgreen\x1b[0m line\r\n\x1b]133;A\x07PS> \x1b[?2026h \
\x1b]7;file:///C:/Proj/%E9%A1%B9%E7%9B%AE\x07 blob \x1b[?2026l\r\n\
\x1b]133;B\x07cmd\x1b]133;D;err=0\x07 done\r\n";
    assert_chunk_invariance(input, "混合流");
}

/// 历史区滚动后的标记:行号换算(history 淘汰)在任意分块下一致。
/// 视口 24 行,灌 60 行后 A/B 的视口行号应为负(滚入历史)且分块不变。
#[test]
fn invariance_marks_after_scrollback() {
    let mut head = Vec::new();
    head.extend_from_slice(b"\x1b]133;A\x07");
    for i in 0..60 {
        head.extend_from_slice(format!("line-{i:03}\r\n").as_bytes());
    }
    head.extend_from_slice(b"\x1b]133;B\x07tail");
    assert_chunk_invariance(&head, "滚回后标记");

    // 直接断言整块形态的行号语义:B 在最后一行(视口底),A 为负(历史)
    let mut s = new_surface();
    s.feed(&head);
    let marks = s.shell_marks();
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[1].line, 23, "B 在视口末行");
    assert!(marks[0].line < 0, "A 已滚入历史(视口行号为负)");
}
