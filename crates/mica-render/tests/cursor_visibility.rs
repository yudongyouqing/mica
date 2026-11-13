//! A TUI may hide its text cursor while moving it to paint a thinking frame.
//! Those intermediate positions must not become visible terminal cursors.
use mica_core::config::palette::Palette;
use mica_core::surface::{CursorShape, ScreenSize, Surface};
use mica_render::font::{
    GlyphStyle,
    metrics::FontMetrics,
    router::{GlyphInfo, GlyphRouter},
};
use mica_render::frame::{CellInstance, RowInst, build_instances, build_rows, repack};

struct TestRouter;
impl GlyphRouter for TestRouter {
    fn route(&mut self, _: char, _: GlyphStyle) -> GlyphInfo {
        GlyphInfo {
            uv: [0.0, 0.0, 6.0, 12.0],
            size_px: [6.0, 12.0],
            offset_px: [0.0, 0.0],
            color: false,
        }
    }
}
fn metrics() -> FontMetrics {
    FontMetrics {
        cell_width: 8.0,
        line_height: 16.0,
        ascent: 13.0,
        descent: 3.0,
    }
}
fn full(surface: &Surface, cursor: Option<CursorShape>) -> Vec<CellInstance> {
    build_instances(
        surface,
        &mut TestRouter,
        &metrics(),
        &Palette::DEFAULT,
        None,
        0,
        cursor,
    )
}
fn update(
    surface: &mut Surface,
    rows: &mut Vec<RowInst>,
    cursor: Option<CursorShape>,
) -> Vec<CellInstance> {
    let damage = surface.take_damage();
    build_rows(
        surface,
        &mut TestRouter,
        &metrics(),
        &Palette::DEFAULT,
        None,
        0,
        cursor,
        &damage,
        rows,
    );
    repack(rows)
}

fn same_quads(left: &[CellInstance], right: &[CellInstance]) -> bool {
    // The full and row-cache paths may order disjoint rows' cursor bars
    // differently. Their actual quads/positions/colors must still agree.
    let mut left: Vec<_> = left.iter().map(bytemuck::bytes_of).collect();
    let mut right: Vec<_> = right.iter().map(bytemuck::bytes_of).collect();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

#[test]
fn dec_hidden_cursor_never_paints_intermediate_tui_positions() {
    for shape in [
        CursorShape::Block,
        CursorShape::Beam,
        CursorShape::Underline,
    ] {
        let mut surface = Surface::new(ScreenSize::new(12, 5));
        surface.feed(b"\x1b[?25l\x1b[4;7Hwait");
        assert!(
            full(&surface, Some(shape)) == full(&surface, None),
            "DEC private mode 25 must suppress {shape:?}, regardless of configured shape"
        );
    }
}

#[test]
fn hidden_thinking_frames_clear_old_cursor_and_do_not_leave_ghosts() {
    for shape in [
        CursorShape::Block,
        CursorShape::Beam,
        CursorShape::Underline,
    ] {
        let mut surface = Surface::new(ScreenSize::new(12, 5));
        surface.feed(b"ready");
        let mut rows = Vec::new();
        let visible = update(&mut surface, &mut rows, Some(shape));
        assert!(visible != full(&surface, None), "cursor starts visible");
        for bytes in [b"\x1b[?25l\x1b[4;9H".as_slice(), b"\x1b[2;3H", b"\x1b[5;2H"] {
            surface.feed(bytes);
            let hidden = update(&mut surface, &mut rows, Some(shape));
            assert!(
                hidden == full(&surface, None),
                "hidden {shape:?} cursor movement must neither paint nor retain an old row cursor"
            );
        }
        // A blink wake-up may request a cursor again while the application
        // still has it hidden; it must remain invisible.
        assert!(update(&mut surface, &mut rows, Some(shape)) == full(&surface, None));
        surface.feed(b"\x1b[?25h");
        let shown = update(&mut surface, &mut rows, Some(shape));
        assert!(
            same_quads(&shown, &full(&surface, Some(shape))),
            "show restores the current position"
        );
        assert!(
            shown != full(&surface, None),
            "show must not get stuck invisible"
        );
    }
}

#[test]
fn visible_mode_still_respects_blink_off_phase() {
    let mut surface = Surface::new(ScreenSize::new(12, 5));
    let mut rows = Vec::new();
    update(&mut surface, &mut rows, Some(CursorShape::Block));
    assert!(update(&mut surface, &mut rows, None) == full(&surface, None));
}
