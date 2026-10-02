//! 协议旁路监听(M4b/D29/D30):与主 feed 并行的第二个 vte Parser。
//!
//! alacritty 上游 Term 不认 DECSET 2026 与 OSC 133,改架构拦截成本高;
//! 旁路 Parser 只认这两个序列,其余全丢——上游对未知序列本就忽略,
//! 双解析零冲突。吞吐代价由冒烟基准把关(>5% 回归则前缀快筛,spec D29)。

/// 一次 scan 的产物。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SidecarEvents {
    /// `CSI ? 2026 h`:进入同步输出(冻结重绘)
    pub sync_begin: usize,
    /// `CSI ? 2026 l`:结束同步输出(放帧)
    pub sync_end: usize,
    /// OSC 133 标记(按到达顺序;行号由消费侧结合光标位记录)
    pub marks: Vec<ShellMark>,
}

/// OSC 133 的 A/B/C/D 语义(prompt/cmd/output/end)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellMark {
    /// A:prompt 开始
    PromptStart,
    /// B:命令开始(输入行)
    CommandStart,
    /// C:输出开始
    OutputStart,
    /// D:命令结束(exit code 可选参数忽略——M5 消费时再议)
    CommandEnd,
}

struct SidecarRecord {
    events: SidecarEvents,
}

impl alacritty_terminal::vte::Perform for SidecarRecord {
    fn csi_dispatch(
        &mut self,
        params: &alacritty_terminal::vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // vte 0.15 实证(源码 lib.rs:207 action_collect):'?'(0x3F)在 CsiParam 态
        // 被 action_collect 收进 **intermediates**(private 标记),params 只收数字——
        // 判定:intermediates == [b'?'] 且首参数为 2026
        if (action == 'h' || action == 'l')
            && intermediates == b"?"
            && params
                .iter()
                .next()
                .is_some_and(|sub| sub.len() == 1 && sub[0] == 2026)
        {
            if action == 'h' {
                self.events.sync_begin += 1;
            } else {
                self.events.sync_end += 1;
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell: bool) {
        // OSC 133 ; <mark>:params[0] = b"133",params[1] = b"A"/"B"/"C"/"D"
        if params.first().is_some_and(|p| *p == b"133") {
            let mark = match params.get(1).copied() {
                Some(b"A") => ShellMark::PromptStart,
                Some(b"B") => ShellMark::CommandStart,
                Some(b"C") => ShellMark::OutputStart,
                Some(b"D") => ShellMark::CommandEnd,
                _ => return, // 未知标记丢弃(D30:非法参数丢弃)
            };
            self.events.marks.push(mark);
        }
    }
}

/// 增量扫描器:跨 chunk 的半序列由内部 Parser 状态保持(与主 feed 同款
/// 逐字节推进,一个 parser 实例服务整个会话生命周期,不要每次 feed 新建)。
pub struct SidecarParser {
    parser: alacritty_terminal::vte::Parser,
}

impl Default for SidecarParser {
    fn default() -> Self {
        Self {
            parser: alacritty_terminal::vte::Parser::new(),
        }
    }
}

impl SidecarParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 扫一段字节流,返回这段内到达的事件(marks 由 Surface 侧记行号)。
    pub fn scan(&mut self, bytes: &[u8]) -> SidecarEvents {
        let mut record = SidecarRecord {
            events: SidecarEvents::default(),
        };
        self.parser.advance(&mut record, bytes);
        record.events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(bytes: &[u8]) -> SidecarEvents {
        SidecarParser::new().scan(bytes)
    }

    #[test]
    fn detects_2026_set_and_reset() {
        assert_eq!(scan(b"\x1b[?2026h").sync_begin, 1);
        assert_eq!(scan(b"\x1b[?2026l").sync_end, 1);
    }

    #[test]
    fn ordinary_2024_and_private_others_ignored() {
        let e = scan(b"\x1b[?2004h\x1b[?1h\x1b[?25l");
        assert_eq!(e, SidecarEvents::default());
    }

    #[test]
    fn split_sequence_across_chunks() {
        let mut p = SidecarParser::new();
        let e1 = p.scan(b"\x1b[?20");
        assert_eq!(e1, SidecarEvents::default(), "半序列不产事件");
        let e2 = p.scan(b"26h");
        assert_eq!(e2.sync_begin, 1, "续块完成序列");
    }

    #[test]
    fn osc133_all_marks() {
        let e = scan(b"\x1b]133;A\x1b\\\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D\x1b\\");
        assert_eq!(
            e.marks,
            vec![
                ShellMark::PromptStart,
                ShellMark::CommandStart,
                ShellMark::OutputStart,
                ShellMark::CommandEnd,
            ]
        );
    }

    #[test]
    fn osc133_unknown_and_plain_text_ignored() {
        assert_eq!(scan(b"\x1b]133;E\x07").marks, Vec::<ShellMark>::new());
        assert_eq!(scan(b"hello \x1b[31m world").marks, Vec::<ShellMark>::new());
        assert_eq!(scan(b"\x1b]0;title\x07"), SidecarEvents::default());
    }

    #[test]
    fn interleaved_with_normal_output() {
        let e = scan(b"line1\n\x1b]133;A\x1b\\PS> \x1b[?2026h big blob \x1b[?2026l\n");
        assert_eq!(e.marks, vec![ShellMark::PromptStart]);
        assert_eq!(e.sync_begin, 1);
        assert_eq!(e.sync_end, 1);
    }
}
