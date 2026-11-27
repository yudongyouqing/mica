# Pane control / snapshots / events review

Scope: issues #145, #146 and #147; core wire schema and terminal extraction,
Windows routing/lifecycle, event transport, strip badges and CLI startup.

## Specification review

All requested operations are implemented: launch options, stable pane identity,
list/focus/send addressing, text snapshots, badges and independent watch streams.
The original request/response wire format remains backward compatible. The
following findings were fixed and rechecked:

| Finding | Resolution and evidence |
| --- | --- |
| First-instance WSL cache was empty | Bounded profile discovery before GUI bootstrap in the child |
| Cold launches raced pipe/GPU startup | Reserve pipe early, forward collision losers, wait for ready windows under the original global deadline |
| Single-window deadline started during GPU setup | Separate readiness marker; deterministic 1650 ms delayed fixture now succeeds inside 3 s |
| Child inherited CLI pipe writers | Explicit zero-handle inheritance; release JSON command returns while GUI stays alive |
| Forwarding child exited between ACK read and exit check | Check process state before the final ACK-file read |
| Relative cwd used server directory | Resolve cwd in the invoking CLI; fresh-instance and release tests use `--cwd .` |
| Read drained output before normal render handling | Preserve pending effects and schedule normal output processing without changing viewport/focus |
| Fast C/D/A/B batch lost command completion | Bounded ordered mark queue; core regression and live watch test retain exit code |
| Split panes shared tab title in metadata | Expose each terminal's observed title |
| QT registry survived destruction or vanished with secondary window | Publish after mailbox setup; only its owning thread removes QT state; real-window regression covers both cases |
| Closed mailbox was reported as timeout during enumeration | Distinguish disconnected/removed mailboxes and omit closed windows from list |

## Code-quality review

No outstanding blocking findings remain. Registry, readiness and mailbox locks
have short, nonoverlapping lifetimes across native operations. No control request
waits synchronously from one UI thread on another. Unclaimed requests cancel on
timeout; claimed mutations retain the existing result-unknown contract.

Snapshots and escaped responses are bounded. Event queues share serialized lines,
use only nonblocking channel sends on the UI thread and reclaim stalled or idle
disconnected subscribers. Synchronous control writes and overlapped event writes
have cancellation regressions. Startup and stream handles have explicit owners.
Badges preserve terminal row caches and use existing rounded primitives/theme
colors. Transfer explicitly preserves the added terminal state.

## Verification

Final suite totals: **432 passed, seven existing ignored**. The repaired desktop
fixture passes as part of the complete window-lifecycle suite, including native
single-tab merging. Its owned windows/pointer form a controlled drag gesture;
the test asserts the real pane owner after migration. Production hit testing is
unchanged.

Strict workspace clippy, formatting and release compilation passed. An isolated
release smoke test passed JSON startup, cwd/environment/ID propagation, text read,
badge auto-clear and live NDJSON output. No installed application or personal
PowerShell/oh-my-posh configuration was changed.

The PR's Windows/macOS workflows provide the full CI validation for this tree.
