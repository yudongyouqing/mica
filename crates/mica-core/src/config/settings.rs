//! 三层合成(spec §5):内置默认 < 主题片段 < 用户 config。
//!
//! 错误哲学与解析器一致:收集而非中断,任何 parse/value 错误都让
//! `resolve` 返回 Err——调用方保旧配置继续跑,不让一行坏配置炸掉终端。

use crate::config::palette::Palette;
use crate::config::parse::{self, ParseError};
use crate::config::theme;
use crate::keymap::{self, Keymap};

/// 默认字体回退链(D9)。定义在此(core)——render 侧 re-export,
/// 与格子度量一样保持单一来源。
pub const DEFAULT_FAMILIES: &[&str] = &[
    "Sarasa Mono SC",
    "Cascadia Mono",
    "Microsoft YaHei",
    "Segoe UI Emoji",
];

pub const DEFAULT_FONT_SIZE_PT: f32 = 12.0;

/// 合成产物:app 据此建 DwriteRouter 与 Palette,渲染与应答同源。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// 回退链按序;重复 `font-family` 追加(D9 链语义)
    pub font_families: Vec<String>,
    pub font_size_pt: f32,
    pub palette: Palette,
    pub theme_name: Option<String>,
    /// 键位表(D15):默认 WT 兼容,`keybind` 键覆盖。
    pub keymap: Keymap,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_families: DEFAULT_FAMILIES.iter().map(|s| s.to_string()).collect(),
            font_size_pt: DEFAULT_FONT_SIZE_PT,
            palette: Palette::DEFAULT,
            theme_name: None,
            keymap: Keymap::wt_default(),
        }
    }
}

/// resolve 的全部失败:解析行错误 + 值语义错误(坏 hex/字号/主题名)。
/// 一起返回,一次保存看到全部问题。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigError {
    pub parse: Vec<ParseError>,
    pub values: Vec<String>,
}

impl ConfigError {
    pub fn is_empty(&self) -> bool {
        self.parse.is_empty() && self.values.is_empty()
    }
}

/// 合成用户 config(可空串 = 全默认)。任何错误 → Err(调用方保旧)。
pub fn resolve(user_source: &str) -> Result<Settings, ConfigError> {
    let mut err = ConfigError::default();
    let (user_pairs, parse_errors) = parse::parse(user_source);
    err.parse = parse_errors;

    let mut settings = Settings::default();

    // theme 片段在用户片段之前应用:用户键可以覆盖主题同名键
    let theme_name = user_pairs
        .iter()
        .rev()
        .find(|(k, _)| k == "theme")
        .map(|(_, v)| v.clone());
    if let Some(name) = theme_name {
        match theme::find_theme(&name) {
            Ok(source) => {
                let (pairs, theme_errors) = parse::parse(&source);
                err.parse.extend(theme_errors);
                apply_pairs(&mut settings, pairs, &mut err);
            }
            Err(reason) => err.values.push(reason),
        }
    }
    apply_pairs(&mut settings, user_pairs, &mut err);

    if err.is_empty() {
        Ok(settings)
    } else {
        Err(err)
    }
}

/// 键集分派:font-family 追加、font-size 限幅、色键走 Palette;
/// 未知键(含 selection-*/cursor-text,M2 接)静默跳过——Ghostty 同款容忍。
fn apply_pairs(settings: &mut Settings, pairs: Vec<(String, String)>, err: &mut ConfigError) {
    for (key, value) in pairs {
        match key.as_str() {
            "font-family" => {
                if !value.is_empty() {
                    settings.font_families.push(value);
                }
            }
            "font-size" => match value.parse::<f32>() {
                Ok(n) if n.is_finite() && (4.0..=72.0).contains(&n) => settings.font_size_pt = n,
                _ => err
                    .values
                    .push(format!("font-size 应为 4-72 的数字,得 `{value}`")),
            },
            "theme" => settings.theme_name = Some(value),
            "keybind" => {
                // 值形如 `ctrl+shift+t = new_tab`;`clear` 清默认表
                let value = value.trim();
                if value.eq_ignore_ascii_case("clear") {
                    settings.keymap.clear();
                } else if let Some((trigger, action)) = value.split_once('=') {
                    match (keymap::parse_trigger(trigger), keymap::parse_action(action)) {
                        (Some(t), Some(a)) => settings.keymap.bind(t, a),
                        _ => err
                            .values
                            .push(format!("keybind 无法解析触发器或动作名: `{value}`")),
                    }
                } else {
                    err.values.push(format!(
                        "keybind 值应为 `<触发器> = <动作>` 或 clear,得 `{value}`"
                    ));
                }
            }
            "palette" | "foreground" | "background" | "cursor-color" => {
                if let Err(reason) = settings.palette.apply_pair(&key, &value) {
                    err.values.push(format!("{key}: {reason}"));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source_is_all_defaults() {
        assert_eq!(resolve("").unwrap(), Settings::default());
        assert_eq!(Settings::default().font_families.len(), 4);
    }

    #[test]
    fn theme_layers_under_user_overrides() {
        // Dracula 底色 #282a36;用户 background 覆盖它,palette 仍来自主题
        let s = resolve("theme = Dracula\nbackground = #000000\n").unwrap();
        assert_eq!(s.theme_name.as_deref(), Some("Dracula"));
        assert_eq!(s.palette.bg.r, 0x00, "用户 background 覆盖主题");
        assert_eq!(s.palette.colors[1].r, 0xff, "palette 槽来自主题(#ff5555)");
    }

    #[test]
    fn unknown_theme_is_value_error_not_ok() {
        let err = resolve("theme = nope\n").unwrap_err();
        assert!(err.values.iter().any(|v| v.contains("nope")));
        // 但 Settings 本身合成正常——Err 的存在让调用方保旧,语义够用
    }

    #[test]
    fn font_family_appends_and_size_bounds() {
        let s = resolve("font-family = JetBrains Mono\nfont-size = 16\n").unwrap();
        assert_eq!(s.font_families.len(), 5, "追加在默认链之后");
        assert_eq!(s.font_families[4], "JetBrains Mono");
        assert_eq!(s.font_size_pt, 16.0);

        for bad in ["0", "144", "abc", "-3"] {
            let err = resolve(&format!("font-size = {bad}\n")).unwrap_err();
            assert!(!err.values.is_empty(), "font-size = {bad} 应报值错误");
        }
    }

    #[test]
    fn user_pairs_apply_in_order_last_wins() {
        let s = resolve("background = #111111\nbackground = #222222\n").unwrap();
        assert_eq!(
            (s.palette.bg.r, s.palette.bg.g, s.palette.bg.b),
            (0x22, 0x22, 0x22)
        );
    }

    #[test]
    fn parse_errors_are_reported_with_values() {
        let err = resolve("broken line\nfont-size = abc\n").unwrap_err();
        assert_eq!(err.parse.len(), 1, "行错误带行号");
        assert_eq!(err.values.len(), 1, "值错误同报");
    }

    #[test]
    fn last_theme_wins_for_repeated_key() {
        let s = resolve("theme = Dracula\ntheme = 3024 Day\n").unwrap();
        // 3024 Day 的 palette 0 = #090300 ≠ Dracula #21222c
        assert_eq!(s.palette.colors[0].b, 0x00, "后出现的 theme 生效");
    }
}
