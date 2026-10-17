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
    /// Ghostty 官方默认(上游 src/terminal/color.zig Name.default + Config 默认:
    /// Tomorrow 系 16 色 + #282C34 底 + 纯白前景 + 白光标;选区 = 前景/
    /// 背景互换,上游 generic.zig "selection background is our foreground"
    /// 实证)——UI 对标 Ghostty 的基准观感;无主题无用户配置时与官方一致。
    /// 未显式配置时的动态互换/跟前景语义在 settings::resolve 收尾派生。
    pub const DEFAULT: Self = Self {
        colors: [
            Rgb {
                r: 0x1d,
                g: 0x1f,
                b: 0x21,
            },
            Rgb {
                r: 0xcc,
                g: 0x66,
                b: 0x66,
            },
            Rgb {
                r: 0xb5,
                g: 0xbd,
                b: 0x68,
            },
            Rgb {
                r: 0xf0,
                g: 0xc6,
                b: 0x74,
            },
            Rgb {
                r: 0x81,
                g: 0xa2,
                b: 0xbe,
            },
            Rgb {
                r: 0xb2,
                g: 0x94,
                b: 0xbb,
            },
            Rgb {
                r: 0x8a,
                g: 0xbe,
                b: 0xb7,
            },
            Rgb {
                r: 0xc5,
                g: 0xc8,
                b: 0xc6,
            },
            Rgb {
                r: 0x66,
                g: 0x66,
                b: 0x66,
            },
            Rgb {
                r: 0xd5,
                g: 0x4e,
                b: 0x53,
            },
            Rgb {
                r: 0xb9,
                g: 0xca,
                b: 0x4a,
            },
            Rgb {
                r: 0xe7,
                g: 0xc5,
                b: 0x47,
            },
            Rgb {
                r: 0x7a,
                g: 0xa6,
                b: 0xda,
            },
            Rgb {
                r: 0xc3,
                g: 0x97,
                b: 0xd8,
            },
            Rgb {
                r: 0x70,
                g: 0xc0,
                b: 0xb1,
            },
            Rgb {
                r: 0xea,
                g: 0xea,
                b: 0xea,
            },
        ],
        fg: Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        },
        bg: Rgb {
            r: 0x28,
            g: 0x2c,
            b: 0x34,
        },
        cursor: Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        },
        selection_fg: Rgb {
            r: 0x28,
            g: 0x2c,
            b: 0x34,
        },
        selection_bg: Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
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

    /// OSC 查询应答口径(ColorRequest 索引):16 色槽 + 256/257/258;
    /// 16-231 = 256 色立方(6³,标准 xterm 公式),232-255 = 灰阶
    /// (8+10n)。M5a/T7:此前 16-255 误答光标色。
    pub fn query(&self, index: usize) -> Rgb {
        match index {
            0..=15 => self.colors[index],
            16..=231 => {
                // 立方坐标 c = index - 16;r=c/36、g=c/6%6、b=c%6;
                // 每级 × 51(0,51,…,255)
                let c = index - 16;
                let lv = |axis: u32| ((c / 6usize.pow(axis)) % 6 * 51) as u8;
                Rgb {
                    r: lv(2),
                    g: lv(1),
                    b: lv(0),
                }
            }
            232..=255 => {
                let v = (8 + 10 * (index - 232)) as u8;
                Rgb { r: v, g: v, b: v }
            }
            256 => self.fg,
            257 => self.bg,
            _ => self.cursor,
        }
    }
}

/// `#rrggbb`(大小写不敏感)。M1 只认 6 位十六进制——主题库全量是
/// 该格式(金样本测试锁定);更短/rgb: 形态等真出现再扩。
/// pub:settings 的 strip-* 颜色键复用同一口径(2026-10-12)。
pub fn parse_hex(value: &str) -> Result<Rgb, String> {
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
    fn default_matches_ghostty_upstream() {
        // 锚 Ghostty 官方默认(上游 src/terminal/color.zig Name.default +
        // Config 默认 bg/fg)——UI 对标的基准值,漂移即回归
        let p = Palette::DEFAULT;
        assert_eq!(
            p.bg,
            Rgb {
                r: 0x28,
                g: 0x2c,
                b: 0x34
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
                r: 0xcc,
                g: 0x66,
                b: 0x66
            },
            "red 槽"
        );
        assert_eq!(
            p.colors[7],
            Rgb {
                r: 0xc5,
                g: 0xc8,
                b: 0xc6
            },
            "white 槽(strip 次级字色)"
        );
        // 选区 = 前景/背景互换(上游 generic.zig null-selection 语义)
        assert_eq!(p.selection_bg, p.fg, "选区底 = 前景");
        assert_eq!(p.selection_fg, p.bg, "选区字 = 背景");
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

    /// 256 色立方与灰阶公式(T7):锚点值锁 xterm 标准口径。
    #[test]
    fn query_cube_and_grayscale_anchors() {
        let p = Palette::DEFAULT;
        // 立方角:16=(0,0,0)黑、17=(0,0,1)蓝 51、21=(0,0,5)蓝 255、
        // 231=(5,5,5)白、196=(5,0,0)红 255
        assert_eq!(p.query(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(p.query(17), Rgb { r: 0, g: 0, b: 51 });
        assert_eq!(p.query(21), Rgb { r: 0, g: 0, b: 255 });
        assert_eq!(
            p.query(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(p.query(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(p.query(46), Rgb { r: 0, g: 255, b: 0 }, "46=(0,5,0)");
        // 灰阶:232=8、255=238
        assert_eq!(p.query(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            p.query(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }
}
