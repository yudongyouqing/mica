//! DirectWrite 光栅化路由。仅 Windows(开发期以交叉 clippy 验证,真机冒烟在 T7);
//! API 形态对照 windows 0.62.2 生成绑定逐一核实(2026-09-22)。
//!
//! 家族链语义:按序找第一个含该字符的家族;缺家族整条跳过,全缺则 new 报错。
//! 空白字符与全链 miss 都路由到空白 GlyphInfo,不占图集。

use std::collections::HashMap;
use std::mem::ManuallyDrop;

use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE,
    DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_GLYPH_METRICS, DWRITE_GLYPH_OFFSET,
    DWRITE_GLYPH_RUN, DWRITE_MEASURING_MODE_NATURAL, DWRITE_RENDERING_MODE_DEFAULT,
    DWRITE_TEXTURE_ALIASED_1x1, DWriteCreateFactory, IDWriteFactory, IDWriteFont,
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
            cache: HashMap::new(),
        })
    }

    pub fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    pub fn atlas(&self) -> &GlyphAtlas {
        &self.atlas
    }

    /// app 每帧比对此版本号,变了才 `Renderer::set_atlas`。
    pub fn atlas_version(&self) -> u64 {
        self.atlas.version()
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
        let font = match (style.bold, style.italic) {
            (true, _) => &family.bold,
            (false, true) => &family.italic,
            _ => &family.plain,
        };
        // SAFETY: CreateFontFace 返回带引用计数的 face
        let face = unsafe { font.CreateFontFace().ok()? };
        let glyph = glyph_index_for(&face, ch).ok()?;
        if glyph == 0 {
            return None; // .notdef → 空白兜底
        }

        let em = self.em_size_dip;
        let baseline_y = self.metrics.ascent;
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
        // SAFETY: run 各指针均指向本侧存活数据;pixelsPerDip=1 ⇒ DIP 即像素
        let analysis = unsafe {
            self.factory
                .CreateGlyphRunAnalysis(
                    &run,
                    1.0,
                    None,
                    DWRITE_RENDERING_MODE_DEFAULT,
                    DWRITE_MEASURING_MODE_NATURAL,
                    0.0,
                    baseline_y,
                )
                .ok()?
        };
        // SAFETY: 取回 face 所有权以正常释放引用计数(run 之后不再使用)
        drop(unsafe { ManuallyDrop::take(&mut run.fontFace) });

        // SAFETY: 常规查询
        let bounds = unsafe {
            analysis
                .GetAlphaTextureBounds(DWRITE_TEXTURE_ALIASED_1x1)
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
        let mut pixels = vec![0u8; w as usize * h as usize]; // ALIASED_1x1:1 字节/像素
        // SAFETY: 缓冲恰好 bounds 大小,1 字节/像素
        unsafe {
            analysis
                .CreateAlphaTexture(DWRITE_TEXTURE_ALIASED_1x1, &bounds, &mut pixels)
                .ok()?
        };
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
            offset_px: [bounds.left as f32, baseline_y + bounds.top as f32],
        })
    }
}

impl GlyphRouter for DwriteRouter {
    fn route(&mut self, ch: char, style: GlyphStyle) -> GlyphInfo {
        let key = (ch, style_bits(style));
        if let Some(g) = self.cache.get(&key) {
            return *g;
        }
        let info = self.rasterize(ch, style).unwrap_or_else(blank_glyph);
        self.cache.insert(key, info);
        info
    }
}
