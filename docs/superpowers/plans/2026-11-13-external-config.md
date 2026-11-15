# External configuration implementation plan

**Goal:** centralize user-facing defaults, complete configuration parsing and
separate app configuration management while preserving user overrides.

Implementation includes configured logical padding and shared per-pane grid geometry.

**Architecture:** one default text source, generated constant projections where
required, cached parsed default Settings, existing theme/user precedence. The app
configuration module owns reading/watching/reload; Win32 work stays in that layer.

## Tasks

- [x] Add regressions for currently ignored settings, default parity and resets.
- [x] Add documented `mica-core/config/defaults`; generate font/palette constants
  in `mica-core/build.rs`; remove duplicated Rust defaults and binding tables.
- [x] Parse all declared settings and wire initial window/QT dimensions.
- [x] Extract configuration IO/watcher/reload to `mica-app/src/app/config.rs`.
- [x] Add default/path export and validation CLI commands with regressions.
- [x] Document supported keys and reload boundaries; review and run checks.
- [x] Back up and enrich the personal config with commented defaults; deploy a
  verified build without interrupting existing Mica sessions; update the PR.

Acceptance: fmt/diff checks, workspace clippy `-D warnings`, workspace tests,
isolated release CLI exports/validation and real main/QT PowerShell 7 startup.
