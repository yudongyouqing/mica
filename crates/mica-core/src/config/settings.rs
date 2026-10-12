//! 三层合成(spec §5):内置默认 < 主题片段 < 用户 config。
//!
//! 错误哲学与解析器一致:收集而非中断,任何 parse/value 错误都让
//! `resolve` 返回 Err——调用方保旧配置继续跑,不让一行坏配置炸掉终端。

use alacritty_terminal::vte::ansi::Rgb;

use crate::config::palette::{Palette, parse_hex};
use crate::config::parse::{self, ParseError};
use crate::config::theme;
use crate::keymap::{self, Keymap};

/// 默认字体回退链(D9)。定义在此(core)——render 侧 re-export,
/// 与格子度量一样保持单一来源。
pub const DEFAULT_FAMILIES: &[&str] = &[
    // JetBrains Mono 打头(Ghostty 官方观感;未安装时 DWrite 回退链自然
    // 落到 Cascadia);Sarasa 装 CJK 等宽首选,再 Cascadia/YaHei/Emoji
    "JetBrains Mono",
    "Sarasa Mono SC",
    "Cascadia Mono",
    "Microsoft YaHei",
    // Nerd Font(oh-my-posh/starship 等提示符引擎的图标/Powerline 三角
    // 形 E0B0 族、F000 图标区;ASCII 仍由链头 JBMono 承担,此处只兜
    // 缺字。两个族名都挂:NF 是 v3 短名(本机实测 GDI/DWrite 都叫这个),
    // Nerd Font 是 v2 长名——未装的机器静默跳过
    "JetBrainsMono NF",
    "JetBrainsMono Nerd Font",
    "Segoe UI Emoji",
    // 图标字形(caption 按钮的 Segoe Fluent Icons PUA 区;终端文本
    // 命中 E700-E8FF 时得到图标也是正确语义,Nerd Font 同族行为)
    "Segoe Fluent Icons",
];

pub const DEFAULT_FONT_SIZE_PT: f32 = 12.0;

/// 标签栏(双行条)外观全量可配(2026-10-12)。默认值 = 源码对标定档
/// 的观感;颜色键写 `auto`(或不写)= 从调色板推导。
/// 键集:`strip-font-family`(空=跟终端链)、`strip-font-size`、
/// `strip-title-height`、`strip-tab-height`、`strip-tab-min-width`、
/// `strip-tab-corner-radius`、`strip-wide-tabs`(Ghostty `gtk-wide-tabs`
/// 同义)、`strip-hide-single-tab`、`strip-bar-color`/`-active-tab-color`/
/// `-text-color`/`-dim-color`(auto|#rrggbb)、`strip-bar-wash`/
/// `-unfocused`(0..1,auto 条底的提亮比例)、
/// `strip-animation-duration`/`-focus`(ms,0=关)。
#[derive(Debug, Clone, PartialEq)]
pub struct StripCfg {
    pub font_family: Option<String>,
    pub font_size_pt: f32,
    pub title_h: f32,
    pub tab_h: f32,
    pub tab_min_w: f32,
    pub tab_corner_r: f32,
    pub wide_tabs: bool,
    pub hide_single: bool,
    pub bar: Option<Rgb>,
    pub bar_wash: f32,
    pub bar_wash_unfocused: f32,
    pub active_tab: Option<Rgb>,
    pub text: Option<Rgb>,
    pub dim: Option<Rgb>,
    pub anim_ms: f32,
    pub focus_ms: f32,
}

impl Default for StripCfg {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size_pt: 9.5,
            title_h: 32.0,
            tab_h: 36.0,
            tab_min_w: 88.0,
            tab_corner_r: 8.0,
            wide_tabs: true,
            hide_single: true,
            bar: None,
            bar_wash: 0.18,
            bar_wash_unfocused: 0.30,
            active_tab: None,
            text: None,
            dim: None,
            anim_ms: 120.0,
            focus_ms: 150.0,
        }
    }
}

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
    /// 光标闪烁(D20):默认不闪,`cursor-blink = true` 开 500ms 定时器。
    pub cursor_blink: bool,
    /// Quick Terminal 全局热键(M3b,默认 Win+`;禁用写 `none`)。
    pub quick_terminal_key: Option<String>,
    /// 打字藏鼠标(Ghostty mouse-hide-while-typing,默认 true)。
    pub mouse_hide_while_typing: bool,
    /// 锁定标题(Ghostty `title` 同义):设定时窗口/标签标题恒为此值,
    /// 程序发的 OSC 0/2 标题被忽略;空串 = 解除锁定回落 shell 标题。
    /// 想视觉隐藏标题写单空格(Ghostty 文档同法)。
    pub title: Option<String>,
    /// 标签栏外观(2026-10-12 全量可配)
    pub strip: StripCfg,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_families: DEFAULT_FAMILIES.iter().map(|s| s.to_string()).collect(),
            font_size_pt: DEFAULT_FONT_SIZE_PT,
            title: None,
            mouse_hide_while_typing: true,
            palette: Palette::DEFAULT,
            theme_name: None,
            keymap: Keymap::wt_default(),
            cursor_blink: false,
            quick_terminal_key: Some("win+oem_3".into()),
            strip: StripCfg::default(),
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
    // 选区/光标是否被任何层显式设置(theme 或用户);[0]=selection-fg
    // [1]=selection-bg [2]=cursor——缺省派生(见尾部)不覆盖显式值
    let mut explicit = [false; 3];

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
                apply_pairs(&mut settings, pairs, &mut err, &mut explicit);
            }
            Err(reason) => err.values.push(reason),
        }
    }
    apply_pairs(&mut settings, user_pairs, &mut err, &mut explicit);

    // Ghostty 缺省语义(上游 renderer/generic.zig 实证):selection/cursor
    // 未显式配置时**动态互换/跟前景**——选区底 = 前景、选区字 = 背景、
    // 光标 = 前景。不是常量:用户只改 foreground 时选区光标随之走
    if !explicit[0] {
        settings.palette.selection_fg = settings.palette.bg;
    }
    if !explicit[1] {
        settings.palette.selection_bg = settings.palette.fg;
    }
    if !explicit[2] {
        settings.palette.cursor = settings.palette.fg;
    }

    if err.is_empty() {
        Ok(settings)
    } else {
        Err(err)
    }
}

/// 键集分派:font-family 追加、font-size 限幅、色键(含 selection-*)走
/// Palette;未知键(含 cursor-text——块光标反色绘制已等价)静默跳过
/// ——Ghostty 同款容忍。explicit 记录选区/光标是否显式设置(缺省派生用)。
fn apply_pairs(
    settings: &mut Settings,
    pairs: Vec<(String, String)>,
    err: &mut ConfigError,
    explicit: &mut [bool; 3],
) {
    // 首个显式 font-family 重置默认链(Ghostty 配置语义:显式键覆盖默认值),
    // 之后的声明追加
    let mut family_declared = false;
    for (key, value) in pairs {
        match key.as_str() {
            "font-family" => {
                if !value.is_empty() {
                    if !family_declared {
                        settings.font_families.clear();
                        family_declared = true;
                    }
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
            // 空值 = 解除锁定(Ghostty:置空回落默认/shell 标题)
            "title" => settings.title = Some(value).filter(|v| !v.is_empty()),
            "quick-terminal-key" => {
                if value.eq_ignore_ascii_case("none") {
                    settings.quick_terminal_key = None;
                } else if crate::keymap::parse_trigger(value.trim()).is_some() {
                    settings.quick_terminal_key = Some(value.to_lowercase());
                } else {
                    err.values.push(format!(
                        "quick-terminal-key 应为 <触发器>(如 win+oem_3)或 none,得 `{value}`"
                    ));
                }
            }
            "mouse-hide-while-typing" => match value.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => settings.mouse_hide_while_typing = true,
                "false" | "0" | "no" => settings.mouse_hide_while_typing = false,
                _ => err.values.push(format!(
                    "mouse-hide-while-typing 应为 true/false,得 `{value}`"
                )),
            },
            "cursor-blink" => match value.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => settings.cursor_blink = true,
                "false" | "0" | "no" => settings.cursor_blink = false,
                _ => err
                    .values
                    .push(format!("cursor-blink 应为 true/false,得 `{value}`")),
            },
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
                if key == "cursor-color" {
                    explicit[2] = true;
                }
            }
            // strip 外观(2026-10-12 全量可配);gtk-wide-tabs 是 Ghostty
            // 同义键,两键同槽
            "gtk-wide-tabs" | "strip-wide-tabs" | "strip-hide-single-tab" => {
                let v = match value.to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => true,
                    "false" | "0" | "no" => false,
                    _ => {
                        err.values
                            .push(format!("{key} 应为 true/false,得 `{value}`"));
                        continue;
                    }
                };
                if key == "strip-hide-single-tab" {
                    settings.strip.hide_single = v;
                } else {
                    settings.strip.wide_tabs = v;
                }
            }
            "strip-font-family" => {
                settings.strip.font_family =
                    (!value.trim().is_empty()).then(|| value.trim().to_string());
            }
            "strip-font-size" => match value.parse::<f32>() {
                Ok(n) if n.is_finite() && (4.0..=24.0).contains(&n) => {
                    settings.strip.font_size_pt = n
                }
                _ => err
                    .values
                    .push(format!("strip-font-size 应为 4-24 的数字,得 `{value}`")),
            },
            "strip-title-height"
            | "strip-tab-height"
            | "strip-tab-min-width"
            | "strip-tab-corner-radius" => match value.parse::<f32>() {
                Ok(n) if n.is_finite() && (0.0..=400.0).contains(&n) => match key.as_str() {
                    "strip-title-height" => settings.strip.title_h = n,
                    "strip-tab-height" => settings.strip.tab_h = n,
                    "strip-tab-min-width" => settings.strip.tab_min_w = n,
                    _ => settings.strip.tab_corner_r = n,
                },
                _ => err
                    .values
                    .push(format!("{key} 应为 0-400 的数字,得 `{value}`")),
            },
            "strip-bar-color"
            | "strip-active-tab-color"
            | "strip-text-color"
            | "strip-dim-color" => {
                if value.eq_ignore_ascii_case("auto") || value.trim().is_empty() {
                    let slot = match key.as_str() {
                        "strip-bar-color" => &mut settings.strip.bar,
                        "strip-active-tab-color" => &mut settings.strip.active_tab,
                        "strip-text-color" => &mut settings.strip.text,
                        _ => &mut settings.strip.dim,
                    };
                    *slot = None;
                } else {
                    match parse_hex(&value) {
                        Ok(c) => {
                            let slot = match key.as_str() {
                                "strip-bar-color" => &mut settings.strip.bar,
                                "strip-active-tab-color" => &mut settings.strip.active_tab,
                                "strip-text-color" => &mut settings.strip.text,
                                _ => &mut settings.strip.dim,
                            };
                            *slot = Some(c);
                        }
                        Err(reason) => err.values.push(format!("{key}: {reason}")),
                    }
                }
            }
            "strip-bar-wash" | "strip-bar-wash-unfocused" => match value.parse::<f32>() {
                Ok(n) if n.is_finite() && (0.0..=1.0).contains(&n) => {
                    if key == "strip-bar-wash" {
                        settings.strip.bar_wash = n;
                    } else {
                        settings.strip.bar_wash_unfocused = n;
                    }
                }
                _ => err
                    .values
                    .push(format!("{key} 应为 0-1 的比例,得 `{value}`")),
            },
            "strip-animation-duration" | "strip-focus-animation-duration" => {
                match value.parse::<f32>() {
                    Ok(n) if n.is_finite() && (0.0..=2000.0).contains(&n) => {
                        if key == "strip-animation-duration" {
                            settings.strip.anim_ms = n;
                        } else {
                            settings.strip.focus_ms = n;
                        }
                    }
                    _ => err
                        .values
                        .push(format!("{key} 应为 0-2000 的毫秒数,得 `{value}`")),
                }
            }
            "selection-foreground" | "selection-background" => {
                if let Err(reason) = settings.palette.apply_pair(&key, &value) {
                    err.values.push(format!("{key}: {reason}"));
                }
                let slot = if key == "selection-background" { 1 } else { 0 };
                explicit[slot] = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// strip-* 全量可配(2026-10-12):键解析/范围/auto 语义/gtk-wide-tabs 同义
    #[test]
    fn strip_keys_parse_and_apply() {
        let s = resolve(
            "strip-font-family = Sarasa Mono SC\n\
             strip-font-size = 10\n\
             strip-title-height = 36\n\
             strip-tab-height = 40\n\
             strip-tab-min-width = 120\n\
             strip-tab-corner-radius = 12\n\
             gtk-wide-tabs = false\n\
             strip-hide-single-tab = false\n\
             strip-bar-color = #123456\n\
             strip-active-tab-color = auto\n\
             strip-text-color = #ffffff\n\
             strip-bar-wash = 0.25\n\
             strip-animation-duration = 0\n\
             strip-focus-animation-duration = 300\n",
        )
        .unwrap();
        let c = &s.strip;
        assert_eq!(c.font_family.as_deref(), Some("Sarasa Mono SC"));
        assert_eq!(c.font_size_pt, 10.0);
        assert_eq!(c.title_h, 36.0);
        assert_eq!(c.tab_h, 40.0);
        assert_eq!(c.tab_min_w, 120.0);
        assert_eq!(c.tab_corner_r, 12.0);
        assert!(!c.wide_tabs, "gtk-wide-tabs 同义");
        assert!(!c.hide_single);
        assert_eq!(c.bar.map(|r| r.r), Some(0x12));
        assert_eq!(c.active_tab, None, "auto = 推导");
        assert_eq!(c.text.map(|r| r.b), Some(0xff));
        assert_eq!(c.bar_wash, 0.25);
        assert_eq!(c.anim_ms, 0.0, "动画关");
        assert_eq!(c.focus_ms, 300.0);

        // 坏值收集不中断
        let e = resolve("strip-font-size = 99\nstrip-bar-wash = 7\n").unwrap_err();
        assert!(e.values.len() >= 2, "范围外值都报错");
    }

    #[test]
    fn empty_source_is_all_defaults() {
        assert_eq!(resolve("").unwrap(), Settings::default());
        // JetBrains Mono 打头后链长 5(2026-10 UI 对标)
        assert_eq!(Settings::default().font_families.len(), 8);
    }

    #[test]
    fn theme_layers_under_user_overrides() {
        // Dracula 底色 #282a36;用户 background 覆盖它,palette 仍来自主题
        let s = resolve("theme = Dracula\nbackground = #000000\n").unwrap();
        assert_eq!(s.theme_name.as_deref(), Some("Dracula"));
        assert_eq!(s.palette.bg.r, 0x00, "用户 background 覆盖主题");
        assert_eq!(s.palette.colors[1].r, 0xff, "palette 槽来自主题(#ff5555)");
    }

    /// Ghostty 缺省选区/光标语义(上游 generic.zig):未显式配置时
    /// 选区 = 前景/背景互换、光标 = 前景——动态跟随,非常量
    #[test]
    fn selection_and_cursor_follow_fg_bg_when_unset() {
        use alacritty_terminal::vte::ansi::Rgb;
        let s = resolve("").unwrap();
        assert_eq!(s.palette.selection_bg, s.palette.fg, "选区底 = 前景");
        assert_eq!(s.palette.selection_fg, s.palette.bg, "选区字 = 背景");
        assert_eq!(s.palette.cursor, s.palette.fg, "光标 = 前景");

        // 只改前景/背景:选区与光标跟着走(不是 DEFAULT 常量)
        let s = resolve("foreground = #123456\nbackground = #abcdef\n").unwrap();
        assert_eq!(
            s.palette.selection_bg,
            Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56
            }
        );
        assert_eq!(
            s.palette.selection_fg,
            Rgb {
                r: 0xab,
                g: 0xcd,
                b: 0xef
            }
        );
        assert_eq!(
            s.palette.cursor,
            Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56
            }
        );

        // 显式 selection-*/cursor-color:不被派生覆盖
        let s = resolve(
            "foreground = #123456\nbackground = #abcdef\nselection-background = #111111\ncursor-color = #222222\n",
        )
        .unwrap();
        assert_eq!(
            s.palette.selection_bg,
            Rgb {
                r: 0x11,
                g: 0x11,
                b: 0x11
            }
        );
        assert_eq!(
            s.palette.selection_fg,
            Rgb {
                r: 0xab,
                g: 0xcd,
                b: 0xef
            },
            "未显式的半边仍派生"
        );
        assert_eq!(
            s.palette.cursor,
            Rgb {
                r: 0x22,
                g: 0x22,
                b: 0x22
            }
        );
    }

    #[test]
    fn title_locks_and_empty_unlocks() {
        // Ghostty `title` 同义:设值锁定,置空解除
        let s = resolve("title = 我的终端").unwrap();
        assert_eq!(s.title.as_deref(), Some("我的终端"));
        let s = resolve("title = ").unwrap();
        assert_eq!(s.title, None, "空值=解除锁定");
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
        assert_eq!(
            s.font_families,
            vec!["JetBrains Mono"],
            "首个显式声明替换默认链"
        );

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
