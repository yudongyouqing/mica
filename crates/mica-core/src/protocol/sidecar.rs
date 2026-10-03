//! 协议旁路监听(M4b/D29/D30):与主 feed 并行的第二个 vte Parser。
//!
//! alacritty 上游 Term 不认 DECSET 2026 与 OSC 133,改架构拦截成本高;
//! 旁路 Parser 只认这两个序列,其余全丢——上游对未知序列本就忽略,
//! 双解析零冲突。吞吐代价由冒烟基准把关:>5% 回归则前缀快筛(spec D29
//! 备胎条款;2026-10-03 基准实测 25.4%,已启用):
//!
//! 只有块内出现候选前缀(`ESC[?2026` / `ESC]133`)才把字节深喂进 vte,
//! 其余字节(SGR 密集的普通输出)整块跳过。正确性依据两条:
//! - vte 状态机里 ESC 从任何状态回到 Escape 态(序列重同步点),所以
//!   深跑窗口从 ESC 起始即可,无需历史状态;
//! - 跨块半序列由 carry(块尾未决前缀,≤8B)与 open_seq(OSC 133 已开
//!   未闭,整块深跑直至 BEL/ST)两个状态续命。
//!
//! 已知近似:DCS 流内嵌的字面 `ESC]133` 会被误判——真实场景不存在,
//! 接受(快筛以近似换吞吐,D29 既定)。

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

/// 候选判定:块内出现这两个前缀之一(或其块尾截断形)才深跑。
const CSI_START: &[u8] = b"\x1b[?2026";
const OSC_START: &[u8] = b"\x1b]133;";
/// carry 判定:块尾若是这些 needle 的严格前缀(含 `\x1b[?2026` 整体——
/// 终结符 h/l 在下一块),续到下块再判。
const CSI_CARRY: [&[u8]; 2] = [b"\x1b[?2026h", b"\x1b[?2026l"];

/// 增量扫描器(前缀快筛 + 深跑 vte)。一个实例服务整个会话生命周期。
pub struct SidecarParser {
    parser: alacritty_terminal::vte::Parser,
    /// 块尾未决的候选前缀(≤ 8B),下块拼回头部重判。
    carry: Vec<u8>,
    /// OSC 133 已开未闭:参数/终止符还在路上,整块深跑直至 BEL/ST。
    open_seq: bool,
}

impl Default for SidecarParser {
    fn default() -> Self {
        Self {
            parser: alacritty_terminal::vte::Parser::new(),
            carry: Vec::new(),
            open_seq: false,
        }
    }
}

impl SidecarParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 扫一段字节流,返回这段内到达的事件(marks 由 Surface 侧记行号)。
    pub fn scan(&mut self, bytes: &[u8]) -> SidecarEvents {
        if self.carry.is_empty() {
            return self.scan_inner(bytes);
        }
        // carry 非空:拼回头部整体重判(carry 已消费,所有权转移防重复拼接)
        let mut input = std::mem::take(&mut self.carry);
        input.extend_from_slice(bytes);
        self.scan_inner(&input)
    }

    fn scan_inner(&mut self, input: &[u8]) -> SidecarEvents {
        self.carry.clear();
        let mut events = SidecarEvents::default();

        if self.open_seq {
            self.deep_run(input, &mut events);
            // BEL / ESC(ST 前导)都能关 OSC;关闭后块尾若恰好又开了新
            // 序列的前缀,交给 carry
            if input.iter().any(|&b| b == 0x1b || b == 0x07) {
                self.open_seq = false;
                self.update_carry(input);
            }
            return events;
        }

        // 快筛:找首个候选 ESC(needle 前缀,含块尾截断形)
        let mut i = 0;
        let mut deep_from = None;
        while let Some(rel) = input[i..].iter().position(|&b| b == 0x1b) {
            let tail = &input[i + rel..];
            if tail.starts_with(CSI_START)
                || tail.starts_with(OSC_START)
                || CSI_START.starts_with(tail)
                || OSC_START.starts_with(tail)
            {
                deep_from = Some(i + rel);
                break;
            }
            i += rel + 1;
        }

        let Some(esc) = deep_from else {
            // 无候选:普通输出(SGR/文本),整块不过 vte。块尾不可能残留
            // needle 前缀——有 ESC 处皆已判否(否 = 块内已见分叉字节,后
            // 缀必同样分叉),无 ESC 处不以前缀起;无需 carry
            return events;
        };

        // 深跑:从候选 ESC 到块尾(窗口内后续序列一并解析)
        let window = &input[esc..];
        self.deep_run(window, &mut events);
        self.update_carry(window);
        // OSC 133 开了没关(needle 之后无 BEL/ST):下块整块深跑。
        // carry 必须清——序列字节已被 parser 消费,重喂会在半开 OSC 里
        // 触发二次 dispatch(重喂 CSI 前缀无害:ESC 重同步 + 字节同一,
        // 重解析幂等;OSC 无此性质)
        let mut p = 0;
        while let Some(rel) = window[p..]
            .windows(OSC_START.len())
            .position(|w| w == OSC_START)
        {
            let hit = p + rel;
            if !window[hit + OSC_START.len()..]
                .iter()
                .any(|&b| b == 0x1b || b == 0x07)
            {
                self.open_seq = true;
                self.carry.clear();
                break;
            }
            p = hit + OSC_START.len();
        }
        events
    }

    fn deep_run(&mut self, bytes: &[u8], events: &mut SidecarEvents) {
        let mut record = SidecarRecord {
            events: SidecarEvents::default(),
        };
        self.parser.advance(&mut record, bytes);
        *events = record.events;
    }

    /// 块尾若是任一 needle 的严格前缀,截下存 carry。
    fn update_carry(&mut self, buf: &[u8]) {
        // 最长 needle = CSI_CARRY(8B);其余 needle 均不超过它
        let max_len = CSI_CARRY[0].len();
        for k in (1..=max_len.min(buf.len())).rev() {
            let suffix = &buf[buf.len() - k..];
            if CSI_CARRY
                .iter()
                .any(|n| n.starts_with(suffix) && n.len() > k)
                || (CSI_START.starts_with(suffix) && CSI_START.len() > k)
                || (OSC_START.starts_with(suffix) && OSC_START.len() > k)
            {
                self.carry = suffix.to_vec();
                return;
            }
        }
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

    // ---- 前缀快筛(D29 备胎)----

    #[test]
    fn gate_skips_sgr_dense_chunks() {
        let mut p = SidecarParser::new();
        // SGR 密集的普通输出:无候选,整块不过 vte
        assert_eq!(
            p.scan(b"\x1b[32mgreen\x1b[0m plain \x1b[1mbold\x1b[m tail\r\n"),
            SidecarEvents::default()
        );
        // 快筛不污染后续检测
        assert_eq!(p.scan(b"\x1b[?2026h").sync_begin, 1);
    }

    #[test]
    fn gate_split_csi_across_chunks() {
        let mut p = SidecarParser::new();
        assert_eq!(p.scan(b"\x1b[?20"), SidecarEvents::default());
        assert_eq!(p.scan(b"26h").sync_begin, 1, "carry 续块完成序列");
    }

    #[test]
    fn gate_split_osc133_across_chunks() {
        let mut p = SidecarParser::new();
        assert_eq!(p.scan(b"text\x1b]13"), SidecarEvents::default());
        let e = p.scan(b"3;D\x07");
        assert_eq!(e.marks, vec![ShellMark::CommandEnd]);
    }

    #[test]
    fn gate_osc133_open_until_terminator() {
        let mut p = SidecarParser::new();
        // OSC 133 开而未闭:参数跨块,BEL 终止后才 dispatch
        assert_eq!(
            p.scan(b"\x1b]133;A;cwd=/home/user"),
            SidecarEvents::default()
        );
        let e = p.scan(b"/proj\x07rest \x1b[32mcolors\x1b[m");
        assert_eq!(e.marks, vec![ShellMark::PromptStart]);
        // 终止后回到快筛路径
        assert_eq!(p.scan(b"\x1b[31mred\x1b[m"), SidecarEvents::default());
    }

    #[test]
    fn gate_lone_esc_carries() {
        let mut p = SidecarParser::new();
        assert_eq!(p.scan(b"tail\x1b"), SidecarEvents::default());
        assert_eq!(p.scan(b"[?2026l").sync_end, 1);
    }

    #[test]
    fn gate_csi_finalizer_next_chunk() {
        let mut p = SidecarParser::new();
        // 完整 needle 落在块尾,终结符 h 在下一块
        assert_eq!(p.scan(b"\x1b[?2026"), SidecarEvents::default());
        assert_eq!(p.scan(b"h").sync_begin, 1);
    }
}
