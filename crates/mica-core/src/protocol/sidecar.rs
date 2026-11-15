//! 协议旁路监听(M4b/D29/D30 → 2026-11-02 T07 重造):与主 feed 并行的
//! 第二个 vte Parser。
//!
//! alacritty 上游 Term 不认 DECSET 2026 与 OSC 133,改架构拦截成本高;
//! 旁路 Parser 只认这几个序列,其余全丢——上游对未知序列本就忽略,
//! 双解析零冲突。吞吐代价由冒烟基准把关:>5% 回退则前缀快筛(spec D29
//! 备胎条款;2026-10-03 基准实测 25.4%,已启用):
//!
//! 只有块内出现候选前缀(`ESC[?2026` / `ESC]133` / `ESC]7`)才把字节
//! 深喂进 vte,其余字节(SGR 密集的普通输出)整块跳过。正确性依据:
//! - vte 状态机里 ESC 从任何状态回到 Escape 态(序列重同步点),深跑
//!   窗口从 ESC 起始即可,无需历史状态;
//! - 跨块半序列由 carry(块尾未决前缀,≤8B)与 open_seq(**任意**候选
//!   OSC 已开未闭——133 与 7 同待遇,T07 修复:旧实现只给 133 续命,
//!   OSC 7 的 cwd 广播拆块即丢)两个状态续命。
//!
//! T07 流序契约(分块不变性):
//! - 事件**有序**且携带字节偏移(偏移 = 序列终点在块内的下一字节):
//!   消费侧(Surface::feed)按偏移切片喂 Term,在事件位置取光标——
//!   marks 行号不再依赖块起点(旧实现块内所有标记共用块首光标,逐块
//!   与整块 feed 产生不同行号)。
//! - 2026 最终状态由**最后事件**决定(消费侧按序赋值即天然 last-wins;
//!   旧计数式 begin/end 各自独立应用,`l`后`h`会被折叠成关)。
//!
//! 已知近似:DCS 流内嵌的字面候选序列会被误判——真实场景不存在,
//! 接受(快筛以近似换吞吐,D29 既定)。

/// 一次 scan 的产物:有序事件流(字节偏移 + 事件),按流内顺序排列。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SidecarEvents {
    pub events: Vec<LocatedEvent>,
}

/// 一条带位置的事件。`offset` = 序列终点在**本块**内的下一字节位置
/// (消费侧把 `bytes[..offset]` 喂给 Term 后,光标即事件位置)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedEvent {
    pub offset: usize,
    pub event: SidecarEvent,
}

/// 旁路识别的事件种类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarEvent {
    /// `CSI ? 2026 h`:进入同步输出(冻结重绘)
    SyncBegin,
    /// `CSI ? 2026 l`:结束同步输出(放帧)
    SyncEnd,
    /// OSC 7 当前目录(file://URL 原文;消费侧解码)
    Cwd(String),
    /// OSC 133 标记(prompt/cmd/output/end)
    Mark(ShellMark),
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
    /// D:命令结束;exit code(D37:状态点消费)。FinalTerm 各实现形态
    /// 不一(`err=0` 与裸 `0` 都常见),宽松双形态都收,缺失/不可解析 = None
    CommandEnd { exit: Option<i32> },
}

/// `133;D` 第三参解析:`err=<n>` / 裸 `<n>` 双形态。
fn parse_exit(raw: Option<&[u8]>) -> Option<i32> {
    let raw = raw?;
    let digits = raw.strip_prefix(b"err=").unwrap_or(raw);
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit() || *b == b'-') {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

struct SidecarRecord {
    events: Vec<SidecarEvent>,
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
            self.events.push(if action == 'h' {
                SidecarEvent::SyncBegin
            } else {
                SidecarEvent::SyncEnd
            });
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell: bool) {
        // OSC 7 ; file://host/path:工作目录广播(消费侧存 per-surface)
        if params.first().is_some_and(|p| *p == b"7")
            && let Some(url) = params.get(1).copied()
            && let Ok(url) = std::str::from_utf8(url)
        {
            self.events.push(SidecarEvent::Cwd(url.to_string()));
        }
        // OSC 133 ; <mark>:params[0] = b"133",params[1] = b"A"/"B"/"C"/"D"
        if params.first().is_some_and(|p| *p == b"133") {
            let mark = match params.get(1).copied() {
                Some(b"A") => ShellMark::PromptStart,
                Some(b"B") => ShellMark::CommandStart,
                Some(b"C") => ShellMark::OutputStart,
                Some(b"D") => ShellMark::CommandEnd {
                    exit: parse_exit(params.get(2).copied()),
                },
                _ => return, // 未知标记丢弃(D30:非法参数丢弃)
            };
            self.events.push(SidecarEvent::Mark(mark));
        }
    }
}

/// 候选判定:块内出现这三个前缀之一(或其块尾截断形)才深跑。
const CSI_START: &[u8] = b"\x1b[?2026";
const OSC_START: &[u8] = b"\x1b]133;";
/// OSC 7 needle(2026-10-07):cwd 广播,新标签继承的数据源
const OSC7_START: &[u8] = b"\x1b]7;";
/// carry 判定:块尾若是这些 needle 的严格前缀(含 `\x1b[?2026` 整体——
/// 终结符 h/l 在下一块),续到下块再判。
const CSI_CARRY: [&[u8]; 2] = [b"\x1b[?2026h", b"\x1b[?2026l"];

/// 增量扫描器(前缀快筛 + 深跑 vte)。一个实例服务整个会话生命周期。
pub struct SidecarParser {
    parser: alacritty_terminal::vte::Parser,
    /// 块尾未决的候选前缀(≤ 8B),下块拼回头部重判。
    carry: Vec<u8>,
    /// 候选 OSC(133/7)已开未闭:参数/终止符还在路上,整块深跑直至
    /// BEL/ST(T07:OSC 7 与 133 同待遇——旧实现 7 拆块即丢事件)。
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

    /// 扫一段字节流,返回这段内到达的事件(带块内偏移;消费侧按偏移
    /// 与主解析交错)。
    pub fn scan(&mut self, bytes: &[u8]) -> SidecarEvents {
        if self.carry.is_empty() {
            return self.scan_inner(bytes, 0);
        }
        // carry 非空:拼回头部整体重判;偏移换算回**原块**坐标系
        // (carry 已消费,所有权转移防重复拼接)
        let carry_len = self.carry.len();
        let mut input = std::mem::take(&mut self.carry);
        input.extend_from_slice(bytes);
        self.scan_inner(&input, carry_len)
    }

    /// `carry_len`:input 头部有多少字节是上一块遗留——事件偏移要减去
    /// 它,落到调用方传入块的坐标系。
    fn scan_inner(&mut self, input: &[u8], carry_len: usize) -> SidecarEvents {
        self.carry.clear();
        let mut events = SidecarEvents::default();
        let to_chunk = |off: usize| off.saturating_sub(carry_len);

        if self.open_seq {
            // 待决序列的终止符不是"候选起点"——若不切第一段,它的
            // 事件会并入下一候选的段而错位(切点矩阵实证)。先定位本块
            // 内的终止符(可能就是首字节),在它处收口第一段
            let pend = pending_terminator(input);
            self.deep_run(input, 0, 0, pend, &mut events);
            // 开口状态 = 旧 pending 未闭合 或 本块新开了 OSC(needle 之后
            // 无终止符)。旧实现用 any(ESC|BEL) 启发式——旧终止符与新
            // 开口混谈,随机块实测丢 D 事件
            let still_open = pend.is_none() || self.has_open_osc(input);
            if still_open {
                self.open_seq = true;
                self.carry.clear(); // 序列字节已被 parser 消费,不得重喂
            } else {
                self.open_seq = false;
                self.update_carry(input);
            }
            for ev in &mut events.events {
                ev.offset = to_chunk(ev.offset);
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
                || tail.starts_with(OSC7_START)
                || CSI_START.starts_with(tail)
                || OSC_START.starts_with(tail)
                || OSC7_START.starts_with(tail)
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

        // 深跑:从候选 ESC 到块尾(窗口内后续序列一并解析),按候选
        // 序列终点定位事件偏移(T07)。carry 拼回时窗口可能从 carry 区
        // 起始——已喂边界换算到窗口坐标(floor 不超过窗口起点则 0)
        let window = &input[esc..];
        let fed_floor = carry_len.saturating_sub(esc);
        self.deep_run(window, esc, fed_floor, None, &mut events);
        self.update_carry(window);
        // 任一候选 OSC 开了没关(needle 之后无 BEL/ST):下块整块深跑。
        // carry 必须清——序列字节已被 parser 消费,重喂会在半开 OSC 里
        // 触发二次 dispatch(重喂 CSI 前缀无害:ESC 重同步 + 字节同一,
        // 重解析幂等;OSC 无此性质)
        if self.has_open_osc(window) {
            self.open_seq = true;
            self.carry.clear();
        }
        for ev in &mut events.events {
            ev.offset = to_chunk(ev.offset);
        }
        events
    }

    /// 窗口内是否存在"开了没关"的候选 OSC(133 或 7)。
    fn has_open_osc(&self, window: &[u8]) -> bool {
        let check = |needle: &[u8]| {
            let mut p = 0;
            while let Some(rel) = window[p..].windows(needle.len()).position(|w| w == needle) {
                let hit = p + rel;
                if !window[hit + needle.len()..]
                    .iter()
                    .any(|&b| b == 0x1b || b == 0x07)
                {
                    return true;
                }
                p = hit + needle.len();
            }
            false
        };
        check(OSC_START) || check(OSC7_START)
    }

    /// 深跑一段窗口,事件偏移 = 各候选**序列终点**(`base` 为窗口在
    /// input 内的起点)。实现:按候选序列起点切窗逐段喂 vte——段内
    /// 触发的事件归属该段包含的那个候选序列(其终点即事件偏移)。
    ///
    /// `fed_floor`:窗口内该位置之前的字节**上一块已 advance 过**
    /// (carry 拼回的遗留前缀)——绝不能重喂:OSC 半态下重喂的 ESC 是
    /// 终止符,会把正在收集的序列毁掉(F06 深层形态,旧实现同样存在,
    /// 分块矩阵测试首次暴露);CSI 半态重喂因 ESC 重同步而无害,但统一
    /// 跳过使两类语义一致。
    fn deep_run(
        &mut self,
        window: &[u8],
        base: usize,
        fed_floor: usize,
        pend: Option<usize>,
        events: &mut SidecarEvents,
    ) {
        let starts = candidate_starts(window);
        let pend_end = pend;
        // floor 只限制**喂入起点**,不筛除候选:起点落在已喂区(carry
        // 拼回的半序列)的候选,其序列终点在 floor 之后,段 = [floor..end]
        // ——若按起点筛除,该序列的事件会误归属到下一段(偏移错乱)
        let mut cursor = fed_floor;
        // 待决 OSC 先收口(它不是候选起点,不吃 seq_end 的 skip(1) 语义)
        if let Some(end) = pend_end
            && end > cursor
        {
            let seg = &window[cursor..end];
            let mut record = SidecarRecord { events: Vec::new() };
            self.parser.advance(&mut record, seg);
            for event in record.events {
                events.events.push(LocatedEvent {
                    offset: base + end,
                    event,
                });
            }
            cursor = end;
        }
        for &s in starts.iter() {
            let end = seq_end(window, s);
            if end <= cursor {
                continue; // 序列完全在已喂区内(理论上不出现,防御)
            }
            let seg = &window[cursor..end];
            let mut record = SidecarRecord { events: Vec::new() };
            self.parser.advance(&mut record, seg);
            for event in record.events {
                events.events.push(LocatedEvent {
                    offset: base + end,
                    event,
                });
            }
            cursor = end;
        }
        // 余段(无新候选起点):不应产生候选事件;防御性收尾(偏移=块尾)
        if cursor < window.len() {
            let end = window.len();
            let mut record = SidecarRecord { events: Vec::new() };
            self.parser.advance(&mut record, &window[cursor..]);
            for event in record.events {
                events.events.push(LocatedEvent {
                    offset: base + end,
                    event,
                });
            }
        }
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
                || (OSC7_START.starts_with(suffix) && OSC7_START.len() > k)
            {
                self.carry = suffix.to_vec();
                return;
            }
        }
    }
}

/// 待决 OSC 在本块内的终止符(排他下标;含首扫描——终止符可能就是
/// 块首字节)。None = 本块仍未闭合。
fn pending_terminator(input: &[u8]) -> Option<usize> {
    input.iter().position(|&b| b == 0x07 || b == 0x1b).map(|k| {
        if input[k] == 0x07 {
            k + 1
        } else {
            (k + 2).min(input.len())
        }
    })
}

/// 窗口内全部候选序列的起点(升序):任一 needle 的完整匹配处,或
/// "块尾截断形"的 ESC 处(序列终点=块尾,由 seq_end 自然处理)。
fn candidate_starts(window: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = window[i..].iter().position(|&b| b == 0x1b) {
        let at = i + rel;
        let tail = &window[at..];
        if tail.starts_with(CSI_START)
            || tail.starts_with(OSC_START)
            || tail.starts_with(OSC7_START)
            || CSI_START.starts_with(tail)
            || OSC_START.starts_with(tail)
            || OSC7_START.starts_with(tail)
        {
            out.push(at);
            if CSI_START.starts_with(tail)
                || OSC_START.starts_with(tail)
                || OSC7_START.starts_with(tail)
            {
                break; // 块尾截断形:后面不会再有完整起点
            }
        }
        i = at + 1;
    }
    out
}

/// 序列终点(排他下标):CSI 2026 固定 8B(h/l 紧随 needle);OSC 找
/// BEL(0x07,+1)或 ST 前导 ESC(+2);未闭合 = 窗口尾(open_seq 路径
/// 由调用方续命)。
fn seq_end(window: &[u8], start: usize) -> usize {
    if window[start..].starts_with(CSI_START) {
        return (start + CSI_START.len()).min(window.len());
    }
    let tail = &window[start..];
    for (k, &b) in tail.iter().enumerate().skip(1) {
        if b == 0x07 {
            return (start + k + 1).min(window.len());
        }
        if b == 0x1b {
            // ESC \(ST)——含转义符本体;ESC 恰为块尾时钳到块尾
            return (start + k + 2).min(window.len());
        }
    }
    window.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(bytes: &[u8]) -> SidecarEvents {
        SidecarParser::new().scan(bytes)
    }

    /// 事件序(略偏移)——多数语义测试只关心种类与顺序。
    fn kinds(e: &SidecarEvents) -> Vec<SidecarEvent> {
        e.events.iter().map(|le| le.event.clone()).collect()
    }

    #[test]
    fn detects_2026_set_and_reset() {
        assert_eq!(kinds(&scan(b"\x1b[?2026h")), vec![SidecarEvent::SyncBegin]);
        assert_eq!(kinds(&scan(b"\x1b[?2026l")), vec![SidecarEvent::SyncEnd]);
    }

    #[test]
    fn ordinary_2024_and_private_others_ignored() {
        assert_eq!(
            scan(b"\x1b[?2004h\x1b[?1h\x1b[?25l"),
            SidecarEvents::default()
        );
    }

    #[test]
    fn split_sequence_across_chunks() {
        let mut p = SidecarParser::new();
        assert_eq!(
            p.scan(b"\x1b[?20"),
            SidecarEvents::default(),
            "半序列不产事件"
        );
        assert_eq!(
            kinds(&p.scan(b"26h")),
            vec![SidecarEvent::SyncBegin],
            "续块完成序列"
        );
    }

    #[test]
    fn osc133_all_marks() {
        let e = scan(b"\x1b]133;A\x1b\\\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D\x1b\\");
        assert_eq!(
            kinds(&e),
            vec![
                SidecarEvent::Mark(ShellMark::PromptStart),
                SidecarEvent::Mark(ShellMark::CommandStart),
                SidecarEvent::Mark(ShellMark::OutputStart),
                SidecarEvent::Mark(ShellMark::CommandEnd { exit: None }),
            ]
        );
    }

    #[test]
    fn osc133_unknown_and_plain_text_ignored() {
        assert_eq!(kinds(&scan(b"\x1b]133;E\x07")), Vec::<SidecarEvent>::new());
        assert_eq!(
            kinds(&scan(b"hello \x1b[31m world")),
            Vec::<SidecarEvent>::new()
        );
        assert_eq!(scan(b"\x1b]0;title\x07"), SidecarEvents::default());
    }

    #[test]
    fn osc133_exit_code_dual_forms() {
        let mark = |e: &SidecarEvents| kinds(e);
        assert_eq!(
            mark(&scan(b"]133;D;err=0\x07")),
            vec![SidecarEvent::Mark(ShellMark::CommandEnd { exit: Some(0) })]
        );
        assert_eq!(
            mark(&scan(b"]133;D;127\x07")),
            vec![SidecarEvent::Mark(ShellMark::CommandEnd {
                exit: Some(127)
            })]
        );
        assert_eq!(
            mark(&scan(b"]133;D;err=-1\x07")),
            vec![SidecarEvent::Mark(ShellMark::CommandEnd { exit: Some(-1) })]
        );
        assert_eq!(
            mark(&scan(b"]133;D;err=x\x07")),
            vec![SidecarEvent::Mark(ShellMark::CommandEnd { exit: None })]
        );
        assert_eq!(
            mark(&scan(b"]133;D\x07")),
            vec![SidecarEvent::Mark(ShellMark::CommandEnd { exit: None })]
        );
    }

    #[test]
    fn interleaved_with_normal_output() {
        let e = scan(b"line1\n\x1b]133;A\x1b\\PS> \x1b[?2026h big blob \x1b[?2026l\n");
        assert_eq!(
            kinds(&e),
            vec![
                SidecarEvent::Mark(ShellMark::PromptStart),
                SidecarEvent::SyncBegin,
                SidecarEvent::SyncEnd,
            ]
        );
    }

    /// T07 核心契约:同一字节流,任意切点(整块/对半/逐字节)产生的
    /// 事件**序列**一致。可观察状态(grid/marks/sync/cwd)的一致性由
    /// tests/stream_invariance.rs 在 Surface 层另锁。
    #[test]
    fn event_order_is_chunk_invariant() {
        let input =
            b"line1\n\x1b]133;A\x1b\\PS> \x1b[?2026h blob \x1b[?2026l \x1b]7;file:///C:/w\x07 tail";
        let whole = kinds(&scan(input));
        let mut p = SidecarParser::new();
        let mut bytewise = Vec::new();
        for b in input.iter() {
            bytewise.extend(kinds(&p.scan(&[*b])));
        }
        assert_eq!(whole, bytewise, "逐字节与整块事件序一致");
        let mut p2 = SidecarParser::new();
        let mut halves = Vec::new();
        halves.extend(kinds(&p2.scan(&input[..input.len() / 2])));
        halves.extend(kinds(&p2.scan(&input[input.len() / 2..])));
        assert_eq!(whole, halves, "对半切与整块事件序一致");
    }

    /// OSC 7 跨块(T07 修复:7 与 133 同待遇续命,旧实现拆块即丢)。
    #[test]
    fn osc7_survives_arbitrary_splits() {
        let input = b"\x1b]7;file:///C:/%E4%B8%AD%E6%96%87\x07";
        let whole = kinds(&scan(input));
        assert_eq!(whole.len(), 1);
        // 计划用例:偏移 4 与 25 处切
        for split in [4usize, 25] {
            let mut p = SidecarParser::new();
            let mut got = Vec::new();
            got.extend(kinds(&p.scan(&input[..split])));
            got.extend(kinds(&p.scan(&input[split..])));
            assert_eq!(whole, got, "OSC 7 在偏移 {split} 处切分必须不丢事件");
        }
        // 逐字节极端形态
        let mut p = SidecarParser::new();
        let mut got = Vec::new();
        for b in input.iter() {
            got.extend(kinds(&p.scan(&[*b])));
        }
        assert_eq!(whole, got, "OSC 7 逐字节 feed 不丢事件");
    }

    /// 偏移语义:事件 offset = 序列终点的下一字节(喂完 bytes[..offset]
    /// 后光标即事件位置)。
    #[test]
    fn offsets_point_past_sequence_end() {
        let input = b"abc\x1b]133;A\x07def";
        let e = scan(input);
        assert_eq!(e.events.len(), 1);
        // 序列 8 字节(ESC ] 1 3 3 ; A BEL),起于 3,排他终点 11
        assert_eq!(e.events[0].offset, 11);
        assert_eq!(&input[..11], b"abc\x1b]133;A\x07");
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
        assert_eq!(
            kinds(&p.scan(b"\x1b[?2026h")),
            vec![SidecarEvent::SyncBegin]
        );
    }

    #[test]
    fn gate_split_csi_across_chunks() {
        let mut p = SidecarParser::new();
        assert_eq!(p.scan(b"\x1b[?20"), SidecarEvents::default());
        assert_eq!(
            kinds(&p.scan(b"26h")),
            vec![SidecarEvent::SyncBegin],
            "carry 续块完成序列"
        );
    }

    #[test]
    fn gate_split_osc133_across_chunks() {
        let mut p = SidecarParser::new();
        assert_eq!(p.scan(b"text\x1b]13"), SidecarEvents::default());
        assert_eq!(
            kinds(&p.scan(b"3;D\x07")),
            vec![SidecarEvent::Mark(ShellMark::CommandEnd { exit: None })]
        );
    }

    #[test]
    fn gate_osc133_open_until_terminator() {
        let mut p = SidecarParser::new();
        // OSC 133 开而未闭:参数跨块,BEL 终止后才 dispatch
        assert_eq!(
            p.scan(b"\x1b]133;A;cwd=/home/user"),
            SidecarEvents::default()
        );
        assert_eq!(
            kinds(&p.scan(b"/proj\x07rest \x1b[32mcolors\x1b[m")),
            vec![SidecarEvent::Mark(ShellMark::PromptStart)]
        );
        // 终止后回到快筛路径
        assert_eq!(p.scan(b"\x1b[31mred\x1b[m"), SidecarEvents::default());
    }

    #[test]
    fn gate_lone_esc_carries() {
        let mut p = SidecarParser::new();
        assert_eq!(p.scan(b"tail\x1b"), SidecarEvents::default());
        assert_eq!(kinds(&p.scan(b"[?2026l")), vec![SidecarEvent::SyncEnd]);
    }

    #[test]
    fn gate_csi_finalizer_next_chunk() {
        let mut p = SidecarParser::new();
        // 完整 needle 落在块尾,终结符 h 在下一块
        assert_eq!(p.scan(b"\x1b[?2026"), SidecarEvents::default());
        assert_eq!(kinds(&p.scan(b"h")), vec![SidecarEvent::SyncBegin]);
    }
}
