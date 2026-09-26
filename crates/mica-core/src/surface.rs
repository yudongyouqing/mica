//! Terminal surface: owns the screen state machine, turns raw pty bytes into
//! grid mutations, and collects events that must leave the terminal
//! (query replies go back to the pty, title changes go to the window).

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Grid, Scroll};
use alacritty_terminal::index::{Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};

/// 滚动指令直通类型(重导出避免壳层直依赖 alacritty_terminal)。
pub use alacritty_terminal::grid::Scroll as ScrollCommand;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::Processor;

use crate::config::palette::Palette;

/// 视口脏区(Task 8):`Full` = 全屏重建;`Lines` = 仅列出的视口行
/// (0 = 顶,升序、无重复)。行号语义已对齐 alacritty 0.26:`Term::damage()`
/// 迭代器产出的 `LineDamageBounds::line` 已是视口行(滚动出屏的不可见行被
/// 上游过滤),消费方只需按行号增量重建。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    Full,
    Lines(Vec<usize>),
}

/// Visible screen dimensions. `total_lines` reports the screen only, matching
/// upstream's `TermSize`; scrollback capacity comes from `Config::scrolling_history`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl ScreenSize {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }
}

impl Dimensions for ScreenSize {
    fn columns(&self) -> usize {
        self.columns
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
}

/// Palette entries reported for OSC 4/10/11/12 queries.
/// Indices follow `NamedColor`: 0..=15 slots, 256 = foreground,
/// 257 = background, 258 = cursor.
struct ProxyState {
    pty_writes: Vec<String>,
    title: Option<String>,
    size: Option<ScreenSize>,
    /// 查询应答用的格子像素尺寸,默认与退役的 8×16 常量一致
    cell: (u16, u16),
    /// OSC 4/10/11/12 应答与渲染同源(单一来源:config::palette)
    palette: Palette,
}

impl Default for ProxyState {
    fn default() -> Self {
        Self {
            pty_writes: Vec::new(),
            title: None,
            size: None,
            cell: (8, 16),
            palette: Palette::DEFAULT,
        }
    }
}

/// 锁获取:持锁 panic 只会毒化标记,内部状态并未损坏
/// (无跨字段不变量),取回继续用,别让一个坏事件炸掉整个标签。
fn lock(state: &Mutex<ProxyState>) -> std::sync::MutexGuard<'_, ProxyState> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// `Term`'s event sink. Everything the terminal wants to say to the *outside*
/// world lands here so `Surface` can drain it deterministically.
#[derive(Clone, Default)]
pub(crate) struct EventProxy(Arc<Mutex<ProxyState>>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let mut state = lock(&self.0);
        match event {
            Event::PtyWrite(s) => state.pty_writes.push(s),
            Event::Title(title) => state.title = Some(title),
            Event::ColorRequest(index, format) => {
                let rgb = state.palette.query(index);
                state.pty_writes.push(format(rgb));
            }
            Event::TextAreaSizeRequest(format) => {
                let size = state.size.unwrap_or(ScreenSize::new(80, 24));
                let ws = WindowSize {
                    num_cols: size.columns as u16,
                    num_lines: size.screen_lines as u16,
                    cell_width: state.cell.0,
                    cell_height: state.cell.1,
                };
                state.pty_writes.push(format(ws));
            }
            // M0 ignores: clipboard, bell, blink, wakeup (window renders on its
            // own cadence), child exit (Task 7 polls the pty child directly).
            _ => {}
        }
    }
}

/// One terminal: a pty-byte sink whose grid can be read for rendering.
pub struct Surface {
    term: Term<EventProxy>,
    parser: Processor,
    proxy: EventProxy,
    size: ScreenSize,
    /// take_damage 首帧哨兵:TermDamageState 构造即 full=true,本应自然
    /// Full,但那是上游实现细节——本层显式保证"第一次消费必是 Full"。
    has_drawn: bool,
}

impl Surface {
    pub fn new(size: ScreenSize) -> Self {
        let proxy = EventProxy::default();
        let config = Config {
            scrolling_history: 10_000,
            ..Default::default()
        };
        let term = Term::new(config, &size, proxy.clone());
        lock(&proxy.0).size = Some(size);
        Self {
            term,
            parser: Processor::new(),
            proxy,
            size,
            has_drawn: false,
        }
    }

    /// Feed raw pty output into the terminal state machine.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, size: ScreenSize) {
        self.size = size;
        self.term.resize(size);
        lock(&self.proxy.0).size = Some(size);
    }

    /// 查询应答用的格子像素尺寸(dwrite 度量产出自 mica-render,启动后立即设置真值)
    pub fn set_cell_metrics(&mut self, cell_width: u16, cell_height: u16) {
        self.proxy.0.lock().unwrap_or_else(|e| e.into_inner()).cell = (cell_width, cell_height);
    }

    /// 热替换调色板:此后 OSC 4/10/11/12 查询按新 palette 应答。
    /// 渲染侧的同步换肤由调用方(app)一并驱动,两处同源才不各说各话。
    pub fn set_palette(&mut self, palette: &Palette) {
        lock(&self.proxy.0).palette = *palette;
    }

    /// Drain damage accumulated since the last call, then reset the term's
    /// damage state (upstream contract: consume implies reset).
    ///
    /// 视口行以外(滚出屏的历史行)一律丢弃:渲染只见视口。已知语义:上游
    /// `Term::damage()` 恒把当前光标行计入(保守正确),所以经本 API 拿到
    /// 空集 `Lines(vec![])` 不可达;空集分支是 render 层的库级兜底。
    pub fn take_damage(&mut self) -> Damage {
        if !self.has_drawn {
            // 首帧哨兵:无消费历史,强制全量。仍要先消费一次 damage():它的
            // 副作用是把上游 last_cursor 同步到当前光标——若跳过,首帧之后
            // 光标一动,(0,0) 旧位会被误伤一行假脏(对拍测试踩出的坑)。
            // 此时 damage 状态构造即 full,消费结果必为 Full,返回值可弃
            self.has_drawn = true;
            let _ = self.term.damage();
            self.term.reset_damage();
            return Damage::Full;
        }
        let screen_lines = self.size.screen_lines();
        let damage = match self.term.damage() {
            TermDamage::Full => Damage::Full,
            TermDamage::Partial(bounds) => {
                let mut rows: Vec<usize> = bounds
                    .filter_map(|b| (b.line < screen_lines).then_some(b.line))
                    .collect();
                // 上游迭代器按行升序且唯一;显式排序去重把契约钉在本层,
                // 不依赖上游迭代器的实现细节
                rows.sort_unstable();
                rows.dedup();
                Damage::Lines(rows)
            }
        };
        self.term.reset_damage();
        damage
    }

    /// Drain replies that must be written back into the pty (DSR/OSC answers).
    pub fn take_pty_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut lock(&self.proxy.0).pty_writes)
    }

    pub fn take_title(&mut self) -> Option<String> {
        std::mem::take(&mut lock(&self.proxy.0).title)
    }

    pub fn grid(&self) -> &Grid<Cell> {
        self.term.grid()
    }

    /// DECCKM(DECSET 1)application cursor keys:编码器据此把导航键发成
    /// SS3 而非 CSI。
    pub fn app_cursor_mode(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// 视口滚动(scrollback)。Delta(正) 向历史方向,Bottom 归零跟随。
    pub fn scroll_display(&mut self, scroll: Scroll) {
        self.term.scroll_display(scroll);
    }

    /// 当前滚动偏移(0 = 钉在屏幕底跟随输出)。
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    // ---- 选择(M2a/T3):状态与拼接全用上游成品,这里只做薄封装 ----

    /// 开始一次选择(按下/双击/三击分别传 Simple/Semantic/Lines)。
    /// point 为 buffer 坐标(视口行 + display_offset)。
    pub fn selection_begin(&mut self, ty: SelectionType, point: Point, side: Side) {
        self.term.selection = Some(Selection::new(ty, point, side));
    }

    /// 拖动更新选区末端。
    pub fn selection_update(&mut self, point: Point, side: Side) {
        if let Some(sel) = &mut self.term.selection {
            sel.update(point, side);
        }
    }

    /// 清空选区(点击空白/ESC/复制后)。
    pub fn selection_clear(&mut self) {
        self.term.selection = None;
    }

    /// 选区规范化范围(起点 ≤ 终点),渲染高亮判定用。
    pub fn selection_range(&self) -> Option<SelectionRange> {
        self.term
            .selection
            .as_ref()
            .and_then(|sel| sel.to_range(&self.term))
    }

    /// 选区文本(上游拼接,含 wrap 语义)。
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    pub fn size(&self) -> ScreenSize {
        self.size
    }
}

#[cfg(test)]
mod tests {
    // Step 1 的测试原样移入此处
    use super::*;
    use alacritty_terminal::event::Event;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
    use std::sync::Arc;

    fn char_at(surface: &Surface, line: usize, column: usize) -> char {
        surface.grid()[Line(line as i32)][Column(column)].c
    }

    #[test]
    fn printed_text_lands_in_grid() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        s.feed(b"hello");
        let row: String = (0..5).map(|c| char_at(&s, 0, c)).collect();
        assert_eq!(row, "hello");
    }

    #[test]
    fn crlf_moves_to_next_line() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        s.feed(b"ab\r\ncd");
        let row: String = (0..2).map(|c| char_at(&s, 1, c)).collect();
        assert_eq!(row, "cd");
    }

    #[test]
    fn ansi_sgr_sets_foreground() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        s.feed(b"\x1b[31mX");
        let cell = &s.grid()[Line(0)][Column(0)];
        assert_eq!(cell.fg, Color::Named(NamedColor::Red));
    }

    #[test]
    fn resize_preserves_content() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        s.feed(b"hello");
        s.resize(ScreenSize::new(20, 5));
        let row: String = (0..5).map(|c| char_at(&s, 0, c)).collect();
        assert_eq!(row, "hello");
        assert_eq!(s.size(), ScreenSize::new(20, 5));
    }

    #[test]
    fn osc_title_is_collected() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        s.feed(b"\x1b]0;my title\x07");
        assert_eq!(s.take_title(), Some("my title".to_string()));
    }

    #[test]
    fn decset_cursor_keys_toggles_app_cursor_mode() {
        // DECSET 1 (DECCKM):编码器据此决定导航键走 SS3 还是 CSI
        let mut s = Surface::new(ScreenSize::new(10, 3));
        assert!(!s.app_cursor_mode());
        s.feed(b"\x1b[?1h");
        assert!(s.app_cursor_mode());
        s.feed(b"\x1b[?1l");
        assert!(!s.app_cursor_mode());
    }

    #[test]
    fn poisoned_proxy_state_is_recovered_not_fatal() {
        let proxy = EventProxy::default();
        // 人为毒化:持锁 panic(静默 panic hook,保持测试输出干净)
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = proxy.0.lock().unwrap();
            panic!("deliberate poison");
        }));
        std::panic::set_hook(prev_hook);
        // 毒化后事件投递必须继续工作,而不是连锁 panic 炸掉标签
        proxy.send_event(Event::Bell);
        assert!(lock(&proxy.0).pty_writes.is_empty());
    }

    #[test]
    fn proxy_answers_color_requests_into_pty_writes() {
        // 直接驱动事件代理,不依赖 Term 对 OSC 查询的具体映射
        let proxy = EventProxy::default();
        let format = Arc::new(|rgb: Rgb| {
            format!(
                "\x1b]10;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}",
                rgb.r >> 4,
                rgb.r & 0xf,
                rgb.g >> 4,
                rgb.g & 0xf,
                rgb.b >> 4,
                rgb.b & 0xf
            )
        });
        proxy.send_event(Event::ColorRequest(256, format));
        let writes = proxy.0.lock().unwrap().pty_writes.clone();
        assert!(writes[0].starts_with("\x1b]10;rgb:"));
    }

    /// 回答应答值的直读格式:6 位 hex,便于断言具体颜色。
    fn fmt_rgb() -> Arc<dyn Fn(Rgb) -> String + Send + Sync> {
        Arc::new(|rgb: Rgb| format!("{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b))
    }

    #[test]
    fn color_request_answers_palette_after_set_palette() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        let mut custom = Palette::DEFAULT;
        custom.apply_pair("palette", "1=#ff5555").unwrap();
        s.set_palette(&custom);
        // OSC 4 查询槽 1:必须答替换后的 palette 值,而非旧的全白
        s.proxy.send_event(Event::ColorRequest(1, fmt_rgb()));
        assert_eq!(s.take_pty_writes(), vec!["ff5555".to_string()]);
    }

    #[test]
    fn color_request_osc12_answers_cursor_color() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        let mut custom = Palette::DEFAULT;
        custom.apply_pair("cursor-color", "#00ff00").unwrap();
        s.set_palette(&custom);
        s.proxy.send_event(Event::ColorRequest(258, fmt_rgb()));
        assert_eq!(s.take_pty_writes(), vec!["00ff00".to_string()]);
    }

    #[test]
    fn color_request_default_answers_fg_and_bg() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        // 256=前景(白)、257=背景(#1e1e1e)——M0 全答白是欠账,现在按 palette 分流
        s.proxy.send_event(Event::ColorRequest(256, fmt_rgb()));
        assert_eq!(s.take_pty_writes(), vec!["ffffff".to_string()]);
        s.proxy.send_event(Event::ColorRequest(257, fmt_rgb()));
        assert_eq!(s.take_pty_writes(), vec!["1e1e1e".to_string()]);
    }

    #[test]
    fn size_query_uses_construction_dimensions() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        let format = Arc::new(|ws: WindowSize| format!("{}x{}", ws.num_cols, ws.num_lines));
        s.proxy.send_event(Event::TextAreaSizeRequest(format));
        assert_eq!(s.take_pty_writes(), vec!["10x3".to_string()]);
    }

    // ---- Task 8: 脏区跟踪 ----

    #[test]
    fn take_damage_first_frame_is_full() {
        // 首帧哨兵:term 的 damage 状态构造即 full,本应自然 Full;哨兵把该
        // 前提钉死在本层,不赌上游实现
        let mut s = Surface::new(ScreenSize::new(10, 5));
        assert_eq!(s.take_damage(), Damage::Full);
    }

    #[test]
    fn damage_reports_exactly_touched_rows_sorted_deduped() {
        let mut s = Surface::new(ScreenSize::new(10, 5));
        s.take_damage(); // 首帧哨兵烧掉
        s.feed(b"one"); // 行 0(光标同在行 0)
        s.feed(b"\x1b[3;1Htwo"); // HVP 到行 2 写入:行 0 旧光标位 + 行 2
        assert_eq!(s.take_damage(), Damage::Lines(vec![0, 2]));
    }

    #[test]
    fn damage_after_reset_reports_cursor_row_only() {
        // 无输入无移动的帧:上游 Term::damage() 恒伤当前光标行(保守正确),
        // 所以 Lines 空集经这条 API 不可达;空集分支由 frame 层单测覆盖
        let mut s = Surface::new(ScreenSize::new(10, 5));
        s.take_damage(); // 首帧哨兵
        assert_eq!(s.take_damage(), Damage::Lines(vec![0]));
    }

    #[test]
    fn resize_marks_full_damage() {
        // 结构性失配(行数/列数变化)必须全量:Term::resize 标 full,
        // take_damage 如实翻译
        let mut s = Surface::new(ScreenSize::new(10, 5));
        s.take_damage(); // 首帧哨兵
        s.feed(b"hello");
        s.resize(ScreenSize::new(20, 8));
        assert_eq!(s.take_damage(), Damage::Full);
    }

    #[test]
    fn clear_screen_marks_full_damage() {
        // ED 2(cls 等价):上游 clear_screen 走 mark_fully_damaged
        let mut s = Surface::new(ScreenSize::new(10, 5));
        s.take_damage(); // 首帧哨兵
        s.feed(b"hello");
        s.feed(b"\x1b[2J");
        assert_eq!(s.take_damage(), Damage::Full);
    }

    #[test]
    fn sentinel_consumes_damage_so_cursor_chain_stays_synced() {
        // 回归(Task 8 复审):哨兵若不消费 damage(),last_cursor 停在默认
        // (0,0),下一帧光标在非零行时会把 (0,0) 误伤进来。烧哨兵前先把
        // 光标挪到非零行——本用例恰好区分“消费过”与“只 reset 过”
        let mut s = Surface::new(ScreenSize::new(10, 5));
        s.feed(b"\x1b[3;1HX"); // 光标停行 2(哨兵燃烧前)
        assert_eq!(s.take_damage(), Damage::Full); // 哨兵
        assert_eq!(
            s.take_damage(),
            Damage::Lines(vec![2]),
            "last_cursor 已同步到行 2:不得出现 (0,0) 假伤"
        );
    }

    #[test]
    fn selection_spans_lines_and_clears() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        s.feed(b"hello\r\nworld");
        use alacritty_terminal::index::{Column, Line};
        // Side::Left = 从该格左缘起(含本格);Right 会跳到下一格右缘
        s.selection_begin(
            SelectionType::Simple,
            Point::new(Line(0), Column(2)),
            Side::Left,
        );
        s.selection_update(Point::new(Line(1), Column(3)), Side::Left);
        let text = s.selection_text().expect("选区应有文本");
        assert!(text.contains("llo"), "首行尾部: {text:?}");
        assert!(text.contains("wor"), "次行首部: {text:?}");
        let range = s.selection_range().expect("选区应有 range");
        assert!(range.start.line <= range.end.line, "to_range 已规范顺序");
        assert!(range.contains(Point::new(Line(0), Column(4))));
        assert!(!range.contains(Point::new(Line(0), Column(0))));
        s.selection_clear();
        assert!(s.selection_text().is_none());
        assert!(s.selection_range().is_none());
    }

    #[test]
    fn size_query_reports_default_then_set_cell_metrics() {
        let mut s = Surface::new(ScreenSize::new(10, 3));
        // 默认 8x16,与退役前的常量一致
        let format = Arc::new(|ws: WindowSize| {
            format!(
                "{}x{}x{}x{}",
                ws.num_cols, ws.num_lines, ws.cell_width, ws.cell_height
            )
        });
        s.proxy
            .send_event(Event::TextAreaSizeRequest(format.clone()));
        assert_eq!(s.take_pty_writes(), vec!["10x3x8x16".to_string()]);
        s.set_cell_metrics(12, 24);
        s.proxy.send_event(Event::TextAreaSizeRequest(format));
        assert_eq!(s.take_pty_writes(), vec!["10x3x12x24".to_string()]);
    }
}
