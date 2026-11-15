// Project defaults remain in config/defaults. Generate only the constant
// projections required by the existing renderer/TLS API; runtime Settings
// uses the regular configuration parser on the same file.
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=config/defaults");
    let source = fs::read_to_string("config/defaults").expect("read default config");
    let pairs: Vec<_> = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (key, value) = line.split_once('=').expect("default key = value");
            (key.trim(), value.trim())
        })
        .collect();
    let value = |key| {
        pairs
            .iter()
            .rev()
            .find(|(k, _)| *k == key)
            .unwrap_or_else(|| panic!("missing default {key}"))
            .1
    };
    let color = |hex: &str| {
        let rgb =
            u32::from_str_radix(hex.strip_prefix('#').expect("#rrggbb"), 16).expect("default RGB");
        assert_eq!(hex.len(), 7);
        format!(
            "alacritty_terminal::vte::ansi::Rgb {{ r: {}, g: {}, b: {} }}",
            rgb >> 16,
            (rgb >> 8) & 255,
            rgb & 255
        )
    };
    let fonts = pairs
        .iter()
        .filter(|(k, _)| *k == "font-family")
        .map(|(_, v)| format!("{v:?}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut colors = vec![None; 16];
    for (_, entry) in pairs.iter().filter(|(k, _)| *k == "palette") {
        let (index, rgb) = entry.split_once('=').expect("default palette index=RGB");
        colors[index.parse::<usize>().expect("palette index")] = Some(color(rgb));
    }
    let colors = colors
        .into_iter()
        .map(|c| c.expect("all 16 default colors"))
        .collect::<Vec<_>>()
        .join(",");
    let fg = color(value("foreground"));
    let bg = color(value("background"));
    let derived = |key, fallback: &str| {
        let v = value(key);
        if v.is_empty() || v == "auto" {
            fallback.to_string()
        } else {
            color(v)
        }
    };
    let size: f32 = value("font-size").parse().expect("default font size");
    let contrast: f32 = value("minimum-contrast").parse().expect("default contrast");
    let output = format!(
        "pub const DEFAULT_FAMILIES: &[&str] = &[{fonts}];\n\
         pub const DEFAULT_FONT_SIZE_PT: f32 = {size:?};\n\
         pub const DEFAULT_PALETTE: crate::config::palette::Palette = crate::config::palette::Palette {{\n\
         colors: [{colors}], fg: {fg}, bg: {bg}, cursor: {},\n\
         selection_fg: {}, selection_bg: {}, minimum_contrast: {contrast:?} }};\n",
        derived("cursor-color", &fg),
        derived("selection-foreground", &bg),
        derived("selection-background", &fg),
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("config_defaults.rs"),
        output,
    )
    .expect("write generated default projections");
}
