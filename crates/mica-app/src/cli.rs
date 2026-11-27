//! Command parsing is pure: invalid input never connects to or starts an instance.
use mica_core::ipc::{BadgeState, IpcRequest};

#[derive(Debug)]
pub(crate) enum Command {
    Control {
        request: Box<IpcRequest>,
        json: bool,
    },
    Watch {
        pane_id: Option<u64>,
        events: Option<String>,
    },
}

pub(crate) fn parse(args: &[String], id: u64) -> Result<Command, String> {
    let command = args.first().ok_or("缺少命令")?.as_str();
    let launch = matches!(command, "new-tab" | "new-window");
    let targetable = matches!(command, "send" | "focus" | "read" | "badge");
    let op = match command {
        "new-tab" | "new-window" | "list" | "focus" => command,
        "send" => "send-text",
        "read" => "read-text",
        "badge" => "set-badge",
        "watch" => "watch",
        _ => return Err(format!("未知命令: {command}")),
    };
    let mut request = IpcRequest::new(id, op);
    let mut json = false;
    let mut literal = false;
    let mut words = Vec::new();
    let mut events = None;
    let mut target_set = false;
    let mut options = true;
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if options && arg == "--" {
            options = false;
            i += 1;
            continue;
        }
        if options && arg.starts_with('-') {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
            let takes_value = matches!(
                flag,
                "--pane"
                    | "--tab"
                    | "--cwd"
                    | "--command"
                    | "--env"
                    | "--font-size"
                    | "-f"
                    | "--lines"
                    | "--state"
                    | "--text"
                    | "--events"
            );
            let value = if takes_value {
                Some(match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i)
                            .ok_or_else(|| format!("{flag} 缺少参数"))?
                            .as_str()
                    }
                })
            } else {
                if inline.is_some() {
                    return Err(format!("{flag} 不接受参数"));
                }
                None
            };
            match flag {
                "--json" if command != "watch" => json = true,
                "--literal" if command == "send" => literal = true,
                "--pane" if targetable || command == "watch" => {
                    if target_set {
                        return Err("只能指定一个目标".into());
                    }
                    request.pane_id =
                        Some(value.unwrap().parse().map_err(|_| "--pane 应为非负整数")?);
                    target_set = true;
                }
                "--tab" if targetable => {
                    if target_set {
                        return Err("只能指定一个目标".into());
                    }
                    let tab: i64 = value
                        .unwrap()
                        .parse()
                        .map_err(|_| "--tab 应为从 0 起的标签序号")?;
                    if tab < 0 {
                        return Err("--tab 应为从 0 起的标签序号; Quick Terminal 使用 --qt".into());
                    }
                    request.tab = Some(tab);
                    target_set = true;
                }
                "--qt" if targetable => {
                    if target_set {
                        return Err("只能指定一个目标".into());
                    }
                    request.tab = Some(-1);
                    target_set = true;
                }
                "--cwd" if launch => request.cwd = Some(value.unwrap().into()),
                "--command" if launch => request.command = Some(value.unwrap().into()),
                "--env" if launch => {
                    let (key, val) = value
                        .unwrap()
                        .split_once('=')
                        .ok_or("--env 应为 KEY=VALUE")?;
                    if key.is_empty() || key.contains('\0') || val.contains('\0') {
                        return Err("--env 的名称不能为空,名称和值不能包含 NUL".into());
                    }
                    if key.eq_ignore_ascii_case("MICA_PANE_ID") {
                        return Err("MICA_PANE_ID 由 Mica 分配,不能覆盖".into());
                    }
                    request
                        .env
                        .get_or_insert_with(Default::default)
                        .insert(key.into(), val.into());
                }
                "--font-size" | "-f" if launch => {
                    let size: f32 = value.unwrap().parse().map_err(|_| "字号应为数字")?;
                    if !size.is_finite() || size <= 0.0 {
                        return Err("字号应为有限正数".into());
                    }
                    request.font_size = Some(size);
                }
                "--lines" if command == "read" => {
                    request.lines =
                        Some(value.unwrap().parse().map_err(|_| "--lines 应为非负整数")?);
                }
                "--scrollback" if command == "read" => request.include_scrollback = Some(true),
                "--state" if command == "badge" => {
                    request.badge_state = Some(match value.unwrap() {
                        "none" => BadgeState::None,
                        "idle" => BadgeState::Idle,
                        "running" => BadgeState::Running,
                        "attention" => BadgeState::Attention,
                        _ => return Err("--state 应为 none|idle|running|attention".into()),
                    })
                }
                "--text" if command == "badge" => request.badge_text = Some(value.unwrap().into()),
                "--sticky" if command == "badge" => request.badge_sticky = Some(true),
                "--events" if command == "watch" => {
                    let filter = value.unwrap();
                    if filter.split(',').any(|v| {
                        !matches!(
                            v,
                            "exit" | "cwd" | "prompt" | "title" | "badge" | "lifecycle"
                        )
                    }) {
                        return Err(
                            "--events 应为 exit,cwd,prompt,title,badge,lifecycle 的逗号分隔子集"
                                .into(),
                        );
                    }
                    events = Some(filter.into());
                }
                _ => return Err(format!("{command}: 不支持参数 {flag}")),
            }
        } else if command == "send" {
            words.push(arg.as_str());
        } else if launch && request.profile.is_none() {
            request.profile = Some(arg.clone());
        } else {
            return Err(format!("{command}: 多余参数 {arg}"));
        }
        i += 1;
    }
    if command == "send" {
        if words.is_empty() {
            return Err("send 缺少文本".into());
        }
        let joined = words.join(" ");
        request.text = Some(if literal {
            joined
        } else {
            String::from_utf8(mica_core::ipc::unescape_send(&joined)).expect("UTF-8 preserved")
        });
    }
    if command == "focus" && !target_set {
        return Err("focus 需要 --pane=ID、--tab=N 或 --qt".into());
    }
    if command == "badge" && request.badge_state.is_none() {
        return Err("badge 需要 --state".into());
    }
    if command == "watch" {
        return Ok(Command::Watch {
            pane_id: request.pane_id,
            events,
        });
    }
    Ok(Command::Control {
        request: Box::new(request),
        json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn launch_options_keep_spaces_equals_and_repeated_env() {
        let Command::Control { request, json } = parse(
            &args(&[
                "new-window",
                "PowerShell",
                "--cwd",
                "C:/a b",
                "--command",
                "pwsh -NoLogo",
                "--env",
                "ONE=1=2",
                "--env",
                "TWO=中文",
                "--font-size",
                "12",
                "--json",
            ]),
            7,
        )
        .unwrap() else {
            panic!("control")
        };
        assert_eq!(request.op, "new-window");
        assert_eq!(request.id, 7);
        assert_eq!(request.profile.as_deref(), Some("PowerShell"));
        assert_eq!(request.cwd.as_deref(), Some("C:/a b"));
        assert_eq!(request.command.as_deref(), Some("pwsh -NoLogo"));
        assert_eq!(request.env.as_ref().unwrap()["ONE"], "1=2");
        assert_eq!(request.env.as_ref().unwrap()["TWO"], "中文");
        assert_eq!(request.font_size, Some(12.0));
        assert!(json);
    }

    #[test]
    fn invalid_options_are_not_treated_as_profiles_or_text() {
        for invalid in [
            vec!["new-tab", "--cwd"],
            vec!["new-tab", "--font-size", "NaN"],
            vec!["send", "--pane=x", "hello"],
            vec!["focus"],
            vec!["focus", "--pane=3", "--tab=1"],
            vec!["read", "--tab=-1"],
            vec!["read", "--lines", "-2"],
            vec!["new-tab", "--env", "=oops"],
            vec!["badge", "--state", "oops"],
            vec!["watch", "--events", "bad"],
            vec!["list", "--cwd", "x"],
            vec!["send", "--typo", "hello"],
        ] {
            assert!(parse(&args(&invalid), 1).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn send_target_and_literal_end_of_options() {
        let Command::Control { request, .. } = parse(
            &args(&[
                "send",
                "--pane",
                "42",
                "--literal",
                "--",
                "--text\\r",
                "中文",
            ]),
            1,
        )
        .unwrap() else {
            panic!("control")
        };
        assert_eq!(request.text.as_deref(), Some("--text\\r 中文"));
        assert_eq!(request.pane_id, Some(42));
    }

    #[test]
    fn read_and_watch_have_explicit_bounded_inputs() {
        let Command::Control { request, json } = parse(
            &args(&[
                "read",
                "--qt",
                "--scrollback",
                "--lines=18446744073709551615",
                "--json",
            ]),
            2,
        )
        .unwrap() else {
            panic!("control")
        };
        assert_eq!(request.op, "read-text");
        assert_eq!(request.tab, Some(-1));
        assert_eq!(request.include_scrollback, Some(true));
        assert_eq!(request.lines, Some(usize::MAX));
        assert!(json);
        let Command::Watch { pane_id, events } = parse(
            &args(&["watch", "--pane=4", "--events", "cwd,prompt,badge"]),
            1,
        )
        .unwrap() else {
            panic!("watch")
        };
        assert_eq!(pane_id, Some(4));
        assert_eq!(events.as_deref(), Some("cwd,prompt,badge"));
    }
}
