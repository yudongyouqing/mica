//! Grid -> render instances. Pure CPU; the GPU pass (Task 6) just draws them.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::Rgb;

use mica_core::surface::Surface;

use crate::color::resolve;
use crate::font::GlyphStyle;
use crate::font::metrics::FontMetrics;
use crate::font::router::GlyphRouter;

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

/// One drawable quad. `pos_uv = [x_px, y_px, u, v]`(uv 为图集内左上,0..1),
/// `size_uv = [w_px, h_px, uw, uvh]`(像素尺寸 + uv 尺寸,0..1)。
/// 背景 quad 与空白格均用 `blank` 形态:uv 尺寸 0,shader 墨恒 0。
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CellInstance {
    pub pos_uv: [f32; 4],
    pub size_uv: [f32; 4],
    pub fg: [f32; 4],
    pub bg: [f32; 4],
}

/// `[u8;3]` 终端色 → shader 的 `[f32;4]`(沿用 M0 的 u8/255 换算,alpha 恒 0)。
fn to_color(rgb: [u8; 3]) -> [f32; 4] {
    [
        rgb[0] as f32 / 255.0,
        rgb[1] as f32 / 255.0,
        rgb[2] as f32 / 255.0,
        0.0,
    ]
}

/// 空白实例:整格背景 quad,uv 尺寸 0 → shader ink 恒 0。
/// 不占图集、不经路由(router 无 blank 概念)。
fn blank(x_px: f32, y_px: f32, metrics: &FontMetrics, fg: [u8; 3], bg: [u8; 3]) -> CellInstance {
    CellInstance {
        pos_uv: [x_px, y_px, 0.0, 0.0],
        size_uv: [metrics.cell_width, metrics.line_height, 0.0, 0.0],
        fg: to_color(fg),
        bg: to_color(bg),
    }
}

/// Snapshot the visible screen into draw instances. Two-instance contract:
/// EVERY cell first emits a full-cell background quad (`blank`, ink_mask 0);
/// non-blank cells then emit the glyph quad on top of it. Blank cells
/// (space / `WIDE_CHAR_SPACER`) stay a single instance. The shader has no
/// blending, so the cell background would otherwise only paint inside the
/// glyph bbox — INVERSE/selection/colored backgrounds would leak the clear
/// color everywhere else. Glyph quads redundantly paint their own bg over
/// the bbox (harmless overdraw); painter's order within the single instanced
/// draw guarantees bg below glyph. M0 redraws everything.
///
/// 语义:空格与 `WIDE_CHAR_SPACER` → 仅 `blank`(背景正常);其余字符先
/// blank 再经 router 出字形,绘制位置 = 格左上 + `offset_px`、尺寸 =
/// `size_px`(宽字形天然 2 格宽,宽度来自 WIDE_CHAR 格语义而非
/// `char_width`);INVERSE 与光标各交换一次 fg/bg——先反色后光标,
/// 双交换抵消,光标在反色选区上仍可辨(已裁定顺序)。
pub fn build_instances(
    surface: &Surface,
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
) -> Vec<CellInstance> {
    let grid = surface.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let cursor = grid.cursor.point;
    let cursor_line = usize::try_from(cursor.line.0).unwrap_or(usize::MAX);
    let cursor_col = cursor.column.0;
    // 上界 = 每格 2 实例(bg + 字形;空白格只有 bg)
    let mut out = Vec::with_capacity(cols * rows * 2);
    for line in 0..rows {
        for col in 0..cols {
            let cell = &grid[Line(line as i32)][Column(col)];
            let mut fg = resolve(cell.fg, DEFAULT_FG, DEFAULT_BG);
            let mut bg = resolve(cell.bg, DEFAULT_FG, DEFAULT_BG);
            // 先反色后光标(双交换抵消,光标在选区仍可辨——已裁定顺序)
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if line == cursor_line && col == cursor_col {
                std::mem::swap(&mut fg, &mut bg);
            }
            let x = col as f32 * metrics.cell_width;
            let y = line as f32 * metrics.line_height;
            // bg quad 恒在先:整格背景,不依赖字形 bbox 覆盖
            out.push(blank(x, y, metrics, fg, bg));
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.c == ' ' {
                continue;
            }
            let g = router.route(cell.c, GlyphStyle::from_flags(cell.flags));
            out.push(CellInstance {
                pos_uv: [x + g.offset_px[0], y + g.offset_px[1], g.uv[0], g.uv[1]],
                size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                fg: to_color(fg),
                bg: to_color(bg),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mica_core::surface::ScreenSize;

    #[test]
    fn instance_layout_is_pod_64() {
        assert_eq!(std::mem::size_of::<CellInstance>(), 64);
        assert_eq!(std::mem::offset_of!(CellInstance, pos_uv), 0);
        assert_eq!(std::mem::offset_of!(CellInstance, size_uv), 16);
        assert_eq!(std::mem::offset_of!(CellInstance, fg), 32);
        assert_eq!(std::mem::offset_of!(CellInstance, bg), 48);
    }

    /// 可编程假路由:每个字符回固定字形,便于断言几何。
    struct FakeRouter {
        atlas_w: f32,
        atlas_h: f32,
        glyph_w: f32,
        glyph_h: f32,
        wide: fn(char) -> bool,
    }
    impl crate::font::router::GlyphRouter for FakeRouter {
        fn route(
            &mut self,
            ch: char,
            _style: crate::font::GlyphStyle,
        ) -> crate::font::router::GlyphInfo {
            let w = if (self.wide)(ch) {
                self.glyph_w * 2.0
            } else {
                self.glyph_w
            };
            crate::font::router::GlyphInfo {
                uv: [0.25, 0.25, w / self.atlas_w, self.glyph_h / self.atlas_h],
                size_px: [w, self.glyph_h],
                offset_px: [1.0, 2.0],
            }
        }
    }

    fn metrics_8x16() -> crate::font::metrics::FontMetrics {
        crate::font::metrics::FontMetrics {
            cell_width: 8.0,
            line_height: 16.0,
            ascent: 12.0,
            descent: 4.0,
        }
    }

    #[test]
    fn every_cell_yields_instance_with_background() {
        let s = Surface::new(ScreenSize::new(4, 2));
        let mut r = FakeRouter {
            atlas_w: 64.0,
            atlas_h: 64.0,
            glyph_w: 6.0,
            glyph_h: 12.0,
            wide: |_| false,
        };
        assert_eq!(build_instances(&s, &mut r, &metrics_8x16()).len(), 8);
    }

    #[test]
    fn typed_glyph_uses_router_geometry() {
        let mut s = Surface::new(ScreenSize::new(4, 1));
        s.feed(b"A");
        let mut r = FakeRouter {
            atlas_w: 64.0,
            atlas_h: 64.0,
            glyph_w: 6.0,
            glyph_h: 12.0,
            wide: |_| false,
        };
        let inst = build_instances(&s, &mut r, &metrics_8x16());
        // 双实例约定:格 0 = A。inst[0] 整格背景 quad(先于字形)
        assert_eq!(inst[0].pos_uv, [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(inst[0].size_uv, [8.0, 16.0, 0.0, 0.0]);
        // inst[1] = A 的字形:格左上 (0,0) + offset (1,2);尺寸 6x12;uv 来自路由
        assert_eq!(inst[1].pos_uv, [1.0, 2.0, 0.25, 0.25]);
        assert_eq!(inst[1].size_uv, [6.0, 12.0, 6.0 / 64.0, 12.0 / 64.0]);
        assert_eq!(inst.len(), 5, "A(2) + 3 空格(各 1)");
        // 格 1..3 空格:单实例 blank——尺寸 = 格尺寸,uv 尺寸 0
        for (i, x) in [8.0, 16.0, 24.0].into_iter().enumerate() {
            let b = &inst[2 + i];
            assert_eq!(b.pos_uv[0], x, "空格格 {i} x");
            assert_eq!(b.size_uv, [8.0, 16.0, 0.0, 0.0], "空格格 {i} 整格背景");
        }
    }

    #[test]
    fn wide_char_routes_wide_spacer_blank() {
        let mut s = Surface::new(ScreenSize::new(4, 1));
        s.feed("中".as_bytes()); // WIDE_CHAR + SPACER(alacritty 网格语义)
        let mut r = FakeRouter {
            atlas_w: 64.0,
            atlas_h: 64.0,
            glyph_w: 6.0,
            glyph_h: 12.0,
            wide: |_| true,
        };
        let inst = build_instances(&s, &mut r, &metrics_8x16());
        assert_eq!(inst.len(), 5, "宽字形(2) + spacer(1) + 2 空格");
        // inst[0] = 格 0 的整格背景;inst[1] = 宽字形在其上
        assert_eq!(inst[0].pos_uv, [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(inst[0].size_uv, [8.0, 16.0, 0.0, 0.0]);
        assert_eq!(inst[1].size_uv[0], 12.0, "宽字形横跨 2 格");
        // inst[2] = spacer 格(格 1)为空白背景
        assert_eq!(inst[2].pos_uv[0], 8.0, "spacer 在格 1");
        assert_eq!(inst[2].size_uv, [8.0, 16.0, 0.0, 0.0]);
    }

    #[test]
    fn bold_style_routed_and_inverse_cursor_kept() {
        let mut s = Surface::new(ScreenSize::new(4, 1));
        s.feed(b"\x1b[1mA\x1b[m\x1b[7mB");
        let seen_styles = std::cell::RefCell::new(Vec::new());
        struct RecordingRouter<'a> {
            seen: &'a std::cell::RefCell<Vec<crate::font::GlyphStyle>>,
            inner: FakeRouter,
        }
        impl crate::font::router::GlyphRouter for RecordingRouter<'_> {
            fn route(
                &mut self,
                ch: char,
                style: crate::font::GlyphStyle,
            ) -> crate::font::router::GlyphInfo {
                self.seen.borrow_mut().push(style);
                self.inner.route(ch, style)
            }
        }
        let mut r = RecordingRouter {
            seen: &seen_styles,
            inner: FakeRouter {
                atlas_w: 64.0,
                atlas_h: 64.0,
                glyph_w: 6.0,
                glyph_h: 12.0,
                wide: |_| false,
            },
        };
        let inst = build_instances(&s, &mut r, &metrics_8x16());
        // 顺序:A@0(bold)、B@1(反色)、光标@2(空格)、空格@3
        assert_eq!(inst.len(), 6, "A、B 各 2(bg+字形),2 空格各 1");
        // 样式确实到达路由:两个非空格字形,第一个带 bold(bg quad 不经路由)
        assert_eq!(
            *seen_styles.borrow(),
            vec![
                crate::font::GlyphStyle {
                    bold: true,
                    italic: false
                },
                crate::font::GlyphStyle::PLAIN,
            ]
        );
        // inst[0] = A 的整格背景(默认色,fg 白、bg 暗)
        assert_eq!(inst[0].size_uv, [8.0, 16.0, 0.0, 0.0]);
        assert_eq!(inst[0].fg, [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(
            inst[0].bg,
            [
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0.0
            ]
        );
        // inst[1] = A 的字形(bold 样式的几何)
        assert_eq!(inst[1].pos_uv, [1.0, 2.0, 0.25, 0.25]);
        assert_eq!(inst[1].size_uv, [6.0, 12.0, 6.0 / 64.0, 12.0 / 64.0]);
        // inst[2] = B 的背景 quad:INVERSE 交换一次——fg 暗底、bg 白
        assert_eq!(
            inst[2].fg,
            [
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0.0
            ]
        );
        assert_eq!(inst[2].bg, [1.0, 1.0, 1.0, 0.0]);
        // inst[3] = B 的字形:同一对交换色(字形自绘 bg 盖 bbox,无害叠绘)
        assert_eq!(inst[3].pos_uv, [9.0, 2.0, 0.25, 0.25]);
        assert_eq!(
            inst[3].fg,
            [
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0.0
            ]
        );
        assert_eq!(inst[3].bg, [1.0, 1.0, 1.0, 0.0]);
        // inst[4] = 光标格 (2,0) 的空白背景:默认色上仅光标交换一次
        // (无 INVERSE,无双交换)→ 白块光标:fg 暗、bg 白
        assert_eq!(inst[4].pos_uv, [16.0, 0.0, 0.0, 0.0]);
        assert_eq!(
            inst[4].fg,
            [
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0.0
            ]
        );
        assert_eq!(inst[4].bg, [1.0, 1.0, 1.0, 0.0]);
    }
}
