# Configured terminal command

## Reference and scope

Reference: [Ghostty command documentation](https://ghostty.org/docs/config/reference#command),
accessed 2026-11-13 (rolling documentation, not a pinned release).
The configuration key selects the executable and arguments for future terminal
surfaces, including windows, tabs and splits. Mica also applies it to its Quick
Terminal when that terminal is initially created. An explicit Mica profile takes
precedence. Existing sessions and their restart/tear-off commands are retained.

This aligns the key and scope, rather than claiming full Ghostty compatibility.
Windows uses the existing direct CRT-style argument parser, with the existing
PowerShell `-Command` source-tail handling. Unix shell expansion, `shell:` and
`direct:` prefixes are not implemented. An unset/empty value preserves Mica's
Windows PowerShell default; PowerShell 7 can be selected with `command = pwsh.exe
-NoLogo` or a quoted absolute path. Profile loading is enabled unless the user
explicitly includes `-NoProfile`.

## Root causes

- `Settings` did not recognize a terminal command. Launch paths hardcoded
  `powershell.exe -NoLogo`, so changing the calling shell did not select PS7.
- The end-to-end launch regression exposed an additional Quick Terminal defect:
  its backlog posted `WM_APP` to the main HWND, whose message handler does not
  consume that message. Both child processes had the configured command line,
  but only the main shell executed the marker script; the QT shell remained
  blocked during startup. Give QT its own wake HWND and invalidate it when
  forgetting the QT state. Keep the original single backlog retry monitor.

## Local evidence

| Check | Result |
| --- | --- |
| Config regression before fix | Failed: configured settings equaled defaults |
| GUI regression before launch fix | Failed: zero configured shell markers |
| GUI after launch fix, before wake fix | Failed: one marker instead of two; both children had the configured argv |
| GUI after wake fix | Main + hidden QT, split, new tab and secondary window all execute the configured script |
| Real watcher reload | Existing five shells retain old command; one new tab executes updated command |
| Explicit profile / empty reset | Unit tests pass; explicit profile GUI coverage is not claimed |
| `cargo test --workspace` | 345 passed, zero failed, five ignored |
| `cargo clippy --workspace --all-targets -- -D warnings` | Pass |
| `cargo fmt --all`, `git diff --check` | Pass |

After independent review removed a duplicate retry-monitor call introduced
during implementation, lint, the full GUI command regression and release build
passed again. No other behavior changed after the workspace test run.

The release build was also launched with isolated identity/state directories and
an absolute path to the local standalone PowerShell installation. IPC was sent
only after verifying the named-pipe server PID matched the owned Mica process.
Both main and hidden QT reported PowerShell **7.6.6**, `PSEdition = Core`, the
expected standalone executable, the existing M365Princess Oh My Posh theme and
distinct session IDs, after inherited `POSH_*` variables were removed. All
validation subprocesses were bounded and the owned process tree was cleaned up.
This is runtime startup/theme evidence, not a visual typography acceptance test.
