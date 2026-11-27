use mica_core::ipc::{
    BadgeState, IpcRequest, IpcResponse, MAX_FRAME_BYTES, PaneBadge, PromptState,
    READ_TEXT_MAX_BYTES, ReadTextResult, encode_frame,
};
use mica_core::surface::{ScreenSize, ScrollCommand, Surface};

#[test]
fn wire_schema_remains_backward_compatible_and_supports_control_fields() {
    let old: IpcRequest = serde_json::from_str(r#"{"id":1,"op":"new-tab"}"#).unwrap();
    assert_eq!(old, IpcRequest::new(1, "new-tab"));
    let old_response: IpcResponse = serde_json::from_str(r#"{"id":1,"status":"applied"}"#).unwrap();
    assert!(old_response.data.is_none());
    let request: IpcRequest = serde_json::from_value(serde_json::json!({
        "id":2,"op":"set-badge","cwd":"C:/工作","command":"pwsh",
        "env":{"A":"B"},"pane_id":42,"lines":50,"include_scrollback":true,
        "badge_state":"attention","badge_text":"review","badge_sticky":true
    }))
    .unwrap();
    assert_eq!(request.env.unwrap()["A"], "B");
    assert_eq!(request.badge_state, Some(BadgeState::Attention));
    for op in ["list", "focus", "read-text", "set-badge"] {
        assert!(IpcRequest::is_known_op(op));
    }
}

#[test]
fn badge_text_is_limited_in_unicode_characters() {
    let badge = PaneBadge::new(BadgeState::Attention, Some("中🦀".repeat(20)), false);
    assert_eq!(badge.text.unwrap(), "中🦀".repeat(16));
    assert!(BadgeState::Attention > BadgeState::Running);
    assert!(BadgeState::Running > BadgeState::Idle);
    assert!(BadgeState::Idle > BadgeState::None);
}

#[test]
fn prompt_state_follows_the_latest_osc133_mark() {
    let mut surface = Surface::new(ScreenSize::new(20, 2));
    assert_eq!(surface.prompt_state(), PromptState::Unknown);
    for (sequence, state) in [
        ("\x1b]133;A\x07", PromptState::Idle),
        ("\x1b]133;B\x07", PromptState::Idle),
        ("\x1b]133;C\x07", PromptState::Running),
        ("\x1b]133;D;0\x07", PromptState::Idle),
    ] {
        surface.feed(sequence.as_bytes());
        assert_eq!(surface.prompt_state(), state);
    }
}

#[test]
fn prompt_events_keep_fast_command_completion_in_a_single_feed() {
    use mica_core::protocol::sidecar::ShellMark;
    let mut surface = Surface::new(ScreenSize::new(20, 2));
    surface.feed(b"\x1b]133;A\x07\x1b]133;B\x07");
    surface.take_shell_mark_events();
    surface.feed(b"\x1b]133;C\x07done\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07");
    assert_eq!(surface.prompt_state(), PromptState::Idle);
    assert_eq!(
        surface.take_shell_mark_events(),
        vec![
            ShellMark::OutputStart,
            ShellMark::CommandEnd { exit: Some(0) },
            ShellMark::PromptStart,
            ShellMark::CommandStart
        ]
    );
    assert!(surface.take_shell_mark_events().is_empty());
    surface.feed(b"\x1b]133;B\x07".repeat(5000).as_slice());
    assert_eq!(surface.take_shell_mark_events().len(), 4096);
}

#[test]
fn snapshot_preserves_wide_combining_and_soft_wrap_spaces() {
    let mut surface = Surface::new(ScreenSize::new(5, 3));
    surface.feed("中e\u{301}  Q\r\nend".as_bytes());
    let snapshot = surface.dump_text(3, false, 1024);
    assert_eq!(snapshot.text, "中e\u{301}  Q\nend\n");
    assert_eq!((snapshot.cols, snapshot.rows), (5, 3));
    assert!(!snapshot.truncated);
}

#[test]
fn snapshot_uses_physical_tail_and_ignores_viewport_scroll() {
    let mut surface = Surface::new(ScreenSize::new(5, 2));
    surface.feed(b"old\r\nabcdefghijK");
    surface.scroll_display(ScrollCommand::Top);
    let offset = surface.display_offset();
    assert_eq!(surface.dump_text(2, true, 1024).text, "fghijK\n");
    assert_eq!(surface.dump_text(1, false, 1024).text, "K\n");
    assert!(surface.dump_text(1, false, 1024).truncated);
    assert_eq!(surface.display_offset(), offset);
}

#[test]
fn snapshot_alt_screen_cannot_leak_main_screen_history() {
    let mut surface = Surface::new(ScreenSize::new(10, 2));
    surface.feed(b"secret\r\nold\r\nmain\x1b[?1049h\x1b[HALT");
    let snapshot = surface.dump_text(100, true, 1024);
    assert_eq!(snapshot.text, "ALT\n\n");
    assert!(!snapshot.truncated);
    surface.feed(b"\x1b[?1049l");
    assert!(surface.dump_text(100, true, 1024).text.contains("secret"));
}

#[test]
fn snapshot_byte_budget_keeps_utf8_tail_including_zero_budget() {
    let mut surface = Surface::new(ScreenSize::new(20, 1));
    surface.feed("old中🦀tail".as_bytes());
    let snapshot = surface.dump_text(1, false, 10);
    assert_eq!(snapshot.text, "🦀tail\n");
    assert!(snapshot.truncated);
    let empty = surface.dump_text(1, false, 0);
    assert_eq!(empty.text, "");
    assert!(empty.truncated);
    assert!(surface.dump_text(0, true, 1024).truncated);
}

#[test]
fn snapshot_skips_the_leading_spacer_when_a_wide_character_wraps() {
    let mut surface = Surface::new(ScreenSize::new(4, 2));
    surface.feed("abc中".as_bytes());
    assert_eq!(surface.dump_text(2, false, 1024).text, "abc中\n");
}

#[test]
fn snapshot_exact_budget_and_combining_on_a_space_are_preserved() {
    let mut surface = Surface::new(ScreenSize::new(5, 1));
    surface.feed("x \u{301}".as_bytes());
    let expected = "x \u{301}\n";
    let snapshot = surface.dump_text(1, false, expected.len());
    assert_eq!(snapshot.text, expected);
    assert!(!snapshot.truncated);
}

#[test]
fn snapshot_tail_is_accurate_after_scrollback_eviction() {
    let mut surface = Surface::new(ScreenSize::new(20, 2));
    for line in 0..10_050 {
        surface.feed(format!("line{line}\r\n").as_bytes());
    }
    assert_eq!(surface.grid_dims().0, 10_000);
    let snapshot = surface.dump_text(3, true, 1024);
    assert_eq!(snapshot.text, "line10048\nline10049\n\n");
    assert!(snapshot.truncated);
}

#[test]
fn read_response_bounds_final_escaped_json_frame() {
    // Every NUL expands to six JSON bytes; a raw 512KiB limit is insufficient.
    for text in [
        "\0".repeat(600_000),
        "\\\"\n".repeat(300_000),
        "中🦀".repeat(150_000),
    ] {
        let response = ReadTextResult {
            pane_id: u64::MAX,
            cols: usize::MAX,
            rows: usize::MAX,
            text,
            truncated: false,
        }
        .into_response(u64::MAX);
        assert!(encode_frame(&response).len() <= MAX_FRAME_BYTES);
        let data: ReadTextResult = serde_json::from_value(response.data.unwrap()).unwrap();
        assert!(data.truncated);
        assert!(data.text.len() <= READ_TEXT_MAX_BYTES);
    }
}
