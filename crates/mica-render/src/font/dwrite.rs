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
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_FEATURE, DWRITE_FONT_FEATURE_TAG_CONTEXTUAL_ALTERNATES,
    DWRITE_FONT_FEATURE_TAG_STANDARD_LIGATURES, DWRITE_FONT_METRICS, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE, DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_GLYPH_IMAGE_FORMATS,
    DWRITE_GLYPH_IMAGE_FORMATS_COLR, DWRITE_GLYPH_METRICS, DWRITE_GLYPH_OFFSET, DWRITE_GLYPH_RUN,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC, DWRITE_SCRIPT_ANALYSIS,
    DWRITE_SCRIPT_SHAPES, DWRITE_SHAPING_GLYPH_PROPERTIES, DWRITE_SHAPING_TEXT_PROPERTIES,
    DWRITE_TEXTURE_CLEARTYPE_3x1, DWRITE_TYPOGRAPHIC_FEATURES, DWriteCreateFactory, IDWriteFactory,
    IDWriteFactory4, IDWriteFont, IDWriteFontCollection, IDWriteFontFace,
};
use windows::core::{BOOL, HSTRING, Interface};
use windows_numerics::Vector2;

use super::GlyphStyle;
use super::atlas::{GlyphAtlas, GlyphBitmap, GlyphFormat};
use super::metrics::FontMetrics;
use super::router::{ClusterLayout, GlyphInfo, GlyphRouter};

/// 一个家族的三个样式面(plain/bold/italic;bold-italic 复用 bold 面,M2 若需要再加)。
struct FamilyFaces {
    name: String,
    plain: IDWriteFont,
    bold: IDWriteFont,
    italic: IDWriteFont,
    /// M5a/T7:bold+italic 面(此前 (true,_) 吞 italic,italic 只在非粗体下生效)
    bold_italic: IDWriteFont,
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
    /// 连字 shaping(M2b/T4):启动探测(主字体对 =>/>= 系列产合成字形)
    /// 不通过则整条 route_run 走 None,行为与 M2a 完全一致
    ligatures: bool,
    /// 连字缓存:key = (字符段, 样式位);命中免 shaping
    run_cache: HashMap<(Vec<char>, u8), Vec<ClusterLayout>>,
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
        color: false,
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
            let bold_italic = pick(DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_STYLE_ITALIC)?;
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
                bold_italic,
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
        // 连字探测(D16):主字体 shape 若干常见连字序列,全都不产合成
        // 字形则关闭整条 shaping 路径(探测在构造期,零运行时成本)
        let factory_probe = factory.clone();
        // 探测全家族:链中任一支持连字即启用(承载段不支持时 cluster
        // 自然 1:1 回退逐字,不会误渲染)
        let ligature_family = resolved
            .iter()
            .find(|f| probe_ligatures(&factory_probe, &f.plain, em_size_dip));
        let ligatures = ligature_family.is_some();
        if let Some(f) = ligature_family {
            eprintln!("ligatures: enabled for {}", f.name);
        }
        Ok(Self {
            factory,
            families: resolved,
            ligatures,
            em_size_dip,
            metrics,
            atlas: GlyphAtlas::new(256, 256),
            last_atlas_version: 0,
            cache: HashMap::new(),
            run_cache: HashMap::new(),
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
        // 样式面候选级联(T7):bold+italic 先专面,miss 降级保样式优先级
        // italic > bold(斜体信息比粗细更难从字形回推);再 miss 回退 plain
        let candidates: &[&IDWriteFont] = match (style.bold, style.italic) {
            (true, true) => &[&family.bold_italic, &family.italic, &family.bold],
            (true, false) => &[&family.bold],
            (false, true) => &[&family.italic],
            _ => &[],
        };
        let mut hit = None;
        for f in candidates {
            if let Some(pair) = face_with_glyph(f, ch) {
                hit = Some(pair);
                break;
            }
        }
        // 全部样式面 miss(如斜体面缺 CJK)→ 回退家族 plain 面;
        // plain 也 miss(None)即全链空白,交 route 兜底
        let (face, glyph) = match hit {
            Some(pair) => pair,
            None => face_with_glyph(&family.plain, ch)?,
        };

        // 彩色路径(M2b/T3):COLR 字形走层合成;非彩色字形上游报错,
        // 自然落回灰度路径——每字符仅首次路由时探测(缓存兜底)
        if let Some(info) = self.rasterize_color(&face, glyph) {
            return Some(info);
        }

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
        let coverage: Vec<u8> = raw.as_chunks::<3>().0.iter().map(|px| px[0]).collect();
        let rect = self
            .atlas
            .insert(&GlyphBitmap::from_coverage(w, h, coverage));
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
            color: false,
        })
    }

    /// 连字合成字形光栅化:单 glyph run(advance=总宽),灰度同款路径。
    /// 失败兜底 blank(该连字不显示,好过崩)。
    fn rasterize_ligature(&mut self, font: &IDWriteFont, glyph: u16, advance: f32) -> GlyphInfo {
        // SAFETY: 与 rasterize 同款 run/analysis/bounds/texture 序
        unsafe {
            let face = match font.CreateFontFace() {
                Ok(f) => f,
                Err(_) => return blank_glyph(),
            };
            let glyphs = [glyph];
            let advances = [advance];
            let offsets = [DWRITE_GLYPH_OFFSET {
                advanceOffset: 0.0,
                ascenderOffset: 0.0,
            }];
            let mut run = DWRITE_GLYPH_RUN {
                fontFace: ManuallyDrop::new(Some(face.clone())),
                fontEmSize: self.em_size_dip,
                glyphCount: 1,
                glyphIndices: glyphs.as_ptr(),
                glyphAdvances: advances.as_ptr(),
                glyphOffsets: offsets.as_ptr(),
                isSideways: BOOL::default(),
                bidiLevel: 0,
            };
            let analysis = self.factory.CreateGlyphRunAnalysis(
                &run,
                1.0,
                None,
                DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
                DWRITE_MEASURING_MODE_NATURAL,
                0.0,
                0.0,
            );
            drop(ManuallyDrop::take(&mut run.fontFace));
            let analysis = match analysis {
                Ok(a) => a,
                Err(_) => return blank_glyph(),
            };
            let bounds = match analysis.GetAlphaTextureBounds(DWRITE_TEXTURE_CLEARTYPE_3x1) {
                Ok(b) => b,
                Err(_) => return blank_glyph(),
            };
            let (w, h) = (
                (bounds.right - bounds.left).max(0),
                (bounds.bottom - bounds.top).max(0),
            );
            if w == 0 || h == 0 {
                return blank_glyph();
            }
            let mut raw = vec![0u8; (w * h) as usize * 3];
            if analysis
                .CreateAlphaTexture(DWRITE_TEXTURE_CLEARTYPE_3x1, &bounds, &mut raw)
                .is_err()
            {
                return blank_glyph();
            }
            let coverage: Vec<u8> = raw.as_chunks::<3>().0.iter().map(|px| px[0]).collect();
            let rect = self
                .atlas
                .insert(&GlyphBitmap::from_coverage(w as u32, h as u32, coverage));
            let (aw, ah) = (self.atlas.width() as f32, self.atlas.height() as f32);
            GlyphInfo {
                uv: [
                    rect.u as f32 / aw,
                    rect.v as f32 / ah,
                    rect.w as f32 / aw,
                    rect.h as f32 / ah,
                ],
                size_px: [w as f32, h as f32],
                offset_px: [bounds.left as f32, self.metrics.ascent + bounds.top as f32],
                color: false,
            }
        }
    }

    /// COLR 彩色字形(M2b/T3):Factory4::TranslateColorGlyphRun 拿层
    /// 枚举器(每层自带 glyphRun 子集 + runColor——调色板已被上游解析),
    /// 逐层以灰度同款 analysis 光栅出 coverage mask,再 over 合成进
    /// union 画布(非预乘存储,shader 侧 fg 白直通原色)。
    /// 非彩色字形:TranslateColorGlyphRun 报错 → None → 回灰度。
    fn rasterize_color(&mut self, face: &IDWriteFontFace, glyph: u16) -> Option<GlyphInfo> {
        let factory4 = self.factory.cast::<IDWriteFactory4>().ok()?;
        let glyphs = [glyph];
        let advances = [self.em_size_dip];
        let offsets = [DWRITE_GLYPH_OFFSET {
            advanceOffset: 0.0,
            ascenderOffset: 0.0,
        }];
        let mut run = DWRITE_GLYPH_RUN {
            fontFace: ManuallyDrop::new(Some(face.clone())),
            fontEmSize: self.em_size_dip,
            glyphCount: 1,
            glyphIndices: glyphs.as_ptr(),
            glyphAdvances: advances.as_ptr(),
            glyphOffsets: offsets.as_ptr(),
            isSideways: BOOL::default(),
            bidiLevel: 0,
        };
        let enumerator = unsafe {
            factory4.TranslateColorGlyphRun(
                Vector2 { X: 0.0, Y: 0.0 },
                &run,
                None,
                DWRITE_GLYPH_IMAGE_FORMATS(DWRITE_GLYPH_IMAGE_FORMATS_COLR.0),
                DWRITE_MEASURING_MODE_NATURAL,
                None,
                0,
            )
        };
        drop(unsafe { ManuallyDrop::take(&mut run.fontFace) });
        let enumerator = enumerator.ok()?;

        // 逐层:coverage mask + 层色(层指针只在 MoveNext 前有效,即取即拷)
        let mut layers: Vec<(windows::Win32::Foundation::RECT, Vec<u8>, [f32; 4])> = Vec::new();
        loop {
            let more = unsafe { enumerator.MoveNext().ok()? };
            if !more.as_bool() {
                break;
            }
            let run1_ptr = unsafe { enumerator.GetCurrentRun().ok()? };
            // DWRITE_GLYPH_RUN 非 Copy(ManuallyDrop 字段):按引用借,不移出;
            // runColor 是 4×f32 的 Copy 结构
            let color = unsafe { (*run1_ptr).Base.runColor };
            // SAFETY: 层 run 的指针参数在此块内有效(枚举器缓冲未动)
            let analysis = unsafe {
                self.factory.CreateGlyphRunAnalysis(
                    &(*run1_ptr).Base.glyphRun,
                    1.0,
                    None,
                    DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
                    DWRITE_MEASURING_MODE_NATURAL,
                    0.0,
                    0.0,
                )
            }
            .ok()?;
            let bounds = unsafe {
                analysis
                    .GetAlphaTextureBounds(DWRITE_TEXTURE_CLEARTYPE_3x1)
                    .ok()?
            };
            let (bw, bh) = (
                (bounds.right - bounds.left).max(0),
                (bounds.bottom - bounds.top).max(0),
            );
            if bw == 0 || bh == 0 {
                continue; // 空层跳过
            }
            let mut raw = vec![0u8; (bw * bh) as usize * 3];
            unsafe {
                analysis
                    .CreateAlphaTexture(DWRITE_TEXTURE_CLEARTYPE_3x1, &bounds, &mut raw)
                    .ok()?
            };
            let coverage: Vec<u8> = raw.as_chunks::<3>().0.iter().map(|px| px[0]).collect();
            layers.push((bounds, coverage, [color.r, color.g, color.b, color.a]));
        }
        if layers.is_empty() {
            return None;
        }

        // union 画布
        let mut u = layers[0].0;
        for (b, _, _) in &layers[1..] {
            u.left = u.left.min(b.left);
            u.top = u.top.min(b.top);
            u.right = u.right.max(b.right);
            u.bottom = u.bottom.max(b.bottom);
        }
        let (uw, uh) = ((u.right - u.left).max(1), (u.bottom - u.top).max(1));
        let mut canvas = vec![0f32; (uw * uh) as usize * 4]; // 非预乘 RGBA f32
        for (b, coverage, rgba) in layers {
            let color = rgba.map(|c| c.clamp(0.0, 1.0));
            for y in 0..(b.bottom - b.top).max(0) {
                for x in 0..(b.right - b.left).max(0) {
                    let cov = coverage[(y * (b.right - b.left) + x) as usize] as f32 / 255.0;
                    if cov <= 0.0 {
                        continue;
                    }
                    let sa = color[3] * cov; // 层有效 alpha
                    let cx = (b.left - u.left + x) as usize;
                    let cy = (b.top - u.top + y) as usize;
                    let i = (cy * uw as usize + cx) * 4;
                    let da = canvas[i + 3];
                    canvas[i] = color[0] * sa + canvas[i] * (1.0 - sa);
                    canvas[i + 1] = color[1] * sa + canvas[i + 1] * (1.0 - sa);
                    canvas[i + 2] = color[2] * sa + canvas[i + 2] * (1.0 - sa);
                    canvas[i + 3] = sa + da * (1.0 - sa);
                }
            }
        }
        let mut pixels = vec![0u8; canvas.len()];
        let (packed, _) = pixels.as_chunks_mut::<4>();
        let (floats, _) = canvas.as_chunks::<4>();
        for (dst, src) in packed.iter_mut().zip(floats.iter()) {
            for (d, v) in dst.iter_mut().zip(src.iter()) {
                *d = (v * 255.0).round() as u8;
            }
        }
        let bmp = GlyphBitmap {
            width: uw as u32,
            height: uh as u32,
            pixels,
            format: GlyphFormat::ColorRgba,
        };
        let rect = self.atlas.insert(&bmp);
        let (aw, ah) = (self.atlas.width() as f32, self.atlas.height() as f32);
        Some(GlyphInfo {
            uv: [
                rect.u as f32 / aw,
                rect.v as f32 / ah,
                rect.w as f32 / aw,
                rect.h as f32 / ah,
            ],
            size_px: [uw as f32, uh as f32],
            offset_px: [u.left as f32, self.metrics.ascent + u.top as f32],
            color: true,
        })
    }
}

impl GlyphRouter for DwriteRouter {
    /// 整段 shaping(连字,D16):cluster 布局逐格返回——单字符 cluster
    /// 给 None(渲染侧走 route 缓存,零额外成本),连字 cluster 给合成
    /// GlyphInfo(光栅化单 glyph 位图,宽 = cluster 总 advance)。
    fn route_run(&mut self, chars: &[char], style: GlyphStyle) -> Option<Vec<ClusterLayout>> {
        if !self.ligatures || chars.len() < 2 {
            return None;
        }
        let key = (chars.to_vec(), style_bits(style));
        if let Some(hit) = self.run_cache.get(&key) {
            return Some(hit.clone());
        }
        // 段首字符选面(与 route 同语义:样式面缺字形回退 plain)
        let font = {
            let family = self.family_for(*chars.first()?)?;
            match (style.bold, style.italic) {
                (true, _) => family.bold.clone(),
                (false, true) => family.italic.clone(),
                _ => family.plain.clone(),
            }
        };
        let shape = shape_run(&self.factory, &font, self.em_size_dip, chars)??;

        // cluster 遍历:cluster_map 的值跳变处 = 新 cluster;cluster 的
        // u16 起止换回 char 位数(chars 的 utf16 前缀长对照)
        let mut u16_bounds = Vec::with_capacity(chars.len() + 1);
        u16_bounds.push(0usize);
        for &c in chars {
            let last = *u16_bounds.last().unwrap();
            u16_bounds.push(last + c.len_utf16());
        }
        // u16 位 → char 位 反查表
        let mut char_at = vec![0usize; *u16_bounds.last().unwrap() + 1];
        for (ci, &b) in u16_bounds.iter().enumerate() {
            char_at[b] = ci;
        }

        let mut layouts: Vec<ClusterLayout> = Vec::new();
        let mut u = 0usize; // 当前 u16 位
        let cm = &shape.cluster_map;
        let text_len = cm.len();
        while u < text_len {
            let start_glyph = cm[u] as usize;
            // cluster 吞到 map 值跳变处(或结尾)
            let mut v = u;
            while v < text_len && cm[v] as usize == start_glyph {
                v += 1;
            }
            // cluster 的 glyph 集:map 值 == start_glyph 的区间对应的 glyph
            // 起始位是 start_glyph,终止位 = 下一 cluster 的起始 map 值
            let next_glyph = if v < text_len {
                cm[v] as usize
            } else {
                shape.glyph_indices.len()
            };
            let cluster_chars = char_at[v] - char_at[u];
            if v - u >= 2 && cluster_chars >= 2 && next_glyph > start_glyph {
                // 连字 cluster(多字符并成 ≥1 glyph):光栅化合成字形,
                // 宽 = 该 cluster 所有 glyph advance 之和
                let advance: f32 = shape.advances[start_glyph..next_glyph].iter().sum();
                let glyph_id = shape.glyph_indices[start_glyph];
                let info = self.rasterize_ligature(&font, glyph_id, advance);
                layouts.push(ClusterLayout {
                    len: cluster_chars,
                    glyph: Some(info),
                });
            } else {
                // 常规:逐字符 cluster(BMP 一 u16 一 char)
                for _ in 0..cluster_chars {
                    layouts.push(ClusterLayout {
                        len: 1,
                        glyph: None,
                    });
                }
            }
            u = v;
        }
        self.run_cache.insert(key, layouts.clone());
        Some(layouts)
    }

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

/// 启动连字探测:任一序列 shaping 后 glyph 数 < 字符数 ⇒ 字体支持连字。
fn probe_ligatures(factory: &IDWriteFactory, font: &IDWriteFont, em: f32) -> bool {
    for seq in ["=>", ">=", "!=", "->", "=="] {
        let chars: Vec<char> = seq.chars().collect();
        if let Some(Some(shape)) = shape_run(factory, font, em, &chars)
            && shape.glyph_indices.len() < chars.len()
        {
            return true;
        }
    }
    false
}

/// shaping 结果:cluster map(每 u16 字符位 → glyph 起始索引)、
/// glyph 索引与 advance。
struct ShapeResult {
    cluster_map: Vec<u16>,
    glyph_indices: Vec<u16>,
    advances: Vec<f32>,
}

/// shaping 核心。script analysis 用零值(拉丁近似):等宽编程连字全是
/// ASCII,探针通过即视为可用;真出现错误只影响该段连字(回退逐字)。
fn shape_run(
    factory: &IDWriteFactory,
    font: &IDWriteFont,
    em: f32,
    chars: &[char],
) -> Option<Option<ShapeResult>> {
    // SAFETY 门面:内部全 unsafe COM,失败归 None(外层 None = COM 失败)
    fn inner(
        factory: &IDWriteFactory,
        font: &IDWriteFont,
        em: f32,
        chars: &[char],
    ) -> Option<ShapeResult> {
        unsafe {
            let face = font.CreateFontFace().ok()?;
            let analyzer = factory.CreateTextAnalyzer().ok()?;
            let mut text = Vec::with_capacity(chars.len());
            for &c in chars {
                let mut buf = [0u16; 2];
                text.extend_from_slice(c.encode_utf16(&mut buf));
            }
            let script = DWRITE_SCRIPT_ANALYSIS {
                script: 0,
                shapes: DWRITE_SCRIPT_SHAPES(0),
            };
            let mut cluster_map = vec![0u16; text.len()];
            let mut text_props = vec![DWRITE_SHAPING_TEXT_PROPERTIES::default(); text.len()];
            let mut glyph_indices = vec![0u16; text.len()];
            let mut glyph_props = vec![DWRITE_SHAPING_GLYPH_PROPERTIES::default(); text.len()];
            let mut actual: u32 = 0;
            // 显式开 liga+calt:编程连字(=> 等)是上下文替换,DWrite 的
            // 默认特性集不启用它——这是探测成败的关键参数
            let mut features = [
                DWRITE_FONT_FEATURE {
                    nameTag: DWRITE_FONT_FEATURE_TAG_STANDARD_LIGATURES,
                    parameter: 1,
                },
                DWRITE_FONT_FEATURE {
                    nameTag: DWRITE_FONT_FEATURE_TAG_CONTEXTUAL_ALTERNATES,
                    parameter: 1,
                },
            ];
            let typo = [DWRITE_TYPOGRAPHIC_FEATURES {
                features: features.as_mut_ptr(),
                featureCount: features.len() as u32,
            }];
            let range_lens = [text.len() as u32];
            let typo_ptr: *const DWRITE_TYPOGRAPHIC_FEATURES = &typo[0];
            analyzer
                .GetGlyphs(
                    windows::core::PCWSTR(text.as_ptr()),
                    text.len() as u32,
                    &face,
                    false,
                    false,
                    &script,
                    None,
                    None,
                    Some(&typo_ptr),
                    Some(range_lens.as_ptr()),
                    1,
                    text.len() as u32,
                    cluster_map.as_mut_ptr(),
                    text_props.as_mut_ptr(),
                    glyph_indices.as_mut_ptr(),
                    glyph_props.as_mut_ptr(),
                    &mut actual,
                )
                .ok()?;
            glyph_indices.truncate(actual as usize);
            let mut advances = vec![0f32; actual as usize];
            let mut offsets = vec![DWRITE_GLYPH_OFFSET::default(); actual as usize];
            analyzer
                .GetGlyphPlacements(
                    windows::core::PCWSTR(text.as_ptr()),
                    cluster_map.as_ptr(),
                    text_props.as_mut_ptr(),
                    text.len() as u32,
                    glyph_indices.as_ptr(),
                    glyph_props.as_ptr(),
                    actual,
                    &face,
                    em,
                    false,
                    false,
                    &script,
                    None,
                    Some(&typo_ptr),
                    Some(range_lens.as_ptr()),
                    1,
                    advances.as_mut_ptr(),
                    offsets.as_mut_ptr(),
                )
                .ok()?;
            Some(ShapeResult {
                cluster_map,
                glyph_indices,
                advances,
            })
        }
    }
    inner(factory, font, em, chars).map(Some)
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

    /// T7:bold+italic 三态互异——(true,_) 吞 italic 的回归锁。
    /// 注意主链 Cascadia Mono 无斜体轴?有( italic 由系统合成或家族自带),
    /// 断言按位图差异;Cascadia 系四态位图实际互异。
    #[test]
    fn bold_italic_face_distinct_from_single_styles() {
        let mut r = DwriteRouter::new(12.0, crate::font::DEFAULT_FAMILIES).expect("router");
        let bi = r.route(
            'B',
            GlyphStyle {
                bold: true,
                italic: true,
            },
        );
        let bold = r.route(
            'B',
            GlyphStyle {
                bold: true,
                italic: false,
            },
        );
        let italic = r.route(
            'B',
            GlyphStyle {
                bold: false,
                italic: true,
            },
        );
        let differs = |a: crate::font::router::GlyphInfo, b: crate::font::router::GlyphInfo| {
            a.uv != b.uv || a.size_px != b.size_px
        };
        assert!(
            differs(bi, bold),
            "bold_italic 与 bold 位图相同:专面未生效(加载或级联失效)"
        );
        assert!(
            differs(bi, italic),
            "bold_italic 与 italic 位图相同:粗体维度丢失"
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
