//! Grid -> render instances. Pure CPU; the GPU pass (Task 6) just draws them.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::grid::Grid;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::cell::Flags;

use mica_core::config::palette::Palette;
use mica_core::surface::{Damage, Surface};

use crate::color::resolve;
use crate::font::GlyphStyle;
use crate::font::metrics::FontMetrics;
use crate::font::router::GlyphRouter;

/// One drawable quad. `pos_uv = [x_px, y_px, u, v]`(uv 为图集内左上,0..1),
/// `size_uv = [w_px, h_px, uw, uvh]`(像素尺寸 + uv 尺寸,0..1)。
/// 背景 quad 与空白格均用 `blank` 形态:uv 尺寸 0,shader 墨恒 0。
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, bytemuck::Pod, bytemuck::Zeroable)]
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
    display_offset: usize,
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
            let cell = &grid[Line(line as i32 - display_offset as i32)][Column(col)];
            let mut fg = resolve(cell.fg, palette);
            let mut bg = resolve(cell.bg, palette);
            // 先反色后光标(双交换抵消,光标在选区仍可辨——已裁定顺序)
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if display_offset == 0 && line == cursor_line && col == cursor_col {
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

/// 一行的两段实例:bg 整格段(行内每格一实例)+ 字形段(仅非空白格)。
/// 行主序持有;**全局**两遍发射契约(全部 bg 先于全部字形)由 [`repack`]
/// 平铺时恢复——行内只保序,平铺才定序。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RowInst {
    pub bg: Vec<CellInstance>,
    pub glyphs: Vec<CellInstance>,
}

/// 行级重建入口(Task 8),按 `damage` 分路:
///
/// - `Full`:全部行重建(与 [`build_instances`] 全量黄金源逐字节等价,
///   黄金对拍测试锁定);
/// - `Lines(rows)`:只重建列出的行——受伤行取**当前** grid 状态,与上次
///   全量的先后无关;未伤行原样保留,路由缓存兜住重复字形的光栅化成本;
/// - `Lines(空)`:纯 no-op(零路由调用,行缓存原样)。上游 `Term::damage()`
///   恒伤光标行,空集一般只出现在手动构造/兜底路径,分支仍须正确。
///
/// 结构守恒:`rows.len()` 必须等于视口行数;不符(resize 后、首建、调用方
/// 维护失误)一律退化为全量重建——行数错位是结构性失配,增量修不回来。
pub fn build_rows(
    surface: &Surface,
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
    display_offset: usize,
    damage: &Damage,
    rows: &mut Vec<RowInst>,
) {
    let grid = surface.grid();
    let screen_lines = grid.screen_lines();
    if rows.len() != screen_lines {
        rebuild_all(grid, router, metrics, palette, display_offset, rows);
        return;
    }
    match damage {
        Damage::Full => rebuild_all(grid, router, metrics, palette, display_offset, rows),
        Damage::Lines(lines) => {
            for &line in lines {
                if line < screen_lines {
                    rows[line] = build_row(grid, line, router, metrics, palette, display_offset);
                }
            }
        }
    }
}

/// 全量重建 `rows` 为恰好视口行数(清尾,适配 resize 缩行)。
fn rebuild_all(
    grid: &Grid<Cell>,
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
    display_offset: usize,
    rows: &mut Vec<RowInst>,
) {
    rows.clear();
    for line in 0..grid.screen_lines() {
        rows.push(build_row(
            grid,
            line,
            router,
            metrics,
            palette,
            display_offset,
        ));
    }
}

/// 单行两段发射:bg 段(每格一整格 quad)先行,字形段随后。与
/// [`build_instances`](全量黄金源)的行内逻辑逐格对齐——两处必须同步修改,
/// 黄金对拍测试锁定等价。
fn build_row(
    grid: &Grid<Cell>,
    line: usize,
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
    display_offset: usize,
) -> RowInst {
    let cols = grid.columns();
    let cursor = grid.cursor.point;
    let cursor_line = usize::try_from(cursor.line.0).unwrap_or(usize::MAX);
    let cursor_col = cursor.column.0;
    let mut row = RowInst::default();
    row.bg.reserve(cols);
    for col in 0..cols {
        let cell = &grid[Line(line as i32 - display_offset as i32)][Column(col)];
        let mut fg = resolve(cell.fg, palette);
        let mut bg = resolve(cell.bg, palette);
        // 先反色后光标(双交换抵消,光标在反色上仍可辨——已裁定顺序)
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if display_offset == 0 && line == cursor_line && col == cursor_col {
            std::mem::swap(&mut fg, &mut bg);
        }
        let x = col as f32 * metrics.cell_width;
        let y = line as f32 * metrics.line_height;
        row.bg.push(blank(x, y, metrics, fg, bg));
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.c == ' ' {
            continue;
        }
        let g = router.route(cell.c, GlyphStyle::from_flags(cell.flags));
        row.glyphs.push(CellInstance {
            pos_uv: [x + g.offset_px[0], y + g.offset_px[1], g.uv[0], g.uv[1]],
            size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
            fg: to_color(fg),
            bg: to_color(bg),
        });
    }
    row
}

/// 平铺行结构为单实例缓冲,恢复全局两遍发射序:先所有行的 bg 段(行主序
/// 连续),后所有行的字形段——与 [`build_instances`]/管线契约逐位一致
/// (黄金对拍锁定)。逐帧分配暂留(计划的暂记账),缓冲复用另案。
pub fn repack(rows: &[RowInst]) -> Vec<CellInstance> {
    let bg_total: usize = rows.iter().map(|r| r.bg.len()).sum();
    let glyph_total: usize = rows.iter().map(|r| r.glyphs.len()).sum();
    let mut out = Vec::with_capacity(bg_total + glyph_total);
    for row in rows {
        out.extend_from_slice(&row.bg);
    }
    for row in rows {
        out.extend_from_slice(&row.glyphs);
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
        assert_eq!(
            build_instances(&s, &mut r, &metrics_8x16(), &Palette::DEFAULT, 0).len(),
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
        let inst = build_instances(&s, &mut r, &metrics_8x16(), &Palette::DEFAULT, 0);
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
        let inst = build_instances(&s, &mut r, &metrics_8x16(), &Palette::DEFAULT, 0);
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
        let inst = build_instances(&s, &mut r, &metrics_8x16(), &Palette::DEFAULT, 0);
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
        let inst = build_instances(&s, &mut r, &metrics_8x16(), &Palette::DEFAULT, 0);
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
        let inst = build_instances(&s, &mut r, &metrics_8x16(), &pal, 0);
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

    // ---- Task 8: 行级脏区重建 + repack ----

    fn bytes(insts: &[CellInstance]) -> &[u8] {
        bytemuck::cast_slice(insts)
    }

    /// 计数路由:转调 FakeRouter。`calls` = route 原始调用次数(受伤行重发
    /// 必然重复路由),`fresh` = 首见 (char, style) 数——模拟 DwriteRouter 的
    /// 图集缓存语义:重复字形命中缓存,不再产生真实光栅化工作。
    struct CountingRouter<'a> {
        calls: &'a std::cell::Cell<usize>,
        fresh: &'a std::cell::Cell<usize>,
        seen: std::collections::HashSet<(char, crate::font::GlyphStyle)>,
        inner: FakeRouter,
    }
    impl crate::font::router::GlyphRouter for CountingRouter<'_> {
        fn route(
            &mut self,
            ch: char,
            style: crate::font::GlyphStyle,
        ) -> crate::font::router::GlyphInfo {
            self.calls.set(self.calls.get() + 1);
            if self.seen.insert((ch, style)) {
                self.fresh.set(self.fresh.get() + 1);
            }
            self.inner.route(ch, style)
        }
    }

    fn counting_router<'a>(
        calls: &'a std::cell::Cell<usize>,
        fresh: &'a std::cell::Cell<usize>,
    ) -> CountingRouter<'a> {
        CountingRouter {
            calls,
            fresh,
            seen: std::collections::HashSet::new(),
            inner: FakeRouter {
                atlas_w: 64.0,
                atlas_h: 64.0,
                glyph_w: 6.0,
                glyph_h: 12.0,
                wide: |_| true, // 空格不路由,wide 与否只影响几何,计数不受影响
            },
        }
    }

    /// 黄金对拍(计划 Step 4):Full 全量的 rows → repack 必须与黄金源
    /// build_instances 逐字节等价——两遍发射契约没有在行级重排中漂移。
    /// 内容覆盖:普通字符、SGR 颜色、反色、bold、宽字形(含 spacer)、
    /// 光标格与滚屏内容。
    #[test]
    fn golden_full_repack_matches_build_instances_byte_for_byte() {
        let mut s = Surface::new(ScreenSize::new(6, 4));
        s.feed(b"A\x1b[31mB\x1b[7mC\r\n");
        s.feed("中D".as_bytes());
        s.feed(b"\r\n\r\n\x1b[1mE");
        let calls = std::cell::Cell::new(0);
        let fresh = std::cell::Cell::new(0);
        let mut router = counting_router(&calls, &fresh);
        let damage = s.take_damage();
        assert_eq!(damage, mica_core::surface::Damage::Full, "首帧哨兵必全量");
        let mut rows = Vec::new();
        build_rows(
            &s,
            &mut router,
            &metrics_8x16(),
            &Palette::DEFAULT,
            0,
            &damage,
            &mut rows,
        );
        let repacked = repack(&rows);
        let golden = build_instances(&s, &mut router, &metrics_8x16(), &Palette::DEFAULT, 0);
        assert_eq!(
            bytes(&repacked),
            bytes(&golden),
            "repack(build_rows(Full)) 必须与 build_instances 逐字节一致"
        );
    }

    /// 行级增量:只重建受损行——路由调用次数作证,未伤行的实例位图原样。
    #[test]
    fn partial_damage_rebuilds_only_damaged_rows() {
        let mut s = Surface::new(ScreenSize::new(4, 3));
        s.feed(b"AB\x1b[3;1HC"); // 行 0 写 AB,行 2 写 C(光标停行 2)
        let calls = std::cell::Cell::new(0);
        let fresh = std::cell::Cell::new(0);
        let mut router = counting_router(&calls, &fresh);
        let mut rows = Vec::new();
        let damage = s.take_damage();
        assert_eq!(damage, mica_core::surface::Damage::Full, "首帧哨兵必全量");
        build_rows(
            &s,
            &mut router,
            &metrics_8x16(),
            &Palette::DEFAULT,
            0,
            &damage,
            &mut rows,
        );
        assert_eq!(calls.get(), 3, "全量:A、B、C 各路由一次");
        assert_eq!(fresh.get(), 3, "全量首见:A、B、C");
        let row0_bg = bytes(&rows[0].bg).to_vec();
        let row0_glyphs = bytes(&rows[0].glyphs).to_vec();
        let row1 = rows[1].clone();

        s.feed(b"D"); // 光标已停行 2:只伤行 2
        let damage = s.take_damage();
        assert_eq!(damage, mica_core::surface::Damage::Lines(vec![2]));
        calls.set(0);
        build_rows(
            &s,
            &mut router,
            &metrics_8x16(),
            &Palette::DEFAULT,
            0,
            &damage,
            &mut rows,
        );
        assert_eq!(calls.get(), 2, "受伤行整行重发:C(缓存命中)+ D");
        assert_eq!(fresh.get(), 4, "图集缓存兜底:仅 D 是新字形");
        assert_eq!(
            bytes(&rows[0].bg),
            &row0_bg[..],
            "未伤行 0 的 bg 段不得变化"
        );
        assert_eq!(
            bytes(&rows[0].glyphs),
            &row0_glyphs[..],
            "未伤行 0 的字形段不得变化"
        );
        assert_eq!(rows[1], row1, "未伤行 1 原样");
        // 受伤行拿到的是当前 grid 状态
        assert_eq!(rows[2].glyphs.len(), 2, "行 2 现有 C、D 两个字形");
    }

    /// 空 Lines 脏区 = 纯 no-op:零路由调用,行缓存位图原样。
    /// (上游 Term::damage() 恒伤光标行,空集经 Surface::take_damage 不可达;
    /// 此分支是"无变化不重绘"的库级兜底。)
    #[test]
    fn empty_damage_is_no_op() {
        let s = Surface::new(ScreenSize::new(2, 2));
        let calls = std::cell::Cell::new(0);
        let fresh = std::cell::Cell::new(0);
        let mut router = counting_router(&calls, &fresh);
        let mut rows = Vec::new();
        build_rows(
            &s,
            &mut router,
            &metrics_8x16(),
            &Palette::DEFAULT,
            0,
            &mica_core::surface::Damage::Full,
            &mut rows,
        );
        let snapshot = rows.clone();
        calls.set(0);
        build_rows(
            &s,
            &mut router,
            &metrics_8x16(),
            &Palette::DEFAULT,
            0,
            &mica_core::surface::Damage::Lines(vec![]),
            &mut rows,
        );
        assert_eq!(calls.get(), 0, "空脏区不得触发任何路由");
        assert_eq!(rows, snapshot, "空脏区行缓存原样");
    }

    /// repack 平铺恢复全局两遍发射序:全部行的 bg 段在前(行主序连续),
    /// 全部字形段随后——管线 blend:None 契约的行级等价物。
    #[test]
    fn repack_emits_all_backgrounds_before_all_glyphs() {
        let mut s = Surface::new(ScreenSize::new(3, 2));
        s.feed("中E".as_bytes()); // 行 0 宽字形(占 2 格)+ E
        s.feed(b"\r\nF"); // 行 1 字形
        let calls = std::cell::Cell::new(0);
        let fresh = std::cell::Cell::new(0);
        let mut router = counting_router(&calls, &fresh);
        let mut rows = Vec::new();
        build_rows(
            &s,
            &mut router,
            &metrics_8x16(),
            &Palette::DEFAULT,
            0,
            &mica_core::surface::Damage::Full,
            &mut rows,
        );
        let packed = repack(&rows);
        let bg_total: usize = rows.iter().map(|r| r.bg.len()).sum();
        assert_eq!(bg_total, 6, "每格一整格背景:2 行 x 3 格");
        assert_eq!(packed.len(), bg_total + 3, "字形段:中、E、F");
        for (i, inst) in packed.iter().take(bg_total).enumerate() {
            assert_eq!(
                inst.size_uv[2], 0.0,
                "packed[{i}] 在 bg 段内:uv 尺寸 0(blank 形态)"
            );
        }
        for (i, inst) in packed.iter().skip(bg_total).enumerate() {
            assert!(inst.size_uv[2] > 0.0, "packed[{}] 在字形段内", bg_total + i);
        }
        // 行主序:bg 段内前 3 个是行 0(x = 0/8/16),后 3 个是行 1
        for (i, x) in [0.0, 8.0, 16.0].into_iter().enumerate() {
            assert_eq!(packed[i].pos_uv[0], x);
            assert_eq!(packed[3 + i].pos_uv[0], x);
        }
    }
}
