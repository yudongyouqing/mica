//! Grid -> render instances. Pure CPU; the GPU pass (Task 6) just draws them.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;

use mica_core::config::palette::Palette;
use mica_core::surface::Surface;

use crate::color::resolve;
use crate::font::GlyphStyle;
use crate::font::metrics::FontMetrics;
use crate::font::router::GlyphRouter;

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

/// Snapshot the visible screen into draw instances. Two-instance, two-pass
/// contract: pass 1 emits a full-cell background quad (`blank`, ink_mask 0)
/// for EVERY cell, row-major; pass 2 then emits the glyph quad for every
/// non-blank cell (blank cells — space / `WIDE_CHAR_SPACER` — emit nothing
/// in pass 2). Both segments share the single instance buffer and instanced
/// draw. The shader has no blending (last-write-wins), so the interleaved
/// per-cell order would let the NEXT cell's bg quad erase a wide glyph's
/// right half — and any earlier glyph's bleed (italic overhang, descenders).
/// Emitting all bgs before all glyphs makes "every glyph above every bg"
/// true globally, not just within a cell. Glyph quads redundantly paint
/// their own bg over the bbox (harmless overdraw). M0 redraws everything.
///
/// 语义:第一段每格发整格背景 quad(行优先);第二段按格序对非空白格经
/// router 出字形(空格与 `WIDE_CHAR_SPACER` 不发),绘制位置 = 格左上 +
/// `offset_px`、尺寸 = `size_px`(宽字形天然 2 格宽,宽度来自 WIDE_CHAR
/// 格语义而非 `char_width`);INVERSE 与光标各交换一次 fg/bg——先反色后
/// 光标,双交换抵消,光标在反色选区上仍可辨(已裁定顺序)。
pub fn build_instances(
    surface: &Surface,
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
) -> Vec<CellInstance> {
    let grid = surface.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let cursor = grid.cursor.point;
    let cursor_line = usize::try_from(cursor.line.0).unwrap_or(usize::MAX);
    let cursor_col = cursor.column.0;
    // 上界 = 每格 2 实例(bg 段 + 字形段;空白格只有 bg)
    let mut out = Vec::with_capacity(cols * rows * 2);
    // 字形段先攒后拼:保证全部 bg 先于全部字形(跨格画家序,见 doc)
    let mut glyphs = Vec::with_capacity(cols * rows);
    for line in 0..rows {
        for col in 0..cols {
            let cell = &grid[Line(line as i32)][Column(col)];
            let mut fg = resolve(cell.fg, palette);
            let mut bg = resolve(cell.bg, palette);
            // 先反色后光标(双交换抵消,光标在选区仍可辨——已裁定顺序)
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if line == cursor_line && col == cursor_col {
                std::mem::swap(&mut fg, &mut bg);
            }
            let x = col as f32 * metrics.cell_width;
            let y = line as f32 * metrics.line_height;
            // bg 段:整格背景,不依赖字形 bbox 覆盖
            out.push(blank(x, y, metrics, fg, bg));
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.c == ' ' {
                continue;
            }
            let g = router.route(cell.c, GlyphStyle::from_flags(cell.flags));
            glyphs.push(CellInstance {
                pos_uv: [x + g.offset_px[0], y + g.offset_px[1], g.uv[0], g.uv[1]],
                size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                fg: to_color(fg),
                bg: to_color(bg),
            });
        }
    }
    out.extend(glyphs);
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
        assert_eq!(
            build_instances(&s, &mut r, &metrics_8x16(), &Palette::DEFAULT).len(),
            8
        );
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
        let inst = build_instances(&s, &mut r, &metrics_8x16(), &Palette::DEFAULT);
        // 两段式:bg 段 inst[0..4) 行优先每格一整格 quad,字形段随后
        assert_eq!(inst.len(), 5, "4 bg + 1 字形");
        assert_eq!(inst[0].pos_uv, [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(inst[0].size_uv, [8.0, 16.0, 0.0, 0.0]);
        // 字形段 inst[4] = A 的字形:格左上 (0,0) + offset (1,2);尺寸 6x12;uv 来自路由
        assert_eq!(inst[4].pos_uv, [1.0, 2.0, 0.25, 0.25]);
        assert_eq!(inst[4].size_uv, [6.0, 12.0, 6.0 / 64.0, 12.0 / 64.0]);
        // bg 段其余:格 1..3 空格 blank——尺寸 = 格尺寸,uv 尺寸 0
        for (i, x) in [8.0, 16.0, 24.0].into_iter().enumerate() {
            let b = &inst[1 + i];
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
        let inst = build_instances(&s, &mut r, &metrics_8x16(), &Palette::DEFAULT);
        assert_eq!(inst.len(), 5, "4 bg + 1 宽字形(spacer 不出字形)");
        // bg 段:格 0 与 spacer 格(格 1)都是整格 blank
        assert_eq!(inst[0].pos_uv, [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(inst[0].size_uv, [8.0, 16.0, 0.0, 0.0]);
        assert_eq!(inst[1].pos_uv[0], 8.0, "spacer 在格 1");
        assert_eq!(inst[1].size_uv, [8.0, 16.0, 0.0, 0.0]);
        // 字形段:宽字形在全部 bg 之后,横跨 2 格
        assert_eq!(inst[4].size_uv[0], 12.0, "宽字形横跨 2 格");
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
        let inst = build_instances(&s, &mut r, &metrics_8x16(), &Palette::DEFAULT);
        // 顺序:A@0(bold)、B@1(反色)、光标@2(空格)、空格@3
        // 两段式:bg 段 [0..4) = A、B、光标格、空格;字形段 [4..6) = A、B
        assert_eq!(inst.len(), 6, "4 bg + A、B 两个字形");
        // 样式确实到达路由:两个非空格字形,第一个带 bold(bg 段不经路由)
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
        // inst[1] = B 的背景 quad:INVERSE 交换一次——fg 暗底、bg 白
        assert_eq!(inst[1].pos_uv[0], 8.0, "B 在格 1");
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
        // inst[2] = 光标格 (2,0) 的空白背景:默认色上仅光标交换一次
        // (无 INVERSE,无双交换)→ 白块光标:fg 暗、bg 白
        assert_eq!(inst[2].pos_uv[0], 16.0);
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
        // 字形段 inst[4] = A 的字形(bold 样式的几何)
        assert_eq!(inst[4].pos_uv, [1.0, 2.0, 0.25, 0.25]);
        assert_eq!(inst[4].size_uv, [6.0, 12.0, 6.0 / 64.0, 12.0 / 64.0]);
        // inst[5] = B 的字形:同一对交换色(字形自绘 bg 盖 bbox,无害叠绘)
        assert_eq!(inst[5].pos_uv, [9.0, 2.0, 0.25, 0.25]);
        assert_eq!(
            inst[5].fg,
            [
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0x1e as f32 / 255.0,
                0.0
            ]
        );
        assert_eq!(inst[5].bg, [1.0, 1.0, 1.0, 0.0]);
    }

    /// 跨格画家序回归:旧逐格交错序下,spacer 格的 bg 后发,blend:None
    /// 直接擦掉宽字形右半。两段式下全部 bg 必须先于全部字形。
    #[test]
    fn wide_glyph_instance_follows_all_backgrounds() {
        let mut s = Surface::new(ScreenSize::new(4, 1));
        s.feed("中".as_bytes());
        let mut r = FakeRouter {
            atlas_w: 64.0,
            atlas_h: 64.0,
            glyph_w: 6.0,
            glyph_h: 12.0,
            wide: |_| true,
        };
        let inst = build_instances(&s, &mut r, &metrics_8x16(), &Palette::DEFAULT);
        // bg 段:全部 4 格在前,均整格 blank(uv 尺寸 0),行优先
        for (i, cell) in inst.iter().take(4).enumerate() {
            assert_eq!(
                cell.size_uv,
                [8.0, 16.0, 0.0, 0.0],
                "inst[{i}] 应为整格背景"
            );
            assert_eq!(cell.pos_uv[0], i as f32 * 8.0, "inst[{i}] 行优先格 x");
        }
        // 字形段:宽字形在全部 bg 之后(索引 4),横跨 2 格,其后无实例
        assert_eq!(inst.len(), 5, "宽字形之后不得再有任何实例");
        assert_eq!(inst[4].size_uv, [12.0, 12.0, 12.0 / 64.0, 12.0 / 64.0]);
    }

    /// 实例颜色必须来自传入的 palette,而非任何内置常量:
    /// 槽 1 喂红色前景、bg 换浅色,两段(bg quad + 字形)都随行。
    #[test]
    fn instance_colors_flow_from_palette() {
        let mut s = Surface::new(ScreenSize::new(4, 1));
        s.feed(b"\x1b[31mA");
        let mut r = FakeRouter {
            atlas_w: 64.0,
            atlas_h: 64.0,
            glyph_w: 6.0,
            glyph_h: 12.0,
            wide: |_| false,
        };
        let mut pal = Palette::DEFAULT;
        pal.apply_pair("palette", "1=#112233").unwrap();
        pal.apply_pair("background", "#abcdef").unwrap();
        let inst = build_instances(&s, &mut r, &metrics_8x16(), &pal);
        // bg 段 inst[0]:A 的背景 = palette.bg(浅色),前景 = 槽 1
        assert_eq!(
            inst[0].bg,
            [
                0xab as f32 / 255.0,
                0xcd as f32 / 255.0,
                0xef as f32 / 255.0,
                0.0
            ]
        );
        assert_eq!(
            inst[0].fg,
            [
                0x11 as f32 / 255.0,
                0x22 as f32 / 255.0,
                0x33 as f32 / 255.0,
                0.0
            ]
        );
        // 字形段 inst[4]:同一对颜色(自绘 bg 盖 bbox,无害叠绘)
        assert_eq!(inst[4].fg, inst[0].fg);
        assert_eq!(inst[4].bg, inst[0].bg);
    }
}
