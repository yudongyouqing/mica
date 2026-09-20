//! Terminal surface: owns the screen state machine, turns raw pty bytes into
//! grid mutations, and collects events that must leave the terminal
//! (query replies go back to the pty, title changes go to the window).

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, Rgb};

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

/// Cell pixel metrics reported to terminal size queries; must stay in sync
/// with `wg_render::atlas` (M1 replaces both with font-derived metrics).
pub const CELL_WIDTH: u16 = 8;
pub const CELL_HEIGHT: u16 = 16;

/// Palette entries reported for OSC 10/11 default-ink queries.
/// Indices follow `NamedColor`: 256 = foreground, 257 = background.
fn default_rgb(index: usize) -> [u8; 3] {
    match index {
        257 => [0x1e, 0x1e, 0x1e],
        _ => [0xff, 0xff, 0xff],
    }
}

#[derive(Default)]
struct ProxyState {
    pty_writes: Vec<String>,
    title: Option<String>,
    size: Option<ScreenSize>,
}

/// `Term`'s event sink. Everything the terminal wants to say to the *outside*
/// world lands here so `Surface` can drain it deterministically.
#[derive(Clone, Default)]
pub(crate) struct EventProxy(Arc<Mutex<ProxyState>>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let mut state = self.0.lock().unwrap();
        match event {
            Event::PtyWrite(s) => state.pty_writes.push(s),
            Event::Title(title) => state.title = Some(title),
            Event::ColorRequest(index, format) => {
                let [r, g, b] = default_rgb(index);
                state.pty_writes.push(format(Rgb { r, g, b }));
            }
            Event::TextAreaSizeRequest(format) => {
                let size = state.size.unwrap_or(ScreenSize::new(80, 24));
                let ws = WindowSize {
                    num_cols: size.columns as u16,
                    num_lines: size.screen_lines as u16,
                    cell_width: CELL_WIDTH,
                    cell_height: CELL_HEIGHT,
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
        self.proxy.0.lock().unwrap().size = Some(size);
    }

    /// Drain replies that must be written back into the pty (DSR/OSC answers).
    pub fn take_pty_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.proxy.0.lock().unwrap().pty_writes)
    }

    pub fn take_title(&mut self) -> Option<String> {
        std::mem::take(&mut self.proxy.0.lock().unwrap().title)
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
}
