//! 调色板单一来源(spec §4):16 色 + 前景/背景/光标。
//!
//! M1 前三处定义(mica-core `default_rgb` / mica-render `BASE16` /
//! `frame::DEFAULT_BG`)在此收敛;渲染与 OSC 4/10/11/12 应答同源,
//! 换主题后查询应答与画面不会再各说各话。

use alacritty_terminal::vte::ansi::Rgb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// 索引 0-15,与 xterm 色序对齐(black/red/…/bright white)。
    pub colors: [Rgb; 16],
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    /// 选区配色(spec §5):主题库 630 个文件全带 selection-* 键,
    /// M1 忽略、M2a 接上。
    pub selection_fg: Rgb,
    pub selection_bg: Rgb,
}

impl Default for Palette {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Palette {
    /// M0 时代的默认值(BASE16 + 白前景 + #1e1e1e 底 + 白光标)——
    /// 退到无主题无用户配置时,观感与 M1-A 完全一致。
    pub const DEFAULT: Self = Self {
        colors: [
            Rgb {
                r: 0x1e,
                g: 0x1e,
                b: 0x1e,
            },
            Rgb {
                r: 0xf7,
                g: 0x4b,
                b: 0x50,
            },
            Rgb {
                r: 0x24,
                g: 0xb0,
                b: 0x7a,
            },
            Rgb {
                r: 0xf2,
                g: 0xa6,
                b: 0x0d,
            },
            Rgb {
                r: 0x43,
                g: 0x9e,
                b: 0xff,
            },
            Rgb {
                r: 0xd3,
                g: 0x82,
                b: 0xea,
            },
            Rgb {
                r: 0x4f,
                g: 0xc5,
                b: 0xe1,
            },
            Rgb {
                r: 0xd4,
                g: 0xd4,
                b: 0xd4,
            },
            Rgb {
                r: 0x66,
                g: 0x66,
                b: 0x66,
            },
            Rgb {
                r: 0xff,
                g: 0x6c,
                b: 0x70,
            },
            Rgb {
                r: 0x3c,
                g: 0xd6,
                b: 0x96,
            },
            Rgb {
                r: 0xff,
                g: 0xc6,
                b: 0x4d,
            },
            Rgb {
                r: 0x63,
                g: 0xb3,
                b: 0xff,
            },
            Rgb {
                r: 0xd8,
                g: 0x98,
                b: 0xf2,
            },
            Rgb {
                r: 0x6c,
                g: 0xd9,
                b: 0xfa,
            },
            Rgb {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
        ],
        fg: Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        },
        bg: Rgb {
            r: 0x1e,
            g: 0x1e,
            b: 0x1e,
        },
        cursor: Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        },
        selection_fg: Rgb {
            r: 0x1e,
            g: 0x1e,
            b: 0x1e,
        },
        selection_bg: Rgb {
            r: 0xd4,
            g: 0xd4,
            b: 0xd4,
        },
    };

    /// 应用一个主题/用户键值对。M1 键集:`palette = <0-15>=#rrggbb`、
    /// `foreground` / `background` / `cursor-color`。非法值返回 Err
    /// (合成层收集为 ParseError,保旧继续)。
    pub fn apply_pair(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "palette" => {
                let Some((idx, hex)) = value.split_once('=') else {
                    return Err(format!("palette 值应为 <0-15>=#rrggbb,得 `{value}`"));
                };
                let idx: usize = idx
                    .parse()
                    .map_err(|_| format!("palette 索引应为 0-15,得 `{idx}`"))?;
                if idx > 15 {
                    return Err(format!("palette 索引应为 0-15,得 {idx}"));
                }
                self.colors[idx] = parse_hex(hex)?;
            }
            "foreground" => self.fg = parse_hex(value)?,
            "background" => self.bg = parse_hex(value)?,
            "cursor-color" => self.cursor = parse_hex(value)?,
            "selection-foreground" => self.selection_fg = parse_hex(value)?,
            "selection-background" => self.selection_bg = parse_hex(value)?,
            _ => {} // 未知键静默跳过(合成层同理)
        }
        Ok(())
    }

    /// OSC 查询应答口径(ColorRequest 索引):16 色槽 + 256/257/258。
    pub fn query(&self, index: usize) -> Rgb {
        match index {
            0..=15 => self.colors[index],
            256 => self.fg,
            257 => self.bg,
            _ => self.cursor,
        }
    }
}

/// `#rrggbb`(大小写不敏感)。M1 只认 6 位十六进制——主题库全量是
/// 该格式(金样本测试锁定);更短/rgb: 形态等真出现再扩。
fn parse_hex(value: &str) -> Result<Rgb, String> {
    let hex = value
        .strip_prefix('#')
        .ok_or_else(|| format!("色值应为 #rrggbb,得 `{value}`"))?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("色值应为 #rrggbb,得 `{value}`"));
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("已验证 hex 位");
    Ok(Rgb {
        r: byte(0),
        g: byte(2),
        b: byte(4),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_m0_observed_colors() {
        let p = Palette::DEFAULT;
        assert_eq!(
            p.bg,
            Rgb {
                r: 0x1e,
                g: 0x1e,
                b: 0x1e
            }
        );
        assert_eq!(
            p.fg,
            Rgb {
                r: 0xff,
                g: 0xff,
                b: 0xff
            }
        );
        assert_eq!(
            p.colors[1],
            Rgb {
                r: 0xf7,
                g: 0x4b,
                b: 0x50
            },
            "red 槽"
        );
    }

    #[test]
    fn palette_pair_fills_slot() {
        let mut p = Palette::DEFAULT;
        p.apply_pair("palette", "0=#ff5555").unwrap();
        assert_eq!(
            p.colors[0],
            Rgb {
                r: 0xff,
                g: 0x55,
                b: 0x55
            }
        );
        p.apply_pair("palette", "15=#AB Cd").unwrap_err();
    }

    #[test]
    fn foreground_background_cursor_pairs() {
        let mut p = Palette::DEFAULT;
        p.apply_pair("foreground", "#123456").unwrap();
        p.apply_pair("background", "#abcdef").unwrap();
        p.apply_pair("cursor-color", "#00ff00").unwrap();
        assert_eq!(
            p.fg,
            Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56
            }
        );
        assert_eq!(
            p.bg,
            Rgb {
                r: 0xab,
                g: 0xcd,
                b: 0xef
            }
        );
        assert_eq!(
            p.cursor,
            Rgb {
                r: 0x00,
                g: 0xff,
                b: 0x00
            }
        );
    }

    #[test]
    fn unknown_key_is_silent_noop() {
        let mut p = Palette::DEFAULT;
        assert!(p.apply_pair("window-padding-y", "10").is_ok());
        assert_eq!(p, Palette::DEFAULT);
    }

    #[test]
    fn bad_values_are_errors() {
        let mut p = Palette::DEFAULT;
        assert!(p.apply_pair("palette", "16=#ffffff").is_err(), "索引越界");
        assert!(p.apply_pair("palette", "1=ffffff").is_err(), "缺 #");
        assert!(p.apply_pair("palette", "1=#fff").is_err(), "3 位短格式");
        assert!(p.apply_pair("palette", "red").is_err(), "非 palette 形态");
        assert!(p.apply_pair("background", "#12").is_err());
        assert_eq!(p, Palette::DEFAULT, "失败的 pair 不得半途写入");
    }

    #[test]
    fn query_covers_color_request_indices() {
        let p = Palette::DEFAULT;
        assert_eq!(p.query(3), p.colors[3]);
        assert_eq!(p.query(256), p.fg);
        assert_eq!(p.query(257), p.bg);
        assert_eq!(p.query(258), p.cursor);
    }
}
