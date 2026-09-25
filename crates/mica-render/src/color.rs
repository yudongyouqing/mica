//! Terminal colors -> concrete RGB for the renderer. Palette is the single
//! source (mica-core `config::palette`); nothing is hardcoded here.

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use mica_core::config::palette::Palette;

/// xterm 256-color cube luminance levels.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn rgb3(rgb: Rgb) -> [u8; 3] {
    [rgb.r, rgb.g, rgb.b]
}

/// Resolve any terminal color to RGB through the palette. Default-ink colors
/// become `palette.fg`/`palette.bg`; other named colors (including `Dim*` and
/// `Cursor`) fall through to `palette.colors[15]`; the vte parser does not
/// emit them in practice.
pub fn resolve(color: Color, palette: &Palette) -> [u8; 3] {
    match color {
        Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        Color::Named(NamedColor::Foreground)
        | Color::Named(NamedColor::BrightForeground)
        | Color::Named(NamedColor::DimForeground) => rgb3(palette.fg),
        Color::Named(NamedColor::Background) => rgb3(palette.bg),
        Color::Named(named) => rgb3(palette.colors[(named as usize).min(15)]),
        Color::Indexed(i) => resolve_indexed(i, palette),
    }
}

fn resolve_indexed(i: u8, palette: &Palette) -> [u8; 3] {
    match i {
        0..=15 => rgb3(palette.colors[i as usize]),
        16..=231 => {
            let i = (i - 16) as usize;
            [
                CUBE_LEVELS[i / 36],
                CUBE_LEVELS[(i % 36) / 6],
                CUBE_LEVELS[i % 6],
            ]
        }
        // 232..=255 grayscale ramp: 8 + 10*(i-232)
        _ => {
            let v = (8 + 10 * (u16::from(i) - 232)) as u8;
            [v, v, v]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::Rgb;
    use mica_core::config::palette::Palette;

    #[test]
    fn named_defaults_resolve_to_palette_ink() {
        let p = Palette::DEFAULT;
        assert_eq!(
            resolve(Color::Named(NamedColor::Foreground), &p),
            [0xff, 0xff, 0xff]
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Background), &p),
            [0x1e, 0x1e, 0x1e]
        );
    }

    #[test]
    fn base16_named_colors_map_directly() {
        let p = Palette::DEFAULT;
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &p),
            [0xf7, 0x4b, 0x50]
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::BrightWhite), &p),
            [0xff, 0xff, 0xff]
        );
    }

    #[test]
    fn custom_palette_drives_named_and_indexed_slots() {
        let mut p = Palette::DEFAULT;
        p.apply_pair("palette", "1=#ff5555").unwrap();
        p.apply_pair("palette", "5=#010203").unwrap();
        // Named 走槽位
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &p),
            [0xff, 0x55, 0x55]
        );
        // 同一槽位被 256 色索引等价命中
        assert_eq!(resolve(Color::Indexed(1), &p), [0xff, 0x55, 0x55]);
        assert_eq!(resolve(Color::Indexed(5), &p), [1, 2, 3]);
    }

    #[test]
    fn custom_palette_drives_default_ink() {
        let mut p = Palette::DEFAULT;
        p.apply_pair("foreground", "#112233").unwrap();
        p.apply_pair("background", "#abcdef").unwrap();
        assert_eq!(
            resolve(Color::Named(NamedColor::Foreground), &p),
            [0x11, 0x22, 0x33]
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Background), &p),
            [0xab, 0xcd, 0xef]
        );
    }

    #[test]
    fn indexed_cube_follows_xterm_formula() {
        let p = Palette::DEFAULT;
        assert_eq!(resolve(Color::Indexed(16), &p), [0, 0, 0]);
        assert_eq!(resolve(Color::Indexed(196), &p), [255, 0, 0]);
        assert_eq!(resolve(Color::Indexed(231), &p), [255, 255, 255]);
    }

    #[test]
    fn indexed_grayscale_ramp() {
        let p = Palette::DEFAULT;
        assert_eq!(resolve(Color::Indexed(232), &p), [8, 8, 8]);
        assert_eq!(resolve(Color::Indexed(244), &p), [128, 128, 128]);
        assert_eq!(resolve(Color::Indexed(255), &p), [238, 238, 238]);
    }

    #[test]
    fn direct_specs_pass_through() {
        let p = Palette::DEFAULT;
        assert_eq!(
            resolve(
                Color::Spec(Rgb {
                    r: 12,
                    g: 34,
                    b: 56
                }),
                &p
            ),
            [12, 34, 56]
        );
    }
}
