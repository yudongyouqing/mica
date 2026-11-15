# External configuration: behavior and evidence

## Authoritative configuration

`crates/mica-core/config/defaults` defines all 43 user-facing keys and 34 default
bindings. Default Settings and StripCfg are parsed/cached from this source;
build.rs generates only the font and palette const projections needed by the
existing renderer/TLS APIs. There is no independently maintained Rust table of
product values. The embedded source preserves standalone-executable startup.

Personal overrides remain in `%APPDATA%\mica\config`. The effective order is
defaults, selected theme, then user overrides; `auto` cursor/selection colors are
derived after all layers. The app's configuration loading/watching/reload is in
`crates/mica-app/src/app/config.rs`. Resource/lifecycle safety limits and protocol
constants are implementation invariants, outside this user-facing scope.

## Industry reference and declared differences

Reference: Ghostty rolling documentation, accessed 2026-11-13:

- https://ghostty.org/docs/config : text configuration and default export.
- https://ghostty.org/docs/config/reference#command : future-terminal command.
- https://ghostty.org/docs/config/reference#window-width : initial size in cells.
- https://ghostty.org/docs/config/reference#window-padding-x : DPI-scaled padding.

Mica preserves its Windows configuration path and Win32/CRT command parsing. It
supports `+show-config --default [--docs]`, `+show-config --path` and
`+validate-config`; arbitrary configuration CLI flags and effective-config export
are not implemented. Padding uses one symmetric Windows logical-pixel value per
axis, supports reload, and does not implement Ghostty's asymmetric comma pairs.
Grid dimensions may be specified independently; the unset axis keeps its default.
QT height uses Mica's `quick-terminal-height` percent key. See README for reload
versus restart scope, especially QT appearance. These are explicit compatibility
boundaries, not claims of complete Ghostty behavior.

## Correctness changes exposed during extraction

- Connect previously ignored background image/opacity, contrast, position,
  right-click and copy-on-select values with finite/range validation.
- Register/rebind/disable the configured QT hotkey, replacing a fixed Win+grave.
- Remove the fixed Shift+Insert fallback so `keybind = clear` takes effect.
  Context-sensitive Ctrl+C/V remain explicitly documented terminal behavior.
- Preserve all effective old bindings. The old Vec contained unreachable later
  arrow duplicates; migration keeps prompt-jump behavior and verifies all 34
  effective bindings against an independent typed regression table.
- Correct initial full-client window sizing: account for custom strip, padding,
  DPI and fractional cells without adding system nonclient borders. The original
  83x17 request returned 84x14 in a real PowerShell 7 shell. The first corrected
  attempt still lost padding cells (82x16); both failures informed the final
  shared geometry calculation.
- Share padded content/layout and resize helpers across creation, rendering,
  input hit testing, native resize, DPI and configuration reload. Background
  ordinary tabs and split panes update without activation; no full-window
  intermediate PTY resize is used during reload.

## Validation

- Existing declared-option and missing-default-source tests were observed red
  before implementation; configuration CLI export was also observed red.
- Config/parser tests cover bounds, nonfinite values, BOM, empty optional resets,
  theme precedence and dynamic cursor/selection colors.
- Real GUI regression uses nonzero padding, a custom title/tab strip and an
  83x17 initial PTY; exercises tabs, split and secondary window, then changes
  padding with the real watcher and compares inactive ordinary/split grids to
  the active grid. Native SetWindowPos resize repeats the inactive-grid checks.
- Native QT regression verifies 23% monitor height and actual hotkey registration
  through none, bind, rebind, disable. No simulated global key input is sent.
- Release instance with isolated identity/state and PID-verified IPC reports
  main 83x17, PowerShell 7.6.6 in main and hidden QT, expected standalone pwsh,
  existing M365Princess theme and independent session IDs. Owned process trees
  are cleaned up with deadlines.
- Independent static review found grid/padding and Shift+Insert issues; all were
  corrected, and the final review reported no remaining blocking items.

Full workspace test and clippy results are recorded in the PR. DPI uses the same
geometry helper; physically moving between different-DPI monitors was not tested.
No new visual/font-typography acceptance or long-run performance claim is made.
