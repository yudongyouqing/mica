//! Terminal colors -> concrete RGB for the renderer.

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// Base 16 palette (dark theme), indices 0-15. Matches the shell's reported
/// defaults so OSC 10/11 answers and rendering agree.
pub const BASE16: [[u8; 3]; 16] = [
    [0x1e, 0x1e, 0x1e], // black
    [0xf7, 0x4b, 0x50], // red
    [0x24, 0xb0, 0x7a], // green
    [0xf2, 0xa6, 0x0d], // yellow
    [0x43, 0x9e, 0xff], // blue
    [0xd3, 0x82, 0xea], // magenta
    [0x4f, 0xc5, 0xe1], // cyan
    [0xd4, 0xd4, 0xd4], // white
    [0x66, 0x66, 0x66], // bright black
    [0xff, 0x6c, 0x70], // bright red
    [0x3c, 0xd6, 0x96], // bright green
    [0xff, 0xc6, 0x4d], // bright yellow
    [0x63, 0xb3, 0xff], // bright blue
    [0xd8, 0x98, 0xf2], // bright magenta
    [0x6c, 0xd9, 0xfa], // bright cyan
    [0xff, 0xff, 0xff], // bright white
];

/// xterm 256-color cube luminance levels.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Resolve any terminal color to RGB. Default-ink colors become the given
/// defaults; `Dim*` base colors render as their plain base in M0.
pub fn resolve(color: Color, default_fg: Rgb, default_bg: Rgb) -> [u8; 3] {
    match color {
        Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        Color::Named(NamedColor::Foreground)
        | Color::Named(NamedColor::BrightForeground)
        | Color::Named(NamedColor::DimForeground) => [default_fg.r, default_fg.g, default_fg.b],
        Color::Named(NamedColor::Background) => [default_bg.r, default_bg.g, default_bg.b],
        Color::Named(named) => BASE16[(named as usize).min(15)],
        Color::Indexed(i) => resolve_indexed(i, default_fg, default_bg),
    }
}

fn resolve_indexed(i: u8, _default_fg: Rgb, _default_bg: Rgb) -> [u8; 3] {
    match i {
        0..=15 => BASE16[i as usize],
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

    const FG: Rgb = Rgb {
        r: 0xff,
        g: 0xff,
        b: 0xff,
    };
    const BG: Rgb = Rgb {
        r: 0x1e,
        g: 0x1e,
        b: 0x1e,
    };

    #[test]
    fn named_defaults_resolve_to_palette_ink() {
        assert_eq!(
            resolve(Color::Named(NamedColor::Foreground), FG, BG),
            [0xff, 0xff, 0xff]
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Background), FG, BG),
            [0x1e, 0x1e, 0x1e]
        );
    }

    #[test]
    fn base16_named_colors_map_directly() {
        assert_eq!(resolve(Color::Named(NamedColor::Red), FG, BG), BASE16[1]);
        assert_eq!(
            resolve(Color::Named(NamedColor::BrightWhite), FG, BG),
            BASE16[15]
        );
    }

    #[test]
    fn indexed_cube_follows_xterm_formula() {
        assert_eq!(resolve(Color::Indexed(16), FG, BG), [0, 0, 0]);
        assert_eq!(resolve(Color::Indexed(196), FG, BG), [255, 0, 0]);
        assert_eq!(resolve(Color::Indexed(231), FG, BG), [255, 255, 255]);
    }

    #[test]
    fn indexed_grayscale_ramp() {
        assert_eq!(resolve(Color::Indexed(232), FG, BG), [8, 8, 8]);
        assert_eq!(resolve(Color::Indexed(244), FG, BG), [128, 128, 128]);
        assert_eq!(resolve(Color::Indexed(255), FG, BG), [238, 238, 238]);
    }

    #[test]
    fn direct_specs_pass_through() {
        assert_eq!(
            resolve(
                Color::Spec(Rgb {
                    r: 12,
                    g: 34,
                    b: 56
                }),
                FG,
                BG
            ),
            [12, 34, 56]
        );
    }
}
