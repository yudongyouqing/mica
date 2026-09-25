//! 主题库:编译期嵌入 `themes/`(D10),按名查找带容错。
//!
//! 630 个主题来自 iterm2-color-schemes 的 ghostty 导出(见 themes/README.md);
//! 解析交给 parse 模块,本模块只管"名字 → 文件"。

use include_dir::{Dir, DirEntry, include_dir};

pub static THEME_LIB: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../themes");

/// 库里的非主题文件(LICENSE/README),查找时排除。
fn is_meta(name: &str) -> bool {
    name.eq_ignore_ascii_case("LICENSE") || name.eq_ignore_ascii_case("README.md")
}

/// 名字规范化:小写、空格/下划线 → 连字符、剥 .cfg 扩展
/// (库内文件是显示名如 `3024 Day`,配置里写 `3024_day`/`3024-day` 都命中)。
fn normalize(name: &str) -> String {
    let lowered = name.trim().to_lowercase();
    let trimmed = lowered.strip_suffix(".cfg").unwrap_or(&lowered);
    trimmed
        .chars()
        .map(|c| match c {
            ' ' | '_' => '-',
            c => c,
        })
        .collect()
}

/// 按名找主题。miss 时给出相近候选(子串匹配),用户排错不用翻目录。
/// 返回主题文本(owned):resolve 是低频路径(启动/热重载),拷一份
/// ~1KB 换取零生命周期传播,调用方拿到的就是普通值。
pub fn find_theme(name: &str) -> Result<String, String> {
    let want = normalize(name);
    let mut total = 0usize;
    let mut close = Vec::new();
    for entry in THEME_LIB.entries() {
        let DirEntry::File(f) = entry else {
            continue; // 库是平目录,子目录留观
        };
        let file_name = f
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let Some(file_name) = file_name else {
            continue;
        };
        if is_meta(&file_name) {
            continue;
        }
        total += 1;
        let normalized = normalize(&file_name);
        if normalized == want {
            return Ok(f.contents_utf8().unwrap_or("").to_owned());
        }
        if !want.is_empty() && (normalized.contains(&want) || want.contains(&normalized)) {
            close.push(file_name);
        }
    }
    let hint = if close.is_empty() {
        format!("库内共 {total} 个主题")
    } else {
        close.truncate(5);
        format!("相近主题:{}", close.join("、"))
    };
    Err(format!("主题 `{name}` 不存在;{hint}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;

    #[test]
    fn finds_exact_and_fuzzy_names() {
        assert!(find_theme("Dracula").is_ok(), "精确名");
        assert!(find_theme("dracula").is_ok(), "大小写不敏感");
        assert!(
            find_theme("3024_Day").is_ok(),
            "下划线归一(库内是 `3024 Day`)"
        );
        assert!(find_theme("TokyoNight Storm").is_ok(), "连空格归一(库内名)");
        assert!(
            find_theme("tokyonight_storm").is_ok(),
            "大小写+下划线全归一"
        );
    }

    #[test]
    fn miss_reports_close_candidates() {
        let err = find_theme("dracul").unwrap_err();
        assert!(err.contains("Dracula"), "提示相近主题: {err}");
    }

    #[test]
    fn meta_files_are_not_themes() {
        assert!(find_theme("LICENSE").is_err());
        assert!(find_theme("README.md").is_err());
    }

    /// 金样本:全库零解析错误。新增主题文件若带解析问题,这里先红。
    #[test]
    fn entire_library_parses_clean() {
        let mut count = 0usize;
        for entry in THEME_LIB.entries() {
            let DirEntry::File(f) = entry else {
                continue;
            };
            let name = f.path().file_name().unwrap().to_string_lossy().into_owned();
            if is_meta(&name) {
                continue;
            }
            let text = f
                .contents_utf8()
                .unwrap_or_else(|| panic!("{name}: 主题文件必须是 UTF-8"));
            let (_, errors) = parse::parse(text);
            assert!(errors.is_empty(), "{name}: {errors:?}");
            count += 1;
        }
        assert_eq!(
            count, 630,
            "主题库规模变化:同步 themes/README.md 与此金样本"
        );
    }
}
