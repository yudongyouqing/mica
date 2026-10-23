//! Grid -> render instances. Pure CPU; the GPU pass (Task 6) just draws them.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::grid::Grid;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::cell::Flags;

use alacritty_terminal::vte::ansi::Rgb;

use mica_core::config::palette::Palette;
use mica_core::config::settings::StripCfg;
use mica_core::surface::{CursorShape, Damage, Surface};

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

/// #90 WCAG 相对亮度(sRGB 简化线性化)
fn rel_lum(rgb: [u8; 3]) -> f32 {
    let c = |v: u8| {
        let s = f32::from(v) / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * c(rgb[0]) + 0.7152 * c(rgb[1]) + 0.0722 * c(rgb[2])
}

/// #90 最小对比度强制:对比低于 min 时把 fg 推向白或黑(朝更远的极端
/// 逐通道夹逼,24 步收敛;纯函数可 TDD)
pub fn enforce_contrast(fg: [u8; 3], bg: [u8; 3], min: f32) -> [u8; 3] {
    if min <= 0.0 {
        return fg;
    }
    let lb = rel_lum(bg);
    let lf = rel_lum(fg);
    let cur = (lf.max(lb) + 0.05) / (lf.min(lb) + 0.05);
    if cur >= min {
        return fg;
    }
    let dark_bg = lb < 0.5;
    let mut out = fg;
    for _ in 0..24 {
        let l = rel_lum(out);
        let ratio = (l.max(lb) + 0.05) / (l.min(lb) + 0.05);
        if ratio >= min {
            break;
        }
        for ch in out.iter_mut() {
            *ch = if dark_bg {
                ch.saturating_add(11)
            } else {
                ch.saturating_sub(11)
            };
        }
    }
    out
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

/// 圆角 blank quad(2026-10 UI 对标):blend:None 下没有 alpha 混合,圆角
/// 抗锯齿的唯一途径是 per-fragment 把**已知底衬色**与面色按 SDF 覆盖率
/// premix(见 cells.wgsl 的 rounded_coverage)。槽位语义仅对无字形 quad
/// 合法:fg.rgb=底衬色、bg.w=上圆角半径、fg.w=下圆角半径(px,0=方角);
/// 字形 quad 的 fg 是墨色,不得走圆角路径。
fn rounded(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    face: [u8; 3],
    under: [u8; 3],
    radii: (f32, f32), // (上, 下)
) -> CellInstance {
    let c = |rgb: [u8; 3]| {
        [
            rgb[0] as f32 / 255.0,
            rgb[1] as f32 / 255.0,
            rgb[2] as f32 / 255.0,
        ]
    };
    CellInstance {
        pos_uv: [x, y, 0.0, 0.0],
        size_uv: [w, h, 0.0, 0.0],
        fg: [c(under)[0], c(under)[1], c(under)[2], radii.1],
        bg: [c(face)[0], c(face)[1], c(face)[2], radii.0],
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
    selection: Option<&SelectionRange>,
    display_offset: usize,
    cursor: Option<CursorShape>,
) -> Vec<CellInstance> {
    let grid = surface.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let cursor_pos = grid.cursor.point;
    let cursor_line = usize::try_from(cursor_pos.line.0).unwrap_or(usize::MAX);
    let cursor_col = cursor_pos.column.0;
    // 上界 = 每格 2 实例(bg 段 + 字形段;空白格只有 bg)
    let mut out = Vec::with_capacity(cols * rows * 2);
    // 字形段先攒后拼:保证全部 bg 先于全部字形(跨格画家序,见 doc)
    let mut glyphs = Vec::with_capacity(cols * rows);
    for line in 0..rows {
        let buffer_line = line as i32 - display_offset as i32;
        let y = line as f32 * metrics.line_height;
        let mut bar = None;
        for col in 0..cols {
            let cell = &grid[Line(buffer_line)][Column(col)];
            let cursor_here = display_offset == 0 && line == cursor_line && col == cursor_col;
            let (mut fg, mut bg) = cell_colors(cell, selection, buffer_line, col, palette);
            // 光标按形状:块=反色交换;条形=不交换、字可见,bar 单独发;
            // None=闪烁关闭相位(不交换、无条)
            if cursor_here {
                match cursor {
                    Some(CursorShape::Block) => std::mem::swap(&mut fg, &mut bg),
                    Some(shape) => {
                        bar = Some((shape, col as f32 * metrics.cell_width, y));
                    }
                    None => {}
                }
            }
            let x = col as f32 * metrics.cell_width;
            if cell.hyperlink().is_some() {
                // OSC 8 链接下划线(D28):置于段首(画在全部 bg 之上、字形之下)
                glyphs.insert(0, link_underline(x, y, metrics, fg));
            }
            // SGR 下划线族(T5):同层(bg 上、字下);本循环先于字形段填 glyphs,
            // 天然序正确
            glyphs.extend(underline_quads(cell.flags, x, y, metrics, fg));
            out.push(blank(x, y, metrics, fg, bg));
        }
        if let Some((shape, x, y)) = bar {
            glyphs.insert(0, cursor_bar(shape, x, y, metrics, palette));
        }
        // 字形段:cluster 步进,与 build_row 逐字节同构(黄金对拍锁定)
        let mut col = 0usize;
        while col < cols {
            let cell = &grid[Line(buffer_line)][Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.c == ' ' {
                col += 1;
                continue;
            }
            let style = GlyphStyle::from_flags(cell.flags);
            let mut end = col + 1;
            while end < cols {
                let next = &grid[Line(buffer_line)][Column(end)];
                if next.flags.contains(Flags::WIDE_CHAR_SPACER)
                    || next.c == ' '
                    || GlyphStyle::from_flags(next.flags) != style
                {
                    break;
                }
                end += 1;
            }
            let mut shaped = None;
            if end - col >= 2 {
                let chars: Vec<char> = (col..end)
                    .map(|c| grid[Line(buffer_line)][Column(c)].c)
                    .collect();
                shaped = router.route_run(&chars, style);
            }
            let Some(clusters) = shaped else {
                let (fg, bg) = cell_colors(cell, selection, buffer_line, col, palette);
                let x = col as f32 * metrics.cell_width;
                let g = router.route(cell.c, style);
                glyphs.push(glyph_instance(x, y, &g, fg, bg));
                col += 1;
                continue;
            };
            let mut pos = col;
            for cluster in clusters {
                let cell_c = &grid[Line(buffer_line)][Column(pos)];
                let (fg, bg) = cell_colors(cell_c, selection, buffer_line, pos, palette);
                let x = pos as f32 * metrics.cell_width;
                match cluster.glyph {
                    Some(g) => glyphs.push(glyph_instance(x, y, &g, fg, bg)),
                    None => {
                        let g = router.route(cell_c.c, style);
                        glyphs.push(glyph_instance(x, y, &g, fg, bg));
                    }
                }
                pos += cluster.len.max(1);
            }
            col = pos;
        }
    }
    out.extend(glyphs);
    out
}

/// strip 基准几何(96dpi 逻辑值)——**全量可配**(2026-10-12,StripCfg
/// 键集见 mica-core settings);此处常量是未配置时的兜底默认。运行时一律
/// 经 [`StripMetrics::from_scale_cfg`] 按配置 + DPI 合成——物理常量在
/// 150% 屏上不缩放,曾致条高/×/内边距与 150% 字体错位(2026-10-06 实测)。
/// app 层 hit-test 同源取值。
pub const TAB_PLUS_W_BASE: f32 = 36.0;
pub const TAB_MIN_W_BASE: f32 = 88.0;
pub const TAB_MAX_W_BASE: f32 = 220.0;
/// 标签间缝与左右内边距(逻辑 px)
pub const TAB_GAP_BASE: f32 = 2.0;
pub const TAB_PAD_BASE: f32 = 10.0;
/// × 关闭钮预留区宽(每标签恒留——宽度稳定,悬停不回流)
pub const TAB_CLOSE_W_BASE: f32 = 24.0;

/// 缩放后的 strip 运行时几何(单源:渲染与 hit-test 都从这里取)。
/// scale = 窗口 DPI/96(M5b 的 DPI_SCALE);cfg = 用户配置。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StripMetrics {
    /// 条总高(两行;客户区顶部,终端区下移此值)
    pub h: f32,
    /// 第一行高:窗口标题 + caption 三键
    pub title_h: f32,
    /// 第二行高:标签 + 右端 "+"
    pub tab_h: f32,
    /// DPI 缩放系数(96dpi 逻辑 px → 物理 px;曾以 h/STRIP_H_BASE 反推,
    /// 双行后不再成立,显式持有)
    pub k: f32,
    pub tab_w: f32, // 退役兼容字段(溢出滚动步进用,值为 min_w)
    pub plus_w: f32,
    pub tab_min_w: f32,
    pub tab_max_w: f32,
    pub tab_gap: f32,
    pub tab_pad: f32,
    pub tab_close_w: f32,
    /// caption 单键宽(— □ ×)
    pub caption_w: f32,
    /// 标签块上圆角半径(下缘直角:活跃块与终端区无缝相接)
    pub tab_top_r: f32,
    /// 溢出滚动箭头钮宽(左右各一,#39)
    pub arrow_w: f32,
    /// tooltip 圆角半径
    pub chip_r: f32,
    /// exit 状态点直径
    pub dot_d: f32,
    /// 标题左内边距(亦为 × 区的视觉基准)
    pub pad: f32,
    /// 标签占满第二行(宽标签);false = 恒 min_w(窄标签)
    pub wide: bool,
    /// 单标签隐藏第二行(false = 恒显)
    pub hide_single: bool,
    /// 条底色覆盖(None = auto:bg 提亮 wash)
    pub bar_c: Option<[u8; 3]>,
    /// auto 条底的提亮比例(聚焦/失焦两档,聚焦位)
    pub bar_wash: f32,
    pub bar_wash_unfocused: f32,
    /// 活跃标签面色覆盖(None = auto:聚焦 = bg 原样)
    pub active_c: Option<[u8; 3]>,
    /// 主文字色覆盖(None = auto:调色板 fg)
    pub text_c: Option<[u8; 3]>,
    /// 次级墨色覆盖(None = auto:mix(白,条底,0.35))
    pub dim_c: Option<[u8; 3]>,
}

impl StripMetrics {
    /// 配置 + DPI 合成(app 层唯一入口)
    pub fn from_scale_cfg(scale: f32, cfg: &StripCfg) -> Self {
        let s = scale.max(1.0);
        let c = |v: Option<Rgb>| v.map(|r| [r.r, r.g, r.b]);
        Self {
            title_h: cfg.title_h * s,
            tab_h: cfg.tab_h * s,
            h: (cfg.title_h + cfg.tab_h) * s,
            k: s,
            tab_w: cfg.tab_min_w * s,
            tab_min_w: cfg.tab_min_w * s,
            tab_max_w: TAB_MAX_W_BASE * s,
            tab_gap: TAB_GAP_BASE * s,
            tab_pad: TAB_PAD_BASE * s,
            tab_close_w: TAB_CLOSE_W_BASE * s,
            plus_w: TAB_PLUS_W_BASE * s,
            caption_w: 46.0 * s,
            tab_top_r: cfg.tab_corner_r * s,
            arrow_w: 24.0 * s,
            chip_r: 6.0 * s,
            dot_d: 7.0 * s,
            pad: 8.0 * s,
            wide: cfg.wide_tabs,
            hide_single: cfg.hide_single,
            bar_c: c(cfg.bar),
            bar_wash: cfg.bar_wash,
            bar_wash_unfocused: cfg.bar_wash_unfocused,
            active_c: c(cfg.active_tab),
            text_c: c(cfg.text),
            dim_c: c(cfg.dim),
        }
    }
    /// 默认配置(测试/旧路径)
    pub fn from_scale(scale: f32) -> Self {
        Self::from_scale_cfg(scale, &StripCfg::default())
    }
}

impl Default for StripMetrics {
    fn default() -> Self {
        Self::from_scale(1.0)
    }
}

/// D37 exit 状态点形态:绿(0)/红(非 0)/灰(D 参数不可解析)。
/// 色取调色板语义槽(green=2 / red=1 / bright black=8),主题同源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitDot {
    Green,
    Red,
    Gray,
}

/// 第二行标签区起点(条左内边距)
pub fn tabs_x0(sm: &StripMetrics) -> f32 {
    6.0 * sm.k
}

/// 第二行右端 "+" 按钮左缘(total_w = 条全宽/客户区宽)
pub fn plus_x(total_w: f32, sm: &StripMetrics) -> f32 {
    total_w - sm.plus_w - 6.0 * sm.k
}

/// 标签可视右缘("+" 左邻呼吸缝;放不下的标签由溢出滚动接管)
pub fn tabs_right(total_w: f32, sm: &StripMetrics) -> f32 {
    plus_x(total_w, sm) - 2.0 * sm.k
}

/// 标签宽(均分填充):标签占满第二行可用宽(用户 2026-10-12 裁定
/// "占满没事";Ghostty `gtk-wide-tabs` 默认 true 同义);可用域 =
/// tabs_x0 到 tabs_right(右端 "+" 左缘)。放不下(min_w × n > 可用域)
/// 则固定 min_w 由溢出滚动兜底。等宽单值,渲染与 hit-test 同源。
pub fn equal_tab_w(n: usize, avail_w: f32, sm: &StripMetrics) -> f32 {
    if n == 0 {
        return sm.tab_min_w;
    }
    let gaps = sm.tab_gap * n as f32;
    let body = (tabs_right(avail_w, sm) - tabs_x0(sm) - gaps).max(0.0);
    (body / n as f32).max(sm.tab_min_w)
}

/// 当前标签宽策略(2026-10-12 全量可配):wide = 均分占满,窄标签 =
/// 恒 min_w——渲染与 hit-test 都取此单源
pub fn tab_w_now(n: usize, avail_w: f32, sm: &StripMetrics) -> f32 {
    if sm.wide {
        equal_tab_w(n, avail_w, sm)
    } else {
        sm.tab_min_w
    }
}

/// strip 布局参数(T8 溢出滚动):tab_scroll = 向左滚过的标签数。
#[derive(Debug, Clone, Copy, Default)]
pub struct StripLayout {
    pub tab_scroll: i32,
}

/// strip 杂项参数打包(M5b/T3 起参数超限):状态点/pane 退出态/caption 区。
/// tab_dx 非 Copy(Vec)——拖拽挤压的邻位滑移量按原始下标对位。
#[derive(Debug, Clone, Default)]
pub struct StripExtras {
    /// D37 OSC 133 exit 状态点
    pub exit_dot: Option<ExitDot>,
    /// T8 系统按钮区起点(客户区坐标)
    pub caption_x: Option<f32>,
    /// T8 溢出滚动布局
    pub layout: StripLayout,
    /// M5b/T3 pane 退出码(活跃 pane 已退出时显示 [exit N])
    pub pane_exit: Option<u32>,
    /// 悬停态(2026-10 UI 对标):标签/加号/caption 按钮的可视反馈
    pub hover: StripHover,
    /// 最大化态(2026-10-07):true 时最大化钮显还原图标(E923)
    pub zoomed: bool,
    /// 窗口聚焦相位(2026-10-12):1.0 = 聚焦(条底 18% 提亮),0.0 = 失焦
    /// (洗白至 30%)——**连续值**,app 侧动画驱动(WM_TIMER 渐进),
    /// 切换不跳变(上游 becomeMain/resignMain 语义的动画化)
    pub focus: f32,
    /// 活跃标签胶囊横移量(动画中):非零时活跃标签 x0 偏移此值,衰减到
    /// 0 即完成——切换/新建/关闭时 app 侧置初值,标签切换的滑动即此
    pub capsule_dx: f32,
    /// 拖拽换序(挤压式):(被拖下标, 目标槽位, 被拖标签左缘 px[标签空间,
    /// 含滚动 shift])。拖住的手粘光标,其余标签按槽位实时让位,松手由
    /// app 侧提交顺序(Chrome/Adw 语义,2026-10-12 二版)
    pub drag: Option<(usize, usize, f32)>,
    /// 邻位滑移量(按原始下标):槽位变化时 app 侧记跳变差并指数衰减到
    /// 0——挤压是**滑**过去不是瞬移(2026-10-12 三版,用户裁定)
    pub tab_dx: Vec<f32>,
}

/// strip 悬停命中(与 app 层 hit-test 同几何单源;命中区互斥)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StripHover {
    /// 悬停的非激活标签(激活标签无反馈必要)
    pub tab: Option<usize>,
    pub plus: bool,
    /// 溢出滚动箭头(#39;仅溢出时有效)
    pub arrow_l: bool,
    pub arrow_r: bool,
    /// caption 三键(— □ ×)
    pub caption: Option<CaptionBtn>,
}

/// 第二行命中(与渲染几何同源):标签 / "+" / 溢出箭头(#39)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StripTabHit {
    Tab(usize),
    Plus,
    ArrowLeft,
    ArrowRight,
}

/// caption 按钮序(与 NCHITTEST 的从左到右一致)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionBtn {
    Min,
    Max,
    Close,
}

/// 整行文本提取(#58 可点击路径 / 未来搜索共用):字符拼串 + 字符→格
/// 下标映射(WIDE_CHAR 占 2 格)。line 为 buffer 行(i32,负 = 历史)。
pub fn row_string(grid: &Grid<Cell>, line: i32) -> (String, Vec<usize>) {
    let cols = grid.columns();
    let mut text = String::with_capacity(cols);
    let mut cell_of = Vec::with_capacity(cols);
    for c in 0..cols {
        let cell = &grid[Line(line)][Column(c)];
        text.push(cell.c);
        let w = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        for _ in 0..w {
            cell_of.push(c);
        }
    }
    (text, cell_of)
}

/// tab strip 的 UI 实例(T7,spec §3 同管线),**双行形制**(2026-10-12
/// 用户裁定):第一行 = 窗口标题(活跃标签名,居中)+ 右端 caption 三键
/// (— □ ×);第二行 = 均分标签占满 + 右端 "+" 开新标签;再下才是终端
/// 主体。单标签时第二行整行隐藏(上游 hasTabBar 同款)。
/// 条底 = 常态浅灰(终端底提亮 18%;失焦再洗至 30%,上游 becomeMain/
/// resignMain 同款),活跃标签块 = 终端底色原样、上圆下方与内容区无缝
/// 相接。strip 文字走**小号等宽路由**(9.5pt 共享图集,app 侧注入;
/// 字号 12pt 的终端路由出的大字形会挤出 × 关闭区,用户实报)。
/// exit_dot(D37):活跃标签命令状态点,"+" 左邻固定锚。
/// pane_exit(M5b/T3):[exit N] 文本,状态点左侧。
/// layout(T8):溢出滚动——越界标签不发射,滚轮驱动,箭头命中区暂缓。
pub fn strip_quads(
    titles: &[(String, bool)],
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
    sm: &StripMetrics,
    extras: StripExtras,
) -> Vec<CellInstance> {
    let StripExtras {
        exit_dot,
        caption_x,
        layout,
        pane_exit,
        hover,
        zoomed,
        focus,
        capsule_dx,
        drag,
        tab_dx,
    } = extras;
    let mut out = Vec::new();
    let total_w = titles.len() as f32 * sm.tab_min_w + sm.plus_w;
    let bg_of = |rgb: [u8; 3]| to_color(rgb);
    let mix = |a: [u8; 3], b: [u8; 3], k: f32| {
        [
            (a[0] as f32 + (b[0] as f32 - a[0] as f32) * k + 0.5) as u8,
            (a[1] as f32 + (b[1] as f32 - a[1] as f32) * k + 0.5) as u8,
            (a[2] as f32 + (b[2] as f32 - a[2] as f32) * k + 0.5) as u8,
        ]
    };
    let bg3 = [palette.bg.r, palette.bg.g, palette.bg.b];
    // 条底 = 常态浅灰(上游实测:标签栏比终端底亮;Ventura 版 14% 提亮同族,
    // 默认 18%),失焦再洗一档(默认 30%;Tahoe becomeMain/resignMain 同款)。
    // focus 是 0..1 的动画相位(app 侧 WM_TIMER 渐进),切换不再跳变——
    // 这是"丝滑"的一半;另一半是活跃块滑动(capsule_dx)。
    // 全部颜色/比例可配(strip-* 键):显式色直通,auto 才走提亮推导
    let focus = focus.clamp(0.0, 1.0);
    let wash = sm.bar_wash + (sm.bar_wash_unfocused - sm.bar_wash) * (1.0 - focus);
    let bar = sm.bar_c.unwrap_or_else(|| mix(bg3, [255, 255, 255], wash));
    // 活跃标签面色:聚焦 = 终端底原样(与内容一体);失焦同样洗白一档
    let active_face = sm
        .active_c
        .unwrap_or_else(|| mix(bg3, [255, 255, 255], 0.18 * (1.0 - focus)));
    let white = sm
        .text_c
        .unwrap_or([palette.fg.r, palette.fg.g, palette.fg.b]);
    // strip 次级墨色(2026-10-12):条底改浅灰后,旧 dim(C5C8C6)在灰上
    // 对比度崩(+/—□× 实测不可见)——向白提亮,压着条底仍有层次
    let ink2 = sm.dim_c.unwrap_or_else(|| mix(white, bar, 0.35));
    let k = sm.k;
    let cw = metrics.cell_width;
    // 等宽格步进的字符步长:宽字形(CJK)按墨宽推进,窄字符不小于格宽
    let adv = |g: &crate::font::router::GlyphInfo| g.size_px[0].max(cw);
    // 单标签 = 无第二行(上游 hasTabBar 同款;strip-hide-single-tab = false
    // 可恒显):条高只余标题行
    let single = sm.hide_single && titles.len() <= 1;
    let eff_h = if single { sm.title_h } else { sm.h };
    // 全条底(两行同色;发丝线退役——活跃块要与内容区无缝相接)
    let strip_w = caption_x
        .map(|cx| cx + 3.0 * sm.caption_w)
        .unwrap_or(total_w)
        .max(total_w);
    out.push(CellInstance {
        pos_uv: [0.0, 0.0, 0.0, 0.0],
        size_uv: [strip_w, eff_h, 0.0, 0.0],
        fg: bg_of(bar),
        bg: bg_of(bar),
    });

    // ---- 第一行:窗口标题(活跃标签名,居中;caption 左界内,超宽省略号)
    let title_y = (sm.title_h - metrics.line_height).max(0.0) / 2.0;
    let title_avail = caption_x.unwrap_or(strip_w) - 8.0 * k;
    let title = titles
        .iter()
        .find(|(_, active)| *active)
        .map(|(t, _)| t.as_str())
        .unwrap_or_default();
    let ell = router.route('\u{2026}', GlyphStyle::PLAIN);
    let routed: Vec<crate::font::router::GlyphInfo> = title
        .chars()
        .map(|ch| router.route(ch, GlyphStyle::PLAIN))
        .collect();
    let mut text_w = 0.0;
    let mut n_take = routed.len();
    let mut with_ell = false;
    for (i, g) in routed.iter().enumerate() {
        // 省略号宽度恒预留:正好装满的标题也换省略号收口,留呼吸
        if text_w + adv(g) + adv(&ell) > title_avail {
            n_take = i;
            with_ell = n_take > 0;
            break;
        }
        text_w += adv(g);
    }
    if with_ell {
        text_w += adv(&ell);
    }
    let mut tx = ((title_avail - text_w) / 2.0).max(0.0);
    let title_ink = mix(ink2, white, focus);
    for g in routed.iter().take(n_take) {
        if g.size_px[0] > 0.0 {
            out.push(CellInstance {
                pos_uv: [
                    tx + g.offset_px[0],
                    title_y + g.offset_px[1],
                    g.uv[0],
                    g.uv[1],
                ],
                size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                fg: bg_of(title_ink),
                bg: bg_of(bar),
            });
        }
        tx += adv(g);
    }
    if with_ell && ell.size_px[0] > 0.0 {
        out.push(CellInstance {
            pos_uv: [
                tx + ell.offset_px[0],
                title_y + ell.offset_px[1],
                ell.uv[0],
                ell.uv[1],
            ],
            size_uv: [ell.size_px[0], ell.size_px[1], ell.uv[2], ell.uv[3]],
            fg: bg_of(title_ink),
            bg: bg_of(bar),
        });
    }

    // ---- caption 三键(第一行右端;hit-test 同几何在 NCHITTEST):
    // 裸字形减淡,hover:min/max 轻亮块、close 红底白字(Windows 惯例)
    if let Some(cx) = caption_x {
        let glyphs = [
            '\u{E921}',
            if zoomed { '\u{E923}' } else { '\u{E922}' },
            '\u{E8BB}',
        ];
        for (i, glyph) in glyphs.iter().enumerate() {
            let bx = cx + i as f32 * sm.caption_w;
            let hovered = hover.caption
                == Some(match i {
                    0 => CaptionBtn::Min,
                    1 => CaptionBtn::Max,
                    _ => CaptionBtn::Close,
                });
            let lighten = mix(bar, white, 0.08);
            let red = [
                palette.colors[1].r,
                palette.colors[1].g,
                palette.colors[1].b,
            ];
            let (face, ink) = match (hovered, i) {
                (true, 2) => (red, white),     // close hover:红底白字
                (true, _) => (lighten, white), // min/max hover:轻亮块
                (false, _) => (bar, ink2),
            };
            if hovered {
                out.push(CellInstance {
                    pos_uv: [bx, 0.0, 0.0, 0.0],
                    size_uv: [sm.caption_w, sm.title_h, 0.0, 0.0],
                    fg: bg_of(face),
                    bg: bg_of(face),
                });
            }
            let g = router.route(*glyph, GlyphStyle::PLAIN);
            if g.size_px[0] > 0.0 {
                // 图标双向真居中(2026-10-12 用户实测纠偏):pos 即墨盒左上,
                // 居中 = 容器 ±(容器尺寸-墨尺寸)/2,**不带 offset_px**——
                // 那是基线相关量,带上会双重偏移(× 曾偏下、caption 曾斜)
                out.push(CellInstance {
                    pos_uv: [
                        bx + (sm.caption_w - g.size_px[0]) / 2.0,
                        (sm.title_h - g.size_px[1]) / 2.0,
                        g.uv[0],
                        g.uv[1],
                    ],
                    size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                    fg: bg_of(ink),
                    bg: bg_of(face),
                });
            }
        }
    }

    // ---- 第二行:均分标签占满 + 右端 "+"(单标签时整行隐藏,不发射)
    if !single {
        let tab_y = sm.title_h;
        let tab_d = sm.h - sm.title_h;
        let text_y = sm.title_h + (tab_d - metrics.line_height).max(0.0) / 2.0;
        let n = titles.len();
        let w = tab_w_now(n, strip_w, sm);
        let shift = layout.tab_scroll as f32 * sm.tab_min_w;
        let right = tabs_right(strip_w, sm);
        // 拖拽槽位序(挤压式):被拖标签占目标槽,其余按原序填空——拖住的
        // 手粘光标(左缘 = drag_x),邻位实时让位;松手 app 侧提交顺序
        let (drag_idx, drag_slot, drag_x) = match drag {
            Some((i, s, x)) if i < n => (i, s.min(n - 1), x),
            _ => (usize::MAX, 0, 0.0),
        };
        let order: Vec<usize> = if drag_idx < n {
            let mut v: Vec<usize> = (0..n).filter(|i| *i != drag_idx).collect();
            v.insert(drag_slot.min(v.len()), drag_idx);
            v
        } else {
            (0..n).collect()
        };
        for (slot, &i) in order.iter().enumerate() {
            let (title, active) = &titles[i];
            let slot_x = tabs_x0(sm) + slot as f32 * (w + sm.tab_gap) - shift;
            let squeeze = tab_dx.get(i).copied().unwrap_or(0.0);
            let x0 = if i == drag_idx {
                drag_x - shift
            } else if *active {
                slot_x + capsule_dx + squeeze
            } else {
                slot_x + squeeze
            };
            if x0 + w <= 0.0 || (i != drag_idx && x0 >= right) {
                continue; // 滚出可视区的标签不发射(被拖标签恒画)
            }
            let hovered = hover.tab == Some(i) && i != drag_idx;
            let hover_face = mix(bar, white, 0.05);
            let (backdrop, ink) = if *active {
                (active_face, white)
            } else if hovered {
                (hover_face, white)
            } else {
                (bar, ink2)
            };
            // 标签块:上圆下方——活跃块 = 终端底色,下缘直抵内容区无缝
            // 相接(macOS 原生标签形制);非活跃悬停轻提亮
            if *active || hovered {
                out.push(rounded(
                    x0,
                    tab_y,
                    w,
                    tab_d,
                    backdrop,
                    bar,
                    (sm.tab_top_r, 0.0),
                ));
            }
            // 标题:等宽格步进,超宽以省略号截断(× 区前留呼吸)
            let close_x = x0 + w - sm.tab_close_w;
            let mut tx = x0 + sm.tab_pad;
            for ch in title.chars() {
                let g = router.route(ch, GlyphStyle::PLAIN);
                if tx + adv(&g) + adv(&ell) > close_x - 6.0 * k {
                    if ell.size_px[0] > 0.0 {
                        out.push(CellInstance {
                            pos_uv: [
                                tx + ell.offset_px[0],
                                text_y + ell.offset_px[1],
                                ell.uv[0],
                                ell.uv[1],
                            ],
                            size_uv: [ell.size_px[0], ell.size_px[1], ell.uv[2], ell.uv[3]],
                            fg: bg_of(ink),
                            bg: bg_of(backdrop),
                        });
                    }
                    break;
                }
                if g.size_px[0] > 0.0 {
                    out.push(CellInstance {
                        pos_uv: [
                            tx + g.offset_px[0],
                            text_y + g.offset_px[1],
                            g.uv[0],
                            g.uv[1],
                        ],
                        size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                        fg: bg_of(ink),
                        bg: bg_of(backdrop),
                    });
                }
                tx += adv(&g);
            }
            // × 关闭钮(Fluent ChromeClose E8BB):激活常显(次级灰)、
            // 悬停浮现(主色);胶囊内双向真居中(不带 offset,见 caption 注),
            // 点区在 WM_LBUTTONDOWN 同几何
            if *active || hovered {
                let kink = if *active { ink2 } else { white };
                let g = router.route('\u{E8BB}', GlyphStyle::PLAIN);
                if g.size_px[0] > 0.0 {
                    out.push(CellInstance {
                        pos_uv: [
                            close_x + (sm.tab_close_w - g.size_px[0]) / 2.0,
                            tab_y + (tab_d - g.size_px[1]) / 2.0,
                            g.uv[0],
                            g.uv[1],
                        ],
                        size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                        fg: bg_of(kink),
                        bg: bg_of(backdrop),
                    });
                }
            }
        }
        // "+" 开新标签(第二行右端;Chrome/WT/Tahoe 都在标签行右端,
        // 2026-10-12 双行重排时用户点名"最右边")
        {
            let x = plus_x(strip_w, sm);
            let hovered = hover.plus;
            let face = mix(bar, white, 0.05);
            // "+":开标签的主入口,恒白(旧 dim 在浅灰条上实测不可见)
            let (glyph_bg, ink) = if hovered { (face, white) } else { (bar, white) };
            if hovered {
                let r = tab_d / 2.0;
                out.push(rounded(
                    x + (sm.plus_w - tab_d) / 2.0,
                    tab_y,
                    tab_d,
                    tab_d,
                    face,
                    bar,
                    (r, r),
                ));
            }
            let g = router.route('+', GlyphStyle::PLAIN);
            if g.size_px[0] > 0.0 {
                out.push(CellInstance {
                    pos_uv: [
                        x + (sm.plus_w - g.size_px[0]) / 2.0,
                        tab_y + (tab_d - g.size_px[1]) / 2.0,
                        g.uv[0],
                        g.uv[1],
                    ],
                    size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                    fg: bg_of(ink),
                    bg: bg_of(glyph_bg),
                });
            }
        }
        // 溢出滚动箭头(#39):总占宽超右缘即显(左右缘各一,条底垫背
        // 遮住下层标签);点击步进 = tab_scroll_adjust(±1),hover 提亮
        let span = w + sm.tab_gap;
        if tabs_x0(sm) + n as f32 * span > right + 0.5 {
            for (glyph, ax, hov) in [
                ('\u{E76B}', 2.0 * k, hover.arrow_l),
                ('\u{E76C}', right - sm.arrow_w, hover.arrow_r),
            ] {
                out.push(CellInstance {
                    pos_uv: [ax, tab_y, 0.0, 0.0],
                    size_uv: [sm.arrow_w, tab_d, 0.0, 0.0],
                    fg: bg_of(bar),
                    bg: bg_of(bar),
                });
                let g = router.route(glyph, GlyphStyle::PLAIN);
                if g.size_px[0] > 0.0 {
                    let ink = if hov { white } else { ink2 };
                    out.push(CellInstance {
                        pos_uv: [
                            ax + (sm.arrow_w - g.size_px[0]) / 2.0,
                            tab_y + (tab_d - g.size_px[1]) / 2.0,
                            g.uv[0],
                            g.uv[1],
                        ],
                        size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                        fg: bg_of(ink),
                        bg: bg_of(bar),
                    });
                }
            }
        }
        // pane 退出态(M5b/T3):第二行右端、"+" 左邻(标签标题截断吃不到);
        // 0 绿/非 0 红,文本止于状态点左
        if let Some(code) = pane_exit {
            let label = format!("[exit {code}]");
            let slot = if code == 0 {
                palette.colors[2]
            } else {
                palette.colors[1]
            };
            let end_x = plus_x(strip_w, sm) - 12.0 * k - sm.dot_d - 8.0 * k;
            let start = end_x - label.chars().count() as f32 * cw;
            for (ci, ch) in label.chars().enumerate() {
                let g = router.route(ch, GlyphStyle::PLAIN);
                if g.size_px[0] <= 0.0 {
                    continue;
                }
                out.push(CellInstance {
                    pos_uv: [
                        start + ci as f32 * cw + g.offset_px[0],
                        text_y + g.offset_px[1],
                        g.uv[0],
                        g.uv[1],
                    ],
                    size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                    fg: bg_of([slot.r, slot.g, slot.b]),
                    bg: bg_of(bar),
                });
            }
        }
        // D37 exit 状态点:第二行右端、"+" 左邻(固定锚),真圆(半径 = 半宽)
        if let Some(dot) = exit_dot {
            let slot = match dot {
                ExitDot::Green => palette.colors[2],
                ExitDot::Red => palette.colors[1],
                ExitDot::Gray => palette.colors[8],
            };
            let dx = plus_x(strip_w, sm) - 12.0 * k - sm.dot_d;
            let cy = sm.title_h + sm.tab_h / 2.0;
            let d = sm.dot_d;
            out.push(rounded(
                dx,
                cy - d / 2.0,
                d,
                d,
                [slot.r, slot.g, slot.b],
                bar,
                (d / 2.0, d / 2.0),
            ));
        }
    }
    out
}

/// 圆角硬边 quad(悬浮层用):底衬是任意终端内容、不可预知,premix AA
/// 无从谈起——blend:None 下正解是圆角外硬 discard(fg.w<0 哨兵,
/// cells.wgsl 判定)。小半径硬边肉眼可接受。
fn rounded_hard(x: f32, y: f32, w: f32, h: f32, face: [u8; 3], r: f32) -> CellInstance {
    let c = |v: u8| v as f32 / 255.0;
    CellInstance {
        pos_uv: [x, y, 0.0, 0.0],
        size_uv: [w, h, 0.0, 0.0],
        fg: [c(face[0]), c(face[1]), c(face[2]), -1.0],
        bg: [c(face[0]), c(face[1]), c(face[2]), r],
    }
}

/// 一行的两段实例:bg 整格段(行内每格一实例)+ 字形段(仅非空白格)。
/// 行主序持有;**全局**两遍发射契约(全部 bg 先于全部字形)由 [`repack`]
/// 平铺时恢复——行内只保序,平铺才定序。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RowInst {
    pub bg: Vec<CellInstance>,
    pub glyphs: Vec<CellInstance>,
}

/// D28 hover URI 气泡:主窗 overlay(spec 的 WS_POPUP 简化——零新窗口零
/// 新管线,同功能;越窗裁剪 v1 接受)。bg + 1px 边框 + 等宽文字(截断
/// 40 字符),锚点为鼠标位 + 偏移(app 侧传入)。
pub fn tooltip_quads(
    url: &str,
    x: f32,
    y: f32,
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
) -> Vec<CellInstance> {
    let bg_of = |rgb: [u8; 3]| to_color(rgb);
    let chars: Vec<char> = url.chars().take(40).collect();
    let text_w = chars.len() as f32 * metrics.cell_width;
    let pad = 6.0;
    let w = text_w + pad * 2.0;
    let h = metrics.line_height + pad;
    let mut out = Vec::with_capacity(1 + chars.len());
    // 圆角悬浮片(2026-10):面色 = 背景向前景 18% 提亮(纯背景色会隐身),
    // 硬边圆角(discard 模式,rounded_hard)——悬浮层下是任意终端内容,
    // 底衬不可预知;旧 1px 边框随圆角化退役
    let mix = |a: [u8; 3], b: [u8; 3], k: f32| {
        [
            (a[0] as f32 + (b[0] as f32 - a[0] as f32) * k + 0.5) as u8,
            (a[1] as f32 + (b[1] as f32 - a[1] as f32) * k + 0.5) as u8,
            (a[2] as f32 + (b[2] as f32 - a[2] as f32) * k + 0.5) as u8,
        ]
    };
    let face = mix(
        [palette.bg.r, palette.bg.g, palette.bg.b],
        [palette.fg.r, palette.fg.g, palette.fg.b],
        0.18,
    );
    out.push(rounded_hard(x, y, w, h, face, 6.0));
    let text_y = (h - metrics.line_height) / 2.0;
    for (ci, ch) in chars.iter().enumerate() {
        let g = router.route(*ch, GlyphStyle::PLAIN);
        if g.size_px[0] <= 0.0 {
            continue;
        }
        out.push(CellInstance {
            pos_uv: [
                x + pad + ci as f32 * metrics.cell_width + g.offset_px[0],
                y + text_y + g.offset_px[1],
                g.uv[0],
                g.uv[1],
            ],
            size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
            fg: bg_of([palette.fg.r, palette.fg.g, palette.fg.b]),
            bg: bg_of(face),
        });
    }
    out
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
#[allow(clippy::too_many_arguments)] // 渲染热路径的参数组:打包 struct 反增噪音
pub fn build_rows(
    surface: &Surface,
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
    selection: Option<&SelectionRange>,
    display_offset: usize,
    cursor: Option<CursorShape>,
    damage: &Damage,
    rows: &mut Vec<RowInst>,
) {
    let grid = surface.grid();
    let screen_lines = grid.screen_lines();
    if rows.len() != screen_lines {
        rebuild_all(
            grid,
            router,
            metrics,
            palette,
            selection,
            display_offset,
            cursor,
            rows,
        );
        return;
    }
    match damage {
        Damage::Full => rebuild_all(
            grid,
            router,
            metrics,
            palette,
            selection,
            display_offset,
            cursor,
            rows,
        ),
        Damage::Lines(lines) => {
            for &line in lines {
                if line < screen_lines {
                    rows[line] = build_row(
                        grid,
                        line,
                        router,
                        metrics,
                        palette,
                        selection,
                        display_offset,
                        cursor,
                    );
                }
            }
        }
    }
}

/// 全量重建 `rows` 为恰好视口行数(清尾,适配 resize 缩行)。
#[allow(clippy::too_many_arguments)] // 同 build_rows:渲染参数组
fn rebuild_all(
    grid: &Grid<Cell>,
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
    selection: Option<&SelectionRange>,
    display_offset: usize,
    cursor: Option<CursorShape>,
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
            selection,
            display_offset,
            cursor,
        ));
    }
}

/// 行内格子的前景/背景对(选中 > 反色,光标由调用处按形状另行处理)。
fn cell_colors(
    cell: &Cell,
    selection: Option<&SelectionRange>,
    buffer_line: i32,
    col: usize,
    palette: &Palette,
) -> ([u8; 3], [u8; 3]) {
    let mut fg = resolve(cell.fg, palette);
    let mut bg = resolve(cell.bg, palette);
    if cell_selected(selection, cell, buffer_line, col) {
        fg = [
            palette.selection_fg.r,
            palette.selection_fg.g,
            palette.selection_fg.b,
        ];
        bg = [
            palette.selection_bg.r,
            palette.selection_bg.g,
            palette.selection_bg.b,
        ];
    } else if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    // #90 最小对比度:最终 (fg,bg) 过闸(0 = 直通,单分支零开销)
    if palette.minimum_contrast > 0.0 {
        fg = enforce_contrast(fg, bg, palette.minimum_contrast);
    }
    (fg, bg)
}

/// OSC 8 链接下划线(D28):底部 1px,色 = fg(fg 即 cell fg,随选区反色自然反)
fn link_underline(x: f32, y: f32, metrics: &FontMetrics, fg: [u8; 3]) -> CellInstance {
    CellInstance {
        pos_uv: [x, y + metrics.line_height - 1.0, 0.0, 0.0],
        size_uv: [metrics.cell_width, 1.0, 0.0, 0.0],
        fg: to_color(fg),
        bg: to_color(fg),
    }
}

/// SGR 下划线族(M5a/T5):UNDERLINE 直线、DOUBLE 双线、UNDERCURL 波浪、
/// DOTTED/DASHED 点/虚线(近似形态)。1px 高,底部对齐;波浪 = 2px 段在
/// bottom-1/bottom-3 交替。色 = fg(SGR 58 专属下划线色未接,边界记录)。
fn underline_quads(
    flags: Flags,
    x: f32,
    y: f32,
    metrics: &FontMetrics,
    fg: [u8; 3],
) -> Vec<CellInstance> {
    let mut out = Vec::new();
    let w = metrics.cell_width;
    let bottom = y + metrics.line_height;
    let q = |qx: f32, qy: f32, qw: f32| CellInstance {
        pos_uv: [qx, qy, 0.0, 0.0],
        size_uv: [qw, 1.0, 0.0, 0.0],
        fg: to_color(fg),
        bg: to_color(fg),
    };
    if flags.contains(Flags::UNDERCURL) {
        let mut sx = x;
        let mut hi = false;
        while sx < x + w - 0.5 {
            let seg = 2.0f32.min(x + w - sx);
            out.push(q(sx, if hi { bottom - 3.0 } else { bottom - 1.0 }, seg));
            hi = !hi;
            sx += seg;
        }
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        out.push(q(x, bottom - 1.0, w));
        out.push(q(x, bottom - 3.0, w));
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        let mut sx = x;
        while sx < x + w - 0.5 {
            out.push(q(sx, bottom - 1.0, 1.0));
            sx += 2.0;
        }
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        out.push(q(x, bottom - 1.0, w.min(4.0)));
        if w > 6.0 {
            out.push(q(x + 6.0, bottom - 1.0, (w - 6.0).min(2.0)));
        }
    } else if flags.contains(Flags::UNDERLINE) {
        out.push(q(x, bottom - 1.0, w));
    }
    out
}

/// 光标条实例(D20):Underline=底部 2px、Beam=左侧 1.2px;色 = palette.cursor。
/// 放字形段头部(bg 之上、字之下;细条与字重叠小,不必顶层)。
fn cursor_bar(
    shape: CursorShape,
    x: f32,
    y: f32,
    metrics: &FontMetrics,
    palette: &Palette,
) -> CellInstance {
    let (w, h) = match shape {
        CursorShape::Underline => (metrics.cell_width, 2.0),
        _ => (1.2, metrics.line_height), // Beam(归一后仅三值)
    };
    let cy = match shape {
        CursorShape::Underline => y + metrics.line_height - 2.0,
        _ => y,
    };
    let ink = [palette.cursor.r, palette.cursor.g, palette.cursor.b];
    CellInstance {
        pos_uv: [x, cy, 0.0, 0.0],
        size_uv: [w, h, 0.0, 0.0],
        fg: to_color(ink),
        bg: to_color(ink),
    }
}

/// 字形实例(连字/单字共用):彩色字形 fg 白原色直通。
fn glyph_instance(
    x: f32,
    y: f32,
    g: &crate::font::router::GlyphInfo,
    fg: [u8; 3],
    bg: [u8; 3],
) -> CellInstance {
    CellInstance {
        pos_uv: [x + g.offset_px[0], y + g.offset_px[1], g.uv[0], g.uv[1]],
        size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
        fg: if g.color {
            to_color([255, 255, 255])
        } else {
            to_color(fg)
        },
        bg: to_color(bg),
    }
}

/// 格是否在选区内(buffer 坐标)。宽字符前瞻一格:选到宽字尾部时
/// 首格(字形承载格)也要高亮,否则半个字形高亮半个白。
fn cell_selected(
    selection: Option<&SelectionRange>,
    cell: &Cell,
    buffer_line: i32,
    col: usize,
) -> bool {
    let Some(range) = selection else {
        return false;
    };
    let point = Point::new(Line(buffer_line), Column(col));
    range.contains(point)
        || (cell.flags.contains(Flags::WIDE_CHAR)
            && range.contains(Point::new(Line(buffer_line), Column(col + 1))))
}

/// 单行两段发射:bg 段(每格一整格 quad)先行,字形段随后。与
/// [`build_instances`](全量黄金源)的行内逻辑逐格对齐——两处必须同步修改,
/// 黄金对拍测试锁定等价。
#[allow(clippy::too_many_arguments)] // 同 build_rows:渲染参数组
fn build_row(
    grid: &Grid<Cell>,
    line: usize,
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
    selection: Option<&SelectionRange>,
    display_offset: usize,
    cursor: Option<CursorShape>,
) -> RowInst {
    let cols = grid.columns();
    let cursor_pos = grid.cursor.point;
    let cursor_line = usize::try_from(cursor_pos.line.0).unwrap_or(usize::MAX);
    let cursor_col = cursor_pos.column.0;
    let buffer_line = line as i32 - display_offset as i32;
    let mut row = RowInst::default();
    row.bg.reserve(cols);
    let y = line as f32 * metrics.line_height;
    let mut bar = None;
    for col in 0..cols {
        let cell = &grid[Line(buffer_line)][Column(col)];
        let cursor_here = display_offset == 0 && line == cursor_line && col == cursor_col;
        let (mut fg, mut bg) = cell_colors(cell, selection, buffer_line, col, palette);
        // 光标按形状(D20):块=反色交换;条形=字可见,bar 单独发;
        // None(闪烁关闭相位)=不画
        if cursor_here {
            match cursor {
                Some(CursorShape::Block) => std::mem::swap(&mut fg, &mut bg),
                Some(shape) => {
                    bar = Some((shape, col as f32 * metrics.cell_width, y));
                }
                None => {}
            }
        }
        let x = col as f32 * metrics.cell_width;
        if cell.hyperlink().is_some() {
            // OSC 8 链接下划线(D28),与 build_instances 同序同构
            row.glyphs.insert(0, link_underline(x, y, metrics, fg));
        }
        // SGR 下划线族(T5):与 build_instances 同序同构
        row.glyphs
            .extend(underline_quads(cell.flags, x, y, metrics, fg));
        row.bg.push(blank(x, y, metrics, fg, bg));
    }
    if let Some((shape, x, y)) = bar {
        row.glyphs
            .insert(0, cursor_bar(shape, x, y, metrics, palette));
    }
    // 字形段:同 style 连续段先试整段 shaping(连字),不成回退逐字
    let mut col = 0usize;
    while col < cols {
        let cell = &grid[Line(buffer_line)][Column(col)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.c == ' ' {
            col += 1;
            continue;
        }
        let style = GlyphStyle::from_flags(cell.flags);
        let mut end = col + 1;
        while end < cols {
            let next = &grid[Line(buffer_line)][Column(end)];
            if next.flags.contains(Flags::WIDE_CHAR_SPACER)
                || next.c == ' '
                || GlyphStyle::from_flags(next.flags) != style
            {
                break;
            }
            end += 1;
        }
        // 整段 shaping:cluster 步进——单字符 cluster 走逐字路由(缓存
        // 路径零额外成本),连字 cluster 首格承载合成字形
        let mut shaped = None;
        if end - col >= 2 {
            let chars: Vec<char> = (col..end)
                .map(|c| grid[Line(buffer_line)][Column(c)].c)
                .collect();
            shaped = router.route_run(&chars, style);
        }
        let Some(clusters) = shaped else {
            let (fg, bg) = cell_colors(cell, selection, buffer_line, col, palette);
            let x = col as f32 * metrics.cell_width;
            let g = router.route(cell.c, style);
            row.glyphs.push(glyph_instance(x, y, &g, fg, bg));
            col += 1;
            continue;
        };
        let mut pos = col;
        for cluster in clusters {
            let cell_c = &grid[Line(buffer_line)][Column(pos)];
            let (fg, bg) = cell_colors(cell_c, selection, buffer_line, pos, palette);
            let x = pos as f32 * metrics.cell_width;
            match cluster.glyph {
                Some(g) => row.glyphs.push(glyph_instance(x, y, &g, fg, bg)),
                None => {
                    let g = router.route(cell_c.c, style);
                    row.glyphs.push(glyph_instance(x, y, &g, fg, bg));
                }
            }
            pos += cluster.len.max(1);
        }
        col = pos;
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
    use crate::font::sprite::draw;

    /// 可编程假路由:每字符回固定字形,便于断言几何。
    struct FakeRouter {
        atlas_w: f32,
        atlas_h: f32,
        glyph_w: f32,
        glyph_h: f32,
        #[allow(dead_code)]
        wide: fn(char) -> bool,
    }
    impl crate::font::router::GlyphRouter for FakeRouter {
        fn route(
            &mut self,
            _ch: char,
            _style: crate::font::GlyphStyle,
        ) -> crate::font::router::GlyphInfo {
            crate::font::router::GlyphInfo {
                uv: [
                    0.25,
                    0.25,
                    self.glyph_w / self.atlas_w,
                    self.glyph_h / self.atlas_h,
                ],
                size_px: [self.glyph_w, self.glyph_h],
                offset_px: [1.0, 2.0],
                color: false,
            }
        }
    }

    fn metrics_8x16() -> FontMetrics {
        FontMetrics {
            cell_width: 8.0,
            line_height: 16.0,
            ascent: 13.0,
            descent: 3.0,
        }
    }

    /// #90 对比度强制:低对比被推达标、已达标直通、0 关闭、方向正确
    #[test]
    fn contrast_enforcement() {
        // 深底深字(黑底 #111):推白
        let dark_fg = enforce_contrast([17, 17, 17], [10, 10, 10], 0.6);
        let lum = rel_lum(dark_fg);
        let ratio =
            (lum.max(rel_lum([10, 10, 10])) + 0.05) / (lum.min(rel_lum([10, 10, 10])) + 0.05);
        assert!(ratio >= 0.59, "深底深字推白后 ratio={ratio}");
        // 白底白字:推黑
        let light_fg = enforce_contrast([240, 240, 240], [250, 250, 250], 0.6);
        assert!(rel_lum(light_fg) < 0.5, "白底白字推黑");
        // 已达标:直通
        assert_eq!(
            enforce_contrast([255, 255, 255], [0, 0, 0], 0.6),
            [255, 255, 255]
        );
        // 0 关闭
        assert_eq!(
            enforce_contrast([17, 17, 17], [10, 10, 10], 0.0),
            [17, 17, 17]
        );
    }

    #[test]
    fn instance_layout_is_pod_64() {
        assert_eq!(std::mem::size_of::<CellInstance>(), 64);
        assert_eq!(std::mem::offset_of!(CellInstance, pos_uv), 0);
        assert_eq!(std::mem::offset_of!(CellInstance, size_uv), 16);
        assert_eq!(std::mem::offset_of!(CellInstance, fg), 32);
        assert_eq!(std::mem::offset_of!(CellInstance, bg), 48);
    }

    #[test]
    fn strip_exit_dot_octagon_and_colors() {
        let mut r = FakeRouter {
            atlas_w: 64.0,
            atlas_h: 64.0,
            glyph_w: 4.0,
            glyph_h: 8.0,
            wide: |_| false,
        };
        // 双标签:状态点属第二行,单标签整行隐藏
        let titles = vec![("a".to_string(), true), ("b".to_string(), false)];
        let palette = Palette::DEFAULT;
        let none_len = strip_quads(
            &titles,
            &mut r,
            &metrics_8x16(),
            &palette,
            &StripMetrics::default(),
            Default::default(),
        )
        .len();
        let dot = strip_quads(
            &titles,
            &mut r,
            &metrics_8x16(),
            &palette,
            &StripMetrics::default(),
            StripExtras {
                exit_dot: Some(ExitDot::Green),
                ..Default::default()
            },
        );
        assert_eq!(dot.len() - none_len, 1, "真圆 = 1 圆角 quad;None 不画");
        let q = &dot[none_len];
        assert_eq!(q.size_uv, [7.0, 7.0, 0.0, 0.0]);
        assert_eq!(q.bg[3], 3.5, "上圆角半径在 bg.w");
        assert_eq!(q.fg[3], 3.5, "下圆角半径在 fg.w");
        let to_u8 = |c: f32| (c * 255.0 + 0.5) as u8;
        assert_eq!(
            [to_u8(q.bg[0]), to_u8(q.bg[1]), to_u8(q.bg[2])],
            [
                palette.colors[2].r,
                palette.colors[2].g,
                palette.colors[2].b
            ],
            "绿点取调色板 slot 2(经 bg.rgb 渲染)"
        );
    }

    /// 均分填充契约:等宽、随可用宽放大、min 兜底;可用域到 "+" 左缘
    #[test]
    fn equal_tab_w_fills_and_clamps() {
        let sm = StripMetrics::default();
        assert_eq!(equal_tab_w(0, 400.0, &sm), sm.tab_min_w, "无标签 = min");
        assert_eq!(
            equal_tab_w(3, 40.0, &sm),
            sm.tab_min_w,
            "极窄可用 = min 兜底(溢出滚动接管)"
        );
        let body = 900.0 - sm.plus_w - 14.0 * sm.k - sm.tab_gap * 2.0;
        assert_eq!(equal_tab_w(2, 900.0, &sm), body / 2.0, "两标签均分");
    }

    #[test]
    fn strip_two_tabs_active_block_and_close() {
        let mut r = FakeRouter {
            atlas_w: 64.0,
            atlas_h: 64.0,
            glyph_w: 4.0,
            glyph_h: 8.0,
            wide: |_| false,
        };
        let titles = vec![("a".to_string(), false), ("b".to_string(), true)];
        let out = strip_quads(
            &titles,
            &mut r,
            &metrics_8x16(),
            &Palette::DEFAULT,
            &StripMetrics::default(),
            StripExtras {
                caption_x: Some(400.0),
                focus: 1.0,
                ..Default::default()
            },
        );
        let sm = StripMetrics::default();
        // 活动标签块:第二行整高(title_h 起)、上圆下方、面色 = 终端底
        let want_w = equal_tab_w(2, 400.0 + 3.0 * sm.caption_w, &sm);
        assert!(
            out.iter().any(|q| q.size_uv[0] == want_w
                && q.size_uv[1] == sm.h - sm.title_h
                && q.pos_uv[1] == sm.title_h
                && q.bg[3] == sm.tab_top_r
                && q.fg[3] == 0.0),
            "活动标签块(第二行整高,上圆下方,等宽 {want_w})"
        );
        // 条底浅灰:聚焦态 = 终端底 18% 提亮
        let p = Palette::DEFAULT;
        let want_bar = [
            p.bg.r + ((255 - p.bg.r) as f32 * 0.18 + 0.5) as u8,
            p.bg.g + ((255 - p.bg.g) as f32 * 0.18 + 0.5) as u8,
            p.bg.b + ((255 - p.bg.b) as f32 * 0.18 + 0.5) as u8,
        ];
        assert!(
            out.iter().any(
                |q| q.size_uv == [400.0 + 3.0 * sm.caption_w, sm.h, 0.0, 0.0]
                    && q.bg[0] == want_bar[0] as f32 / 255.0
                    && q.bg[1] == want_bar[1] as f32 / 255.0
                    && q.bg[2] == want_bar[2] as f32 / 255.0
            ),
            "条底 = 浅灰(18% 提亮)"
        );
        // 关闭钮字形(E8BB)在:active 恒显(第二行)
        assert!(
            out.iter()
                .any(|q| q.size_uv[2] > 0.0 && q.pos_uv[1] >= sm.title_h),
            "× 字形存在(第二行)"
        );
    }

    /// 单标签 = 第二行整行隐藏(上游 hasTabBar 同款):无标签块、无 "+",
    /// 条高只余标题行
    #[test]
    fn strip_single_tab_hides_tab_row() {
        let mut r = FakeRouter {
            atlas_w: 64.0,
            atlas_h: 64.0,
            glyph_w: 4.0,
            glyph_h: 8.0,
            wide: |_| false,
        };
        let one = vec![("solo".to_string(), true)];
        let out = strip_quads(
            &one,
            &mut r,
            &metrics_8x16(),
            &Palette::DEFAULT,
            &StripMetrics::default(),
            StripExtras {
                caption_x: Some(400.0),
                focus: 1.0,
                ..Default::default()
            },
        );
        let sm = StripMetrics::default();
        // 条底只到标题行底
        assert!(
            out.iter()
                .any(|q| q.size_uv == [400.0 + 3.0 * sm.caption_w, sm.title_h, 0.0, 0.0]),
            "单标签:条高 = 标题行"
        );
        assert!(
            !out.iter().any(|q| q.bg[3] > 0.0 && q.size_uv[1] > 1.0),
            "无标签块/胶囊"
        );
        assert!(
            out.iter()
                .any(|q| q.size_uv[2] > 0.0 && q.pos_uv[1] < sm.title_h),
            "标题字形在第一行"
        );
        assert!(
            !out.iter()
                .any(|q| q.size_uv[2] > 0.0 && q.pos_uv[1] >= sm.title_h),
            "第二行无任何字形(隐藏)"
        );
    }

    #[test]
    fn three_quadrant_blocks_carve_correct_corner() {
        let (w, h) = (10.0, 21.0);
        let idx = |x: usize, y: usize| y * w as usize + x;
        let cov = draw(0x259B, w, h).unwrap().2;
        assert!(cov[idx(0, 0)] == 255, "259B 左上应满");
        assert!(cov[idx(9, 20)] == 0, "259B 右下应空");
        let cov = draw(0x259C, w, h).unwrap().2;
        assert!(cov[idx(9, 0)] == 255, "259C 右上应满");
        assert!(cov[idx(0, 20)] == 0, "259C 左下应空");
    }

    #[test]
    fn out_of_range_returns_none() {
        assert!(draw(0x41, 10.0, 21.0).is_none(), "字母回落字体");
        assert!(draw(0x25FF, 10.0, 21.0).is_none(), "未覆盖几何形状回落字体");
    }

    #[test]
    fn powerline_triangles_fill_cell() {
        let (w, h) = (10.0, 21.0);
        let cov = draw(0xE0B0, w, h).unwrap().2;
        let idx = |x: usize, y: usize| y * w as usize + x;
        for y in 0..h as usize {
            assert!(cov[idx(0, y)] > 80, "E0B0 左缘第 {y} 行缺墨");
        }
        assert!(cov[idx(9, 10)] > 80, "E0B0 斜边中点缺墨");
    }

    #[test]
    fn braille_dots_follow_bits() {
        let (w, h) = (10.0, 21.0);
        let blank = draw(0x2800, w, h).unwrap().2;
        assert!(blank.iter().all(|&v| v == 0), "空白盲文无墨");
        let one = draw(0x2801, w, h).unwrap().2;
        assert!(one.iter().any(|&v| v > 200), "Bit1 点应有墨");
    }
}

/// M5c/T3(D34):渲染吞吐基准——build_rows 全管线路径(feed→damage→
/// 重建),无 GPU 依赖,CI 可跑。80×24×500 行回放,cells/s。
/// `#ignored` + `--release`:`cargo test -p mica-render bench -- --ignored --nocapture`。
/// 输出可被 CI 的 perf job 抓取:`BENCH cells_per_sec=<f64> frame_us=<f64>`。
/// 2026-10-12 重建:bbb71d2 标签栏重写误删本模块,perf job 抓零红(实证)。
#[cfg(test)]
mod bench {
    use super::*;
    use crate::font::router::{GlyphInfo, GlyphRouter};
    use mica_core::config::palette::Palette;
    use mica_core::surface::{Damage, ScreenSize, Surface};

    struct BenchRouter;
    impl GlyphRouter for BenchRouter {
        fn route(&mut self, ch: char, _style: crate::font::GlyphStyle) -> GlyphInfo {
            GlyphInfo {
                // 4×8 定形:uv/尺寸稳定,图集无关(不测上传,测 CPU 组装)
                uv: [0.25, 0.25, 4.0 / 256.0, 8.0 / 256.0],
                size_px: [4.0, 8.0],
                offset_px: [1.0, 2.0],
                color: ch as u32 & 0xff == 1, // 极少彩色
            }
        }
    }

    #[test]
    #[ignore]
    fn bench_build_rows_throughput() {
        let mut s = Surface::new(ScreenSize::new(80, 24));
        // 混合负载:SGR/文本/CJK 交替,代表真实色彩输出
        let line = b"\x1b[32mhello world 0123456789 \x1b[m plain text here\r\n";
        let mut router = BenchRouter;
        let metrics = crate::font::metrics::FontMetrics {
            cell_width: 9.0,
            line_height: 19.0,
            ascent: 15.0,
            descent: 4.0,
        };
        let palette = Palette::DEFAULT;
        // 预喂 500 行(全量 damage 状态)
        for _ in 0..500 {
            s.feed(line);
        }
        let mut rows = Vec::new();
        let t0 = std::time::Instant::now();
        let mut total_cells = 0usize;
        for round in 0..3 {
            let damage = if round == 0 {
                Damage::Full
            } else {
                Damage::Lines(vec![round % 24])
            };
            rows.clear();
            build_rows(
                &s,
                &mut router,
                &metrics,
                &palette,
                None,
                0,
                None,
                &damage,
                &mut rows,
            );
            total_cells += rows
                .iter()
                .map(|r| r.bg.len() + r.glyphs.len())
                .sum::<usize>();
        }
        let elapsed = t0.elapsed();
        // 单帧(全量)基准
        let t1 = std::time::Instant::now();
        rows.clear();
        build_rows(
            &s,
            &mut router,
            &metrics,
            &palette,
            None,
            0,
            None,
            &Damage::Full,
            &mut rows,
        );
        let frame_us = t1.elapsed().as_secs_f64() * 1e6;
        let cells_per_sec = total_cells as f64 / elapsed.as_secs_f64();
        println!("BENCH cells_per_sec={cells_per_sec:.0} frame_us={frame_us:.0}");
    }
}
