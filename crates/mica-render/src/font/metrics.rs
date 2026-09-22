//! 由字体设计度量推格子像素尺寸(纯数学,DWRITE 换算已核实:
//! px = design_units * em_size_dip / designUnitsPerEm)。

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontMetrics {
    pub cell_width: f32,
    pub line_height: f32,
    pub ascent: f32,
    pub descent: f32,
}

impl FontMetrics {
    /// design-unit 度量 → 像素。`max_advance_du` 为主字体最大 advance(等宽字体即单一 advance)。
    pub fn from_dwrite(
        ascent: u16,
        descent: u16,
        line_gap: i16,
        design_units_per_em: u16,
        em_size_dip: f32,
        max_advance_du: u32,
    ) -> Self {
        let dpuem = design_units_per_em.max(1) as f32;
        let px = |du: f32| du * em_size_dip / dpuem;
        let ascent = px(ascent as f32);
        let descent = px(descent as f32);
        let line_height = (ascent + descent + px(line_gap as f32)).max(1.0);
        let cell_width = px(max_advance_du as f32).max(1.0);
        Self {
            cell_width,
            line_height,
            ascent,
            descent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 假想字体:1000 units/em,ascent 800,descent 200,lineGap 0,最大 advance 500
    // em = 16 DIP(12pt)→ 1 du = 0.016 px
    #[test]
    fn converts_design_units_to_pixels() {
        let m = FontMetrics::from_dwrite(800, 200, 0, 1000, 16.0, 500);
        assert!((m.ascent - 12.8).abs() < 1e-4, "{}", m.ascent);
        assert!((m.descent - 3.2).abs() < 1e-4);
        assert!((m.line_height - 16.0).abs() < 1e-4);
        assert!((m.cell_width - 8.0).abs() < 1e-4, "500du * 0.016");
    }

    #[test]
    fn line_height_includes_gap() {
        let m = FontMetrics::from_dwrite(800, 200, 100, 1000, 16.0, 500);
        assert!((m.line_height - 17.6).abs() < 1e-4);
    }

    #[test]
    fn cell_width_never_below_one_pixel() {
        let m = FontMetrics::from_dwrite(10, 5, 0, 65535, 1.0, 1);
        assert!(m.cell_width >= 1.0);
        assert!(m.line_height >= 1.0);
    }
}
