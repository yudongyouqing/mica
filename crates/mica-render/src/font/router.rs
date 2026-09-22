//! 字形路由:字符+样式 → 图集几何。DirectWrite 实现见 dwrite.rs;
//! 测试用 Fake(见 frame::tests)。

use super::GlyphStyle;

/// 一个已就位的字形:uv 已归一化(0..1),offset 相对格左上。
#[derive(Debug, Clone, Copy)]
pub struct GlyphInfo {
    pub uv: [f32; 4],
    pub size_px: [f32; 2],
    pub offset_px: [f32; 2],
}

pub trait GlyphRouter {
    /// 路由一个字符+样式到图集条目;同 (char, style) 必须命中缓存同值。
    /// 空格与未知字符由实现者决定(通常路由到空白或 .notdef)。
    fn route(&mut self, ch: char, style: GlyphStyle) -> GlyphInfo;
}
