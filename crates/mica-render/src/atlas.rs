//! M0 glyph atlas: the 8x8 bitmap font from `font8x8`, each row stretched 2x
//! to fill the 8x16 cell. Replaced by the DirectWrite atlas in M1; CJK shows
//! as '?' until then (known M0 limitation, spec Global Constraints).

use font8x8::UnicodeFonts;

pub const CELL_WIDTH: u32 = 8;
pub const CELL_HEIGHT: u32 = 16;
/// ASCII printable range, inclusive.
pub const FIRST_CHAR: u32 = 32;
pub const LAST_CHAR: u32 = 126;
pub const GLYPH_COUNT: usize = (LAST_CHAR - FIRST_CHAR + 1) as usize;

// 编译期锁死:两 crate 的格子度量不得漂移(spec 全局约束)
const _: () = assert!(
    // u32::from 在 const 上下文尚未稳定,这里用宽化 as 转换
    CELL_WIDTH == mica_core::surface::CELL_WIDTH as u32
        && CELL_HEIGHT == mica_core::surface::CELL_HEIGHT as u32,
    "cell metrics drifted between mica-core and mica-render"
);

/// R8 (luminance) atlas: one row of glyphs, each `CELL_WIDTH` wide.
pub struct Atlas {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Atlas {
    pub fn ascii() -> Self {
        let width = GLYPH_COUNT as u32 * CELL_WIDTH;
        let height = CELL_HEIGHT;
        let mut data = vec![0u8; (width * height) as usize];
        for (i, code) in (FIRST_CHAR..=LAST_CHAR).enumerate() {
            let ch = char::from_u32(code).expect("ASCII range is valid chars");
            let bitmap: [u8; 8] = font8x8::BASIC_FONTS.get(ch).unwrap_or([0; 8]);
            // font8x8: 每字节一行(自顶向下),LSB = 该行最左像素
            for (row, byte) in bitmap.iter().enumerate() {
                for px in 0..8u32 {
                    let on = ((byte >> px) & 1) == 1;
                    let value = if on { 255 } else { 0 };
                    let x = i as u32 * CELL_WIDTH + px;
                    // 每行拉伸 2x 占满 16 像素高
                    data[(row as u32 * 2 * width + x) as usize] = value;
                    data[((row as u32 * 2 + 1) * width + x) as usize] = value;
                }
            }
        }
        Self {
            width,
            height,
            data,
        }
    }
}

/// Atlas column index for a char; unknown glyphs (incl. all non-ASCII) render
/// as '?'. Space is index 0 and its bitmap is blank, so empty cells are
/// background-only.
pub fn atlas_glyph_index(ch: char) -> f32 {
    match u32::from(ch) {
        FIRST_CHAR..=LAST_CHAR => (u32::from(ch) - FIRST_CHAR) as f32,
        _ => (u32::from('?') - FIRST_CHAR) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_geometry_matches_cell_metrics() {
        let atlas = Atlas::ascii();
        assert_eq!(atlas.width, GLYPH_COUNT as u32 * CELL_WIDTH);
        assert_eq!(atlas.height, CELL_HEIGHT);
        assert_eq!(atlas.data.len(), (atlas.width * atlas.height) as usize);
    }

    #[test]
    fn glyph_a_pixels_come_from_font8x8() {
        // font8x8 'A' 首行是 0x0C:第 2、3 位亮(LSB = 最左像素)
        let atlas = Atlas::ascii();
        let idx = atlas_glyph_index('A') as usize;
        let x = (idx as u32 * CELL_WIDTH) as usize;
        assert_eq!(atlas.data[x + 2], 255);
        assert_eq!(atlas.data[x + 3], 255);
        assert_eq!(atlas.data[x + 4], 0);
        // 拉伸填充:第 1 行与第 0 行相同
        assert_eq!(atlas.data[atlas.width as usize + x + 2], 255);
    }

    #[test]
    fn unknown_chars_fall_back_to_question_mark() {
        assert_eq!(atlas_glyph_index('中'), atlas_glyph_index('?'));
        assert_eq!(atlas_glyph_index('\u{1F600}'), atlas_glyph_index('?'));
    }

    #[test]
    fn space_is_glyph_zero() {
        assert_eq!(atlas_glyph_index(' '), 0.0);
    }
}
