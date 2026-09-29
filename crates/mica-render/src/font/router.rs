//! 字形路由:字符+样式 → 图集几何。DirectWrite 实现见 dwrite.rs;
//! 测试用 Fake(见 frame::tests)。

use super::GlyphStyle;

/// 一个已就位的字形:uv 已归一化(0..1),offset 相对格左上。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphInfo {
    pub uv: [f32; 4],
    pub size_px: [f32; 2],
    pub offset_px: [f32; 2],
    /// 彩色字形(COLR,M2b/T3):位图带原色,渲染实例 fg 必须填白让原色
    /// 直通(shader: mix(bg, tex.rgb * fg, tex.a))。灰度字形恒 false。
    pub color: bool,
}

/// shaping 的逐 cluster 布局(见 GlyphRouter::route_run)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterLayout {
    /// 该 cluster 吞掉的字符数
    pub len: usize,
    /// Some = 合成字形(连字);None = 常规字符(len 必为 1)
    pub glyph: Option<GlyphInfo>,
}

pub trait GlyphRouter {
    /// 整段 shaping(M2b/T4 连字):同 style 的连续字符段一次性整形,
    /// 返回逐 cluster 布局(长度恒等于 chars;sum(len) == chars.len())。
    /// `len == 1 且 glyph == None` = 常规字符,调用方走逐字路由(缓存路径);
    /// `len >= 2 且 Some` = 连字 cluster,合成字形由首格承载、宽为 cluster 总宽。
    /// 返回 None(外层)= 不支持 shaping,整段逐字。
    fn route_run(
        &mut self,
        _chars: &[char],
        _style: super::GlyphStyle,
    ) -> Option<Vec<ClusterLayout>> {
        None
    }

    /// 路由一个字符+样式到图集条目;同 (char, style) 必须命中缓存同值。
    /// 空格与未知字符由实现者决定(通常路由到空白或 .notdef)。
    fn route(&mut self, ch: char, style: GlyphStyle) -> GlyphInfo;
}
