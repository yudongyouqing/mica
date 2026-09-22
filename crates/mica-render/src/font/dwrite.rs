//! DirectWrite 光栅化路由。仅 Windows(开发期以交叉 clippy 验证,真机冒烟在 T7);
//! API 形态对照 windows 0.62.2 生成绑定逐一核实(2026-09-22)。
//!
//! 家族链语义:按序找第一个含该字符的家族;缺家族整条跳过,全缺则 new 报错。
//! 空白字符与全链 miss 都路由到空白 GlyphInfo,不占图集。
//! 光栅化约定:NATURAL_SYMMETRIC + CLEARTYPE_3x1 纹理(每像素 3 字节),
//! 取 R 通道作 8 位灰度 coverage(合 R8 图集);基线原点取 (0,0),
//! 字形 bounds 按基线相对解读,offset_px = ascent + bounds.top。
//! 注意纹理类型必须匹配渲染模式——ALIASED_1x1 在 NATURAL 系下 bounds 恒空。

use std::collections::HashMap;
use std::mem::ManuallyDrop;

use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE,
    DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_GLYPH_METRICS, DWRITE_GLYPH_OFFSET,
    DWRITE_GLYPH_RUN, DWRITE_MEASURING_MODE_NATURAL, DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
    DWRITE_TEXTURE_CLEARTYPE_3x1, DWriteCreateFactory, IDWriteFactory, IDWriteFont,
    IDWriteFontCollection, IDWriteFontFace,
};
use windows::core::{BOOL, HSTRING};

use super::GlyphStyle;
use super::atlas::{GlyphAtlas, GlyphBitmap};
use super::metrics::FontMetrics;
use super::router::{GlyphInfo, GlyphRouter};

/// 一个家族的三个样式面(plain/bold/italic;bold-italic 复用 bold 面,M2 若需要再加)。
struct FamilyFaces {
    name: String,
    plain: IDWriteFont,
    bold: IDWriteFont,
    italic: IDWriteFont,
    /// 'M' 的 advance(design units)——等宽字体即 cell 宽
    max_advance_du: u32,
}

impl FamilyFaces {
    /// 该家族 plain 面是否含此字符(回退链探测;COM 失败按不含处理)。
    fn has_character(&self, ch: char) -> bool {
        // SAFETY: 常规 COM 调用,无指针出参
        unsafe {
            self.plain
                .HasCharacter(ch as u32)
                .is_ok_and(|b| b.as_bool())
        }
    }
}

pub struct DwriteRouter {
    factory: IDWriteFactory,
    families: Vec<FamilyFaces>,
    em_size_dip: f32,
    metrics: FontMetrics,
    atlas: GlyphAtlas,
    /// 图集版本哨兵:grow 会重排搬动全部条目,缓存里的 UV 是按当时尺寸归一的
    last_atlas_version: u64,
    cache: HashMap<(char, u8), GlyphInfo>,
}

/// 缓存键的样式位:bold=bit0,italic=bit1。
fn style_bits(s: GlyphStyle) -> u8 {
    u8::from(s.bold) | (u8::from(s.italic) << 1)
}

/// 全零字形:空白与兜底共用,不进图集。
fn blank_glyph() -> GlyphInfo {
    GlyphInfo {
        uv: [0.0; 4],
        size_px: [0.0; 2],
        offset_px: [0.0; 2],
    }
}

/// 码点 → glyph index(0 = .notdef)。原始指针形签名(0.62 无 GetGlyphIndicesW)。
fn glyph_index_for(face: &IDWriteFontFace, ch: char) -> anyhow::Result<u16> {
    let code = [ch as u32];
    let mut idx = [0u16];
    // SAFETY: 一进一出,指针与计数严格对应
    unsafe { face.GetGlyphIndices(code.as_ptr(), 1, idx.as_mut_ptr())? };
    Ok(idx[0])
}

/// font → face → 非零字形索引;.notdef 与 COM 失败都归 None(供 plain 面回退)。
/// face 随 Some 一并返回(run 构造要用);miss 时 face 在此就地释放,无泄漏。
fn face_with_glyph(font: &IDWriteFont, ch: char) -> Option<(IDWriteFontFace, u16)> {
    // SAFETY: CreateFontFace 返回带引用计数的 face
    let face = unsafe { font.CreateFontFace().ok()? };
    let glyph = glyph_index_for(&face, ch).ok()?;
    (glyph != 0).then_some((face, glyph))
}

impl DwriteRouter {
    pub fn new(font_size_pt: f32, families: &[&str]) -> anyhow::Result<Self> {
        // SAFETY: 工厂创建,T 由 turbofish 指定
        let factory = unsafe { DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED)? };
        let mut collection: Option<IDWriteFontCollection> = None;
        // SAFETY: 出参由本侧 Option 承接
        unsafe { factory.GetSystemFontCollection(&mut collection, true)? };
        let collection = collection.ok_or_else(|| anyhow::anyhow!("no system font collection"))?;

        let em_size_dip = font_size_pt * 96.0 / 72.0;
        let mut resolved = Vec::new();
        let mut head_dm = None;
        for name in families {
            let wide: HSTRING = (*name).into();
            let mut index = 0u32;
            let mut exists = BOOL::default();
            // SAFETY: index/exists 出参由本侧持有
            unsafe { collection.FindFamilyName(&wide, &mut index, &mut exists)? };
            if !exists.as_bool() {
                continue; // 回退链语义:家族缺失就跳过
            }
            // SAFETY: index 来自 FindFamilyName 的成功探测
            let family = unsafe { collection.GetFontFamily(index)? };
            let pick = |weight: DWRITE_FONT_WEIGHT, style: DWRITE_FONT_STYLE| unsafe {
                family.GetFirstMatchingFont(weight, DWRITE_FONT_STRETCH_NORMAL, style)
            };
            let plain = pick(DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL)?;
            let bold = pick(DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_STYLE_NORMAL)?;
            let italic = pick(DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_ITALIC)?;
            // SAFETY: IDWriteFont::CreateFontFace 无额外前提
            let face = unsafe { plain.CreateFontFace()? };
            let mut dm = DWRITE_FONT_METRICS::default();
            // SAFETY: GetMetrics 无 Result,只填充本侧结构体
            unsafe { plain.GetMetrics(&mut dm) };
            // 等宽判定用的 'M' advance(design units)
            let m_idx = [glyph_index_for(&face, 'M')?];
            let mut gm = [DWRITE_GLYPH_METRICS::default()];
            // SAFETY: 一组度量出参,计数 1
            unsafe { face.GetDesignGlyphMetrics(m_idx.as_ptr(), 1, gm.as_mut_ptr(), false)? };
            let max_advance_du = gm[0].advanceWidth;
            head_dm.get_or_insert(dm); // 首个家族的度量是格子尺寸的来源
            resolved.push(FamilyFaces {
                name: (*name).to_string(),
                plain,
                bold,
                italic,
                max_advance_du,
            });
        }
        if resolved.is_empty() {
            anyhow::bail!("no font family resolved from {families:?}");
        }
        let head_dm = head_dm.expect("resolved families checked non-empty");
        let metrics = FontMetrics::from_dwrite(
            head_dm.ascent,
            head_dm.descent,
            head_dm.lineGap,
            head_dm.designUnitsPerEm,
            em_size_dip,
            resolved[0].max_advance_du,
        );
        Ok(Self {
            factory,
            families: resolved,
            em_size_dip,
            metrics,
            atlas: GlyphAtlas::new(256, 256),
            last_atlas_version: 0,
            cache: HashMap::new(),
        })
    }

    pub fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    pub fn atlas(&self) -> &GlyphAtlas {
        &self.atlas
    }

    /// app 每帧比对此修订号,变了才 `Renderer::set_atlas`——内容变更
    /// (普通 insert 或 grow)都得重传,与是否重排无关。
    pub fn atlas_revision(&self) -> u64 {
        self.atlas.revision()
    }

    pub fn families_in_use(&self) -> Vec<String> {
        self.families.iter().map(|f| f.name.clone()).collect()
    }

    fn family_for(&self, ch: char) -> Option<&FamilyFaces> {
        self.families.iter().find(|f| f.has_character(ch))
    }

    /// 真光栅化:失败(None)由 route 落空白兜底并同样进缓存。
    fn rasterize(&mut self, ch: char, style: GlyphStyle) -> Option<GlyphInfo> {
        if matches!(ch, ' ' | '\u{0}') {
            return Some(blank_glyph());
        }
        let family = self.family_for(ch)?;
        let styled_font = match (style.bold, style.italic) {
            (true, _) => Some(&family.bold),
            (false, true) => Some(&family.italic),
            _ => None,
        };
        // 样式面缺字形(如斜体面缺 CJK)→ 回退家族 plain 面;
        // plain 也 miss(None)即全链空白,交 route 兜底
        let (face, glyph) = match styled_font.and_then(|f| face_with_glyph(f, ch)) {
            Some(pair) => pair,
            None => face_with_glyph(&family.plain, ch)?,
        };

        let em = self.em_size_dip;
        let offset = DWRITE_GLYPH_OFFSET {
            advanceOffset: 0.0,
            ascenderOffset: 0.0,
        };
        let mut run = DWRITE_GLYPH_RUN {
            // ManuallyDrop:结构体要求;分析完成后手动 take 释放引用计数
            fontFace: ManuallyDrop::new(Some(face)),
            fontEmSize: em,
            glyphCount: 1,
            glyphIndices: &glyph,
            glyphAdvances: &em, // 分析器只用索引渲染形状;advance 由度量侧决定格子
            glyphOffsets: &offset,
            isSideways: BOOL::default(),
            bidiLevel: 0,
        };
        // 基线原点取 (0,0):bounds 的"基线相对 vs 绝对"两种语义在原点为 0 时数值重合
        // SAFETY: run 各指针均指向本侧存活数据;pixelsPerDip=1 ⇒ DIP 即像素;
        // DEFAULT 非法于此调用(MSDN),对称灰度 AA 正合 R8 图集
        let analysis = unsafe {
            self.factory.CreateGlyphRunAnalysis(
                &run,
                1.0,
                None,
                DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
                DWRITE_MEASURING_MODE_NATURAL,
                0.0,
                0.0,
            )
        };
        // SAFETY: 无论成败先取回 face 所有权——错误路径同样不漏引用计数
        // (run 之后不再使用)
        drop(unsafe { ManuallyDrop::take(&mut run.fontFace) });
        let analysis = analysis.ok()?;

        // SAFETY: 常规查询。纹理类型必须匹配渲染模式:NATURAL_SYMMETRIC 下
        // ALIASED_1x1 的 bounds 恒空(每字符都落空白兜底 → 全屏无墨,黑屏);
        // CLEARTYPE_3x1 每像素 3 字节,取 R 通道即 8 位灰度 coverage
        let bounds = unsafe {
            analysis
                .GetAlphaTextureBounds(DWRITE_TEXTURE_CLEARTYPE_3x1)
                .ok()?
        };
        let (w, h) = (
            (bounds.right - bounds.left).max(0),
            (bounds.bottom - bounds.top).max(0),
        );
        if w == 0 || h == 0 {
            return Some(blank_glyph()); // 空字形(如组合符单发)不占图集
        }
        let (w, h) = (w as u32, h as u32);
        let mut raw = vec![0u8; w as usize * h as usize * 3]; // 3x1:RGB 亚像素
        // SAFETY: 缓冲恰好 bounds 大小 × 3 字节/像素
        unsafe {
            analysis
                .CreateAlphaTexture(DWRITE_TEXTURE_CLEARTYPE_3x1, &bounds, &mut raw)
                .ok()?
        };
        // 亚像素三通道即三份水平错位的 coverage:取 R 通道,单通道做墨量
        // 是亚像素纹理转灰度的标准做法(等价灰度 AA,合 R8 图集)
        let pixels: Vec<u8> = raw.as_chunks::<3>().0.iter().map(|px| px[0]).collect();
        let rect = self.atlas.insert(&GlyphBitmap {
            width: w,
            height: h,
            pixels,
        });
        let (aw, ah) = (self.atlas.width() as f32, self.atlas.height() as f32);
        // 字形相对格左上:水平 = bounds.left;垂直 = ascent + bounds.top(top 为负 = 基线上方)
        Some(GlyphInfo {
            uv: [
                rect.u as f32 / aw,
                rect.v as f32 / ah,
                rect.w as f32 / aw,
                rect.h as f32 / ah,
            ],
            size_px: [w as f32, h as f32],
            offset_px: [bounds.left as f32, self.metrics.ascent + bounds.top as f32],
        })
    }
}

impl GlyphRouter for DwriteRouter {
    fn route(&mut self, ch: char, style: GlyphStyle) -> GlyphInfo {
        // 图集 grow 重排后全部条目换了位置,旧缓存的 UV 按当时尺寸归一已失效——
        // 版本号一动即清缓存(本轮新插入本就按当前尺寸计算,不受影响)
        let version = self.atlas.version();
        if version != self.last_atlas_version {
            self.cache.clear();
            self.last_atlas_version = version;
        }
        let key = (ch, style_bits(style));
        if let Some(g) = self.cache.get(&key) {
            return *g;
        }
        let info = self.rasterize(ch, style).unwrap_or_else(blank_glyph);
        self.cache.insert(key, info);
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机回归锁:光栅化链必须产出有墨字形。曾经 ALIASED_1x1 纹理配
    /// NATURAL_SYMMETRIC 模式 bounds 恒空,每字符都落空白兜底——
    /// 背景照画、全屏无墨,症状是"黑屏"(T7 冒烟发现的)。
    #[test]
    fn rasterize_yields_inked_glyphs() {
        let mut r = DwriteRouter::new(12.0, crate::font::DEFAULT_FAMILIES).expect("router");
        for ch in ['A', 'a', '0', '中'] {
            let g = r.route(ch, GlyphStyle::PLAIN);
            assert!(
                g.size_px[0] > 0.0 && g.size_px[1] > 0.0,
                "{ch:?} 字形尺寸为零(blank 兜底泄漏)"
            );
            assert!(
                g.uv[2] > 0.0 && g.uv[3] > 0.0,
                "{ch:?} uv 尺寸为零(blank 兜底泄漏)"
            );
        }
        let atlas = r.atlas();
        let ink = atlas.texture().iter().filter(|&&p| p > 0).count();
        assert!(ink > 0, "图集无墨:路由未产出任何位图");
    }

    /// bold 面回归锁:bold 样式必须路由到与 plain 不同的字形位图。
    /// (T7 冒烟发现 bold 不变粗时先跑此测试分诊:红了说明 GetFirstMatchingFont
    /// 对 BOLD weight 回落到了 regular 面——家族缺 bold 变体;绿了说明
    /// 终端侧链路完好,问题在输入侧没发出 SGR 1。)
    #[test]
    fn bold_face_gives_distinct_glyph() {
        let mut r = DwriteRouter::new(12.0, crate::font::DEFAULT_FAMILIES).expect("router");
        let plain = r.route('B', GlyphStyle::PLAIN);
        let bold = r.route(
            'B',
            GlyphStyle {
                bold: true,
                italic: false,
            },
        );
        assert!(
            plain.uv != bold.uv || plain.size_px != bold.size_px,
            "bold 与 plain 字形位图相同:.bold 面未生效(GetFirstMatchingFont 回落?)"
        );
    }

    /// 字形几何落在格内:offset + 尺寸以格左上为参照,不越出格子行高。
    #[test]
    fn glyph_geometry_stays_within_line() {
        let mut r = DwriteRouter::new(12.0, crate::font::DEFAULT_FAMILIES).expect("router");
        let m = r.metrics();
        for ch in ['A', 'g', '中'] {
            let g = r.route(ch, GlyphStyle::PLAIN);
            let top = g.offset_px[1];
            let bottom = top + g.size_px[1];
            assert!(
                top >= 0.0 && bottom <= m.line_height + 1.0,
                "{ch:?} 纵向越界: top={top} bottom={bottom} line={}",
                m.line_height
            );
        }
    }
}
