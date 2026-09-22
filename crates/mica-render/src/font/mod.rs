//! Font pipeline: style/width policy (pure), metrics math, atlas, routing.
//! The DirectWrite rasterizer lives in `dwrite` behind #[cfg(windows)].

pub mod atlas;
pub mod metrics;

use alacritty_terminal::term::cell::Flags;
use unicode_width::UnicodeWidthChar;

/// 一个字形的呈现变体(来自 cell flags 的样式位)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct GlyphStyle {
    pub bold: bool,
    pub italic: bool,
}

impl GlyphStyle {
    pub const PLAIN: Self = Self {
        bold: false,
        italic: false,
    };

    pub fn from_flags(flags: Flags) -> Self {
        Self {
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
        }
    }
}

/// 字符占的格子数(East Asian Width:宽/全角 = 2)。
/// 零宽字符(组合符/ZWJ)按 1 兜底,保证实例总是可绘制的。
pub fn char_width(ch: char) -> usize {
    ch.width().filter(|w| *w > 0).unwrap_or(1)
}

/// 默认字体回退链(D9):更纱黑体严格等宽中英混排,未安装时落系统链。
pub const DEFAULT_FAMILIES: &[&str] = &[
    "Sarasa Mono SC",
    "Cascadia Mono",
    "Microsoft YaHei",
    "Segoe UI Emoji",
];

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::cell::Flags;

    #[test]
    fn plain_and_flag_mapping() {
        assert_eq!(
            GlyphStyle::PLAIN,
            GlyphStyle {
                bold: false,
                italic: false
            }
        );
        assert_eq!(
            GlyphStyle::from_flags(Flags::BOLD),
            GlyphStyle {
                bold: true,
                italic: false
            }
        );
        assert_eq!(
            GlyphStyle::from_flags(Flags::ITALIC),
            GlyphStyle {
                bold: false,
                italic: true
            }
        );
        assert_eq!(
            GlyphStyle::from_flags(Flags::BOLD_ITALIC),
            GlyphStyle {
                bold: true,
                italic: true
            }
        );
        // 无关位(如 INVERSE)不影响样式
        assert_eq!(GlyphStyle::from_flags(Flags::INVERSE), GlyphStyle::PLAIN);
    }

    #[test]
    fn ascii_is_one_cell_wide() {
        for ch in "aZ0 ~!@#".chars() {
            assert_eq!(char_width(ch), 1, "{ch:?}");
        }
    }

    #[test]
    fn cjk_ideographs_and_fullwidth_are_two_cells() {
        for ch in ['中', '文', '字', 'あ', 'ア', '한', '。', '\u{FF0C}'] {
            assert_eq!(char_width(ch), 2, "{ch:?}");
        }
    }

    #[test]
    fn emoji_width_follows_unicode_width_table() {
        // U+1F600 在 unicode-width 0.2 的 East Asian Width 判定为宽(2)
        assert_eq!(char_width('\u{1F600}'), 2);
        // 组合用区/零宽字符为 0,按 1 格兜底(不产生 0 宽实例)
        assert_eq!(char_width('\u{200D}'), 1);
    }

    #[test]
    fn default_families_lead_with_sarasa() {
        assert_eq!(DEFAULT_FAMILIES[0], "Sarasa Mono SC");
        assert!(DEFAULT_FAMILIES.contains(&"Segoe UI Emoji"));
    }
}
