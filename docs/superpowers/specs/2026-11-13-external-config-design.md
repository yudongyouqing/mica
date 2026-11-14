# External configuration

User request: extract configuration completely. Scope: all existing user-facing
settings, defaults and key bindings, plus the currently hardcoded initial grid
and Quick Terminal height. Protocol constants and enforced resource/lifecycle
limits remain implementation invariants. The confirmed scope also includes
logical window padding; use Ghostty's `window-padding-x` / `window-padding-y`
keys and share a single padded content rectangle for rendering and hit testing.

## Chosen design

Keep the existing Ghostty-style `key = value` file at `%APPDATA%\mica\config`.
Move product defaults to `crates/mica-core/config/defaults`, one authoritative
text source with documented keys. Parse that source with the same resolver as
user settings, cache the immutable result, then apply theme and user overrides.
Keep an embedded copy so a missing external file never prevents startup. Expose
the complete defaults through `mica +show-config --default` and the configuration
path through `mica +show-config --path`. Provide `mica +validate-config` with
nonzero errors. Extract app configuration loading/watching/reload into its own
module, keeping UI/GPU work on the owning UI thread.

The user configuration contains overrides; a commented reference may list all
options without freezing defaults or overriding theme colors. Preserve existing
PowerShell 7 and theme preferences. No second independently maintained defaults
table is allowed. Compile-time palette/font constants needed by existing renderer
and TLS APIs are generated from the authoritative file at build time.

Alternatives considered: moving Rust functions alone would leave values scattered;
requiring a runtime defaults file would break standalone executable distribution.
The chosen source file plus embedded fallback keeps configuration editable and
exportable while preserving the existing deployment model.

## Behavior and validation

Fix existing declared-but-unparsed keys (background image/opacity, minimum
contrast, window position, right-click and copy-on-select), validate ranges, and
allow documented empty resets. New windows honor configured grid dimensions;
QT height applies on creation. Appearance and input behavior reload normally;
command changes affect future terminals, existing sessions retain their shell.

Regression requirements: preserve the old default Settings/StripCfg/Palette and
default key bindings; retain theme/user precedence and dynamic cursor/selection
colors; exported defaults must parse; BOM and invalid-value behavior remain safe;
real multi-window watcher/command regressions pass. Verify exact 83x17 PTY sizing
with a custom title/tab strip; validate native QT height and hotkey rebind/disable.
Verify fmt, workspace clippy,
workspace tests and release CLI/GUI with isolated identity and PID ownership.

Reference: Ghostty configuration overview and `+show-config --default --docs`,
https://ghostty.org/docs/config, accessed 2026-11-13. This aligns configuration
format and discoverability, not every Ghostty option or command-line flag.
