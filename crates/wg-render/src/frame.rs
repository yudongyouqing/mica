//! Grid -> render instances. Pure CPU; the GPU pass (Task 6) just draws them.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::Rgb;

use wg_core::surface::Surface;

use crate::atlas::atlas_glyph_index;
use crate::color::resolve;

pub const DEFAULT_FG: Rgb = Rgb {
    r: 0xff,
    g: 0xff,
    b: 0xff,
};
pub const DEFAULT_BG: Rgb = Rgb {
    r: 0x1e,
    g: 0x1e,
    b: 0x1e,
};

/// One drawable cell. Layout matches the WGSL instance inputs: three
/// `vec4<f32>` locations, stride 48.
/// `pos_glyph = [x_px, y_px, glyph_index, 0]`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CellInstance {
    pub pos_glyph: [f32; 4],
    pub fg: [f32; 4],
    pub bg: [f32; 4],
}

impl CellInstance {
    fn new(x_px: f32, y_px: f32, glyph: f32, fg: [u8; 3], bg: [u8; 3]) -> Self {
        Self {
            pos_glyph: [x_px, y_px, glyph, 0.0],
            fg: [
                fg[0] as f32 / 255.0,
                fg[1] as f32 / 255.0,
                fg[2] as f32 / 255.0,
                0.0,
            ],
            bg: [
                bg[0] as f32 / 255.0,
                bg[1] as f32 / 255.0,
                bg[2] as f32 / 255.0,
                0.0,
            ],
        }
    }
}

/// Snapshot the visible screen into draw instances. One instance per cell
/// (spaces included, so the background paints); M0 redraws everything.
pub fn build_instances(surface: &Surface) -> Vec<CellInstance> {
    let grid = surface.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    // 块状光标:光标所在格前景/背景互换(`Cursor.point` 是公开字段,
    // `Point.line.0: i32` 为屏幕相对行,0 = 屏幕顶)
    let cursor = grid.cursor.point;
    let cursor_line = usize::try_from(cursor.line.0).unwrap_or(usize::MAX);
    let cursor_col = cursor.column.0;
    let mut out = Vec::with_capacity(cols * rows);
    for line in 0..rows {
        for col in 0..cols {
            let cell = &grid[Line(line as i32)][Column(col)];
            let mut fg = resolve(cell.fg, DEFAULT_FG, DEFAULT_BG);
            let mut bg = resolve(cell.bg, DEFAULT_FG, DEFAULT_BG);
            // SGR 7 反色(PSReadLine 选中高亮依赖);其余 flags(BOLD/WIDE/…)M0 仍忽略
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if line == cursor_line && col == cursor_col {
                std::mem::swap(&mut fg, &mut bg);
            }
            out.push(CellInstance::new(
                col as f32 * crate::atlas::CELL_WIDTH as f32,
                line as f32 * crate::atlas::CELL_HEIGHT as f32,
                atlas_glyph_index(cell.c),
                fg,
                bg,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::BASE16;
    use wg_core::surface::ScreenSize;

    #[test]
    fn instance_count_is_cells_and_layout_is_pod_48() {
        let s = Surface::new(ScreenSize::new(4, 2));
        assert_eq!(build_instances(&s).len(), 8);
        assert_eq!(std::mem::size_of::<CellInstance>(), 48);
    }

    #[test]
    fn typed_characters_land_at_expected_positions() {
        let mut s = Surface::new(ScreenSize::new(10, 2));
        s.feed(b"OK");
        let inst = build_instances(&s);
        assert_eq!(inst[0].pos_glyph, [0.0, 0.0, atlas_glyph_index('O'), 0.0]);
        assert_eq!(inst[1].pos_glyph, [8.0, 0.0, atlas_glyph_index('K'), 0.0]);
        // 后续空格是 glyph 0
        assert_eq!(inst[2].pos_glyph[2], 0.0);
    }

    #[test]
    fn sgr_colors_flow_into_instances() {
        let mut s = Surface::new(ScreenSize::new(10, 2));
        s.feed(b"\x1b[31mX");
        let inst = build_instances(&s);
        assert_eq!(
            inst[0].fg,
            [
                BASE16[1][0] as f32 / 255.0,
                BASE16[1][1] as f32 / 255.0,
                BASE16[1][2] as f32 / 255.0,
                0.0
            ]
        );
    }

    #[test]
    fn second_row_offsets_by_cell_height() {
        let mut s = Surface::new(ScreenSize::new(4, 2));
        s.feed(b"a\r\nb");
        let inst = build_instances(&s);
        assert_eq!(inst[4].pos_glyph, [0.0, 16.0, atlas_glyph_index('b'), 0.0]);
    }

    #[test]
    fn sgr_reverse_video_swaps_fg_bg() {
        let mut s = Surface::new(ScreenSize::new(4, 2));
        s.feed(b"\x1b[7mA\x1b[mB");
        let inst = build_instances(&s);
        // 反色格:fg 变暗底、bg 变白
        assert_eq!(
            inst[0].fg,
            [
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0.0
            ]
        );
        assert_eq!(inst[0].bg, [1.0, 1.0, 1.0, 0.0]);
        // 紧随其后的普通格不受影响
        assert_eq!(inst[1].fg, [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(
            inst[1].bg,
            [
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0.0
            ]
        );
    }

    #[test]
    fn cursor_cell_swaps_fg_bg() {
        let mut s = Surface::new(ScreenSize::new(4, 2));
        s.feed(b"a");
        let inst = build_instances(&s);
        // 光标停在 (1,0):该格 fg 变暗底色、bg 变白(白块光标)
        assert_eq!(
            inst[1].fg,
            [
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0.0
            ]
        );
        assert_eq!(inst[1].bg, [1.0, 1.0, 1.0, 0.0]);
        // 非光标格保持默认:fg 白、bg 暗
        assert_eq!(inst[2].fg, [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(
            inst[2].bg,
            [
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0.0
            ]
        );
    }
}
