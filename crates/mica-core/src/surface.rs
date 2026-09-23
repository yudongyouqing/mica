//! Terminal surface: owns the screen state machine, turns raw pty bytes into
//! grid mutations, and collects events that must leave the terminal
//! (query replies go back to the pty, title changes go to the window).

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use crate::config::palette::Palette;

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
