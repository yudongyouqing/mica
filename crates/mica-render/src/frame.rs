//! Grid -> render instances. Pure CPU; the GPU pass (Task 6) just draws them.

use alacritty_terminal::vte::ansi::Rgb;

use mica_core::surface::Surface;

use crate::font::metrics::FontMetrics;

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

/// One drawable cell. `pos_uv = [x_px, y_px, u, v]`(uv 为图集内左上,0..1),
/// `size_uv = [w_px, h_px, uw, uvh]`(像素尺寸 + uv 尺寸,0..1)。
/// 空白格(空格/spacer)用 `blank_instance`:uv 尺寸 0,shader 墨恒 0。
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CellInstance {
    pub pos_uv: [f32; 4],
    pub size_uv: [f32; 4],
    pub fg: [f32; 4],
    pub bg: [f32; 4],
}

/// Snapshot the visible screen into draw instances. One instance per cell
/// (spaces included, so the background paints); M0 redraws everything.
///
/// T4 中间态:Task 5 按 64B 实例 + GlyphRouter 重建(app.rs 调用点届时同步)。
pub fn build_instances(_surface: &Surface, _metrics: &FontMetrics) -> Vec<CellInstance> {
    todo!("rebuilt in Task 5")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_layout_is_pod_64() {
        assert_eq!(std::mem::size_of::<CellInstance>(), 64);
        assert_eq!(std::mem::offset_of!(CellInstance, pos_uv), 0);
        assert_eq!(std::mem::offset_of!(CellInstance, size_uv), 16);
        assert_eq!(std::mem::offset_of!(CellInstance, fg), 32);
        assert_eq!(std::mem::offset_of!(CellInstance, bg), 48);
    }
}
