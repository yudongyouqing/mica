# Footer visibility and path hit testing

User symptom: Claude Code Auto Mode status text disappeared from the bottom left;
hovering a file path highlighted another location. Work is based on 9d9bfbb.

## Reference contracts

- [wgpu 30 RenderPass::set_scissor_rect](https://docs.rs/wgpu/30.0.0/wgpu/struct.RenderPass.html#method.set_scissor_rect):
  scissor bounds use viewport/attachment coordinates; they discard fragments
  without translating coordinates. A content rectangle's bottom includes its
  origin, including the fixed title/tab strip.
- alacritty_terminal 0.26.0, the pinned project dependency: Grid indexes use
  negative lines for scrollback; a visible row maps to `row - display_offset`,
  and rendering reverses that with `buffer_line + display_offset`. Character
  width is represented by lead and continuation cells, not extra text characters.
  Source: [Grid](https://docs.rs/crate/alacritty_terminal/0.26.0/source/src/grid/mod.rs),
  [Cell](https://docs.rs/crate/alacritty_terminal/0.26.0/source/src/term/cell.rs).

## Findings and fixes

1. Pane scissor capped absolute Y at content height (`client height - strip`),
   cropping the final strip-height pixels. Restore the header origin when
   computing the viewport bound; skip empty clipped batches.
2. `span_of_token` returned `(column, 0, width)` while the caller read the second
   field as the column. Return an honest `(column, width)` pair.
3. `row_string` emitted wide continuation blanks and pushed duplicate map entries.
   Emit each character once, retain attached combining characters at their source
   cell, and include an end boundary. Keep a leading wrap-placeholder as a blank
   separator; it is not part of the preceding path. Path detection, underlines
   and hover share these boundaries, including wide continuation clicks.
4. Hover used the focused terminal with the rectangle of the hovered pane.
   Store pane identity in hover state, read the hit terminal and color only its
   render batch. Use pane-local coordinates once, negative history lines, and
   the same conversion for ordinary/double-click/drag selection. Ignore partial
   padding cells beyond the actual grid.
5. Refresh path state on both MOVE and HOVER. Scroll clears stale state. Exclude
   merely touching quad edges with a small subpixel intersection threshold so
   floating-point rounding cannot tint the adjacent background cell.

OSC 8 links keep their existing URI handling. Plain URI strings are no longer
normalized as Windows filesystem paths. Automatic detection of arbitrary bare
web URLs is outside this fix.

## Evidence

- Four independent pre-fix regressions were observed red: footer scissor height
  701 instead of 746; path start column 0 instead of 5; wrong pane-local/history
  coordinate; wide text containing extra spacer blanks and mismatched mapping.
- Tests cover wide and combining filenames, 10-column `C:/foobar中` wrapping,
  nonzero pane origin, scrollback round trips and rejection of another pane ID.
- Real isolated Mica windows rendered the same VT fixture, with a last-row
  `auto mode on` marker, a CJK-prefixed file path and an OSC 8 link. Screenshots
  before the fix show a blank bottom and highlight over the CJK prefix; after
  the fix show the footer and highlight aligned to the path. Only test-owned
  windows were clicked and closed; no links were opened.
- Independent review identified the leading-wrap boundary and combining-mark
  edge cases. Both were corrected and the final review found no blockers.

This is a deterministic terminal fixture reproducing the reported visual failure,
not an end-to-end Claude conversation or a change to Claude permission settings.
Workspace checks and installed-build evidence are reported in the PR.
