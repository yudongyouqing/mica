//! Ghostty 语法解析器:`KEY = VALUE`,整行 `#` 注释。
//!
//! 语义对齐 Ghostty(config 容错哲学):未知键不报错(键集过滤在合成层),
//! 逐行收集错误绝不 panic。行内 `#` 不剥离——值含 `#rrggbb` 色值,
//! 行内注释语义 Ghostty 未承诺,M1 不支持。

/// 一行解析失败(1-based 行号)。收集全部而非首个:用户一次改 N 行,
/// 只报第一个错误会逼出 N 轮保存-提示循环。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub reason: String,
}

/// 解析为有序键值对(保留重复键与出现顺序——`font-family` 可重复追加、
/// `palette = 0=#rrggbb` 的值本身含 `=`)。
pub fn parse(source: &str) -> (Vec<(String, String)>, Vec<ParseError>) {
    let mut pairs = Vec::new();
    let mut errors = Vec::new();
    for (i, raw) in source.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            errors.push(ParseError {
                line: line_no,
                reason: "missing '='".into(),
            });
            continue;
        };
        let key = k.trim();
        if key.is_empty() {
            errors.push(ParseError {
                line: line_no,
                reason: "empty key".into(),
            });
            continue;
        }
        if key.chars().any(char::is_whitespace) {
            errors.push(ParseError {
                line: line_no,
                reason: format!("invalid key `{key}`"),
            });
            continue;
        }
        pairs.push((key.to_string(), v.trim().to_string()));
    }
    (pairs, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(source: &str) -> Vec<(String, String)> {
        parse(source).0
    }

    #[test]
    fn single_pair_with_spaces() {
        assert_eq!(
            keys("theme = Dracula"),
            vec![("theme".into(), "Dracula".into())]
        );
    }

    #[test]
    fn no_spaces_around_equals_is_legal() {
        assert_eq!(keys("font-size=9"), vec![("font-size".into(), "9".into())]);
    }

    #[test]
    fn repeated_keys_keep_order() {
        let got = keys("font-family = Sarasa Mono SC\nfont-family = Cascadia Mono\n");
        assert_eq!(
            got,
            vec![
                ("font-family".into(), "Sarasa Mono SC".into()),
                ("font-family".into(), "Cascadia Mono".into()),
            ]
        );
    }

    #[test]
    fn value_may_contain_equals_and_hash() {
        // palette 值 `0=#rrggbb`:split_once 只切第一个 `=`,`#` 留在值里
        assert_eq!(
            keys("palette = 0=#ff5555"),
            vec![("palette".into(), "0=#ff5555".into())]
        );
        assert_eq!(
            keys("background = #1e1e1e"),
            vec![("background".into(), "#1e1e1e".into())]
        );
    }

    #[test]
    fn full_line_comments_skipped() {
        assert!(keys("# comment\n   # indented comment\n").is_empty());
    }

    #[test]
    fn blank_and_whitespace_lines_skipped() {
        assert!(keys("\n   \n\t\n").is_empty());
    }

    #[test]
    fn crlf_tolerated() {
        assert_eq!(
            keys("theme = Dracula\r\nfont-size = 12\r\n").len(),
            2,
            "\\r 由 trim 吃掉"
        );
    }

    #[test]
    fn unknown_keys_are_pairs_not_errors() {
        let (pairs, errors) = parse("window-padding-y = 10\n");
        assert_eq!(pairs.len(), 1);
        assert!(errors.is_empty(), "键集过滤是合成层的事");
    }

    #[test]
    fn missing_equals_reports_line_number() {
        let (_, errors) = parse("ok = 1\nbroken line\n");
        assert_eq!(
            errors,
            vec![ParseError {
                line: 2,
                reason: "missing '='".into()
            }]
        );
    }

    #[test]
    fn empty_key_reports_line_number() {
        let (_, errors) = parse(" = value\n");
        assert_eq!(
            errors,
            vec![ParseError {
                line: 1,
                reason: "empty key".into()
            }]
        );
    }

    #[test]
    fn key_with_internal_space_is_invalid() {
        let (_, errors) = parse("foo bar = 1\n");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].reason.contains("foo bar"));
    }

    #[test]
    fn all_errors_collected_not_just_first() {
        let (_, errors) = parse("a\n\nb\n");
        assert_eq!(
            errors.iter().map(|e| e.line).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }
}
