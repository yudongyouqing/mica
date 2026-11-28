# Pane control, text snapshots and status events

Implements issues #145, #146 and #147 on the current 1.5 UI/transfer base.
Existing control operations and their acknowledgement semantics remain compatible.

1. Extend the core wire schema, stable pane metadata, OSC 133 prompt state,
   bounded Unicode text snapshots and badge types. Test old frames and snapshot
   boundaries, including JSON escaping and alternate screens.
2. Allocate pane IDs before PTY spawn, inject reserved identity, preserve initial
   cwd/environment across restart and transfer. Add cross-window dispatch with
   cancellation and bounded native message deadlines; list/focus/send/read/badge
   target the owning UI thread. Return concrete pane metadata for creation.
3. Add an independent per-user event pipe, bounded fanout and cancellable writes.
   Implement status dots using existing strip primitives and palette colors.
4. Wire lifecycle/output/focus events and implement strict CLI parsing, JSON/plain
   output, watch filtering and first-instance spawn option propagation.
5. Run focused tests, workspace tests and strict clippy. Review specification
   coverage and code quality; fix findings. Document usage and validation.

Ownership: core schema/surface; native control/lifecycle; events/strip; root CLI,
integration and review. Shared-file edits are coordinated explicitly.

Native snapshots never read another thread's TLS directly. No UI thread waits for
another UI thread. Explicit invalid cwd fails without silent fallback. Snapshot
and event serialization must remain bounded; slow readers must release resources.
No installed application is replaced or restarted as part of this development.

## Implementation decisions and review

- Worker-to-window operations use posted messages and a bounded mailbox rather
  than synchronous native sends. A request can be claimed once, or cancelled
  before claim; a claimed operation may finish after its caller times out.
  Each window wait is at most 1500 ms and enumeration shares a 3 s deadline.
  The native HWND registry remains available during bootstrap, but control
  routing waits for a separate readiness marker after GPU/first-frame setup;
  startup consumes the global budget before the per-window wait begins.
- The first process reserves the control pipe before GPU setup. A competing
  cold launch forwards its original request. Initial options and its concrete
  ACK travel through child-only environment entries and a temporary JSON file.
  The GUI inherits zero handles, so returning JSON does not leave CLI pipes open.
- Text snapshots keep at most 128 KiB of UTF-8, leaving room for worst-case
  six-byte JSON escaping within the existing 1 MiB frame. Pending output is
  captured once from the bounded backlog; normal render messages still process
  its damage, title, clipboard and synchronized-output effects.
- Prompt events retain ordered OSC 133 transitions separately from row markers.
  A fast C/D/A/B sequence emits its exit event even if its final state is idle.
- Event fanout has eight fixed pipe workers and 128 queued shared lines per
  subscriber. Overlapped writes cancel after 1 s; idle disconnects are probed
  off the UI thread. Badge rendering changes no terminal-grid dirty flags.

Native API references checked against windows-rs 0.62.2 bindings and Microsoft
Learn: [CreateProcessW](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw),
[AttachConsole](https://learn.microsoft.com/en-us/windows/console/attachconsole),
[standard handles](https://learn.microsoft.com/en-us/windows/console/getstdhandle).

Independent specification/code review found and resolved cold-start profile and
ACK races, relative cwd resolution, read/render handoff, Quick Terminal index
publication/destruction, per-pane titles and fast command-completion events.

## Validation

The follow-up review suites account for 444 passing tests and eight ignored tests (including subprocess helper entry points explicitly exercised by parent tests).
Core/render and all other application suites passed in the workspace run. Its
desktop drag fixture was then made independent of unrelated topmost windows and
pointer movement; the complete window-lifecycle suite passed (12 passed, one
existing ignored). It verifies pane identity, destination window and live input
after detach/rejoin. No product hit-testing rule was relaxed.

- Strict workspace clippy (`--all-targets -- -D warnings`) and formatting pass.
- Deterministic 1650 ms bootstrap regression fails before the readiness fix and
  passes afterwards within the unchanged 3 s total request budget.
- Release build passes; an isolated release executable smoke test verifies first
  pane JSON, caller-relative cwd, injected identity/environment, read snapshots,
  badge/focus clearing and live watch NDJSON. It does not replace the installed
  application or modify personal shell configuration.
- Specification and code-quality review dispositions are recorded in
  `docs/superpowers/reviews/2026-11-28-agent-control.md`.


## Follow-up review dispositions

Re-review found eight gaps missed by the initial tests: reopened-pane routing and
launch environment, effective default cwd, hidden QT focus redraw, snapshot tabs,
forwarded command flags, partial NDJSON/UTF-8 EOF, Ctrl+C during blocked stdout,
and silent producer-side mark overflow. Each has a reproduction or regression;
the implementation and updated review record include the fixes. Original green
CI alone was not sufficient evidence for those paths.
