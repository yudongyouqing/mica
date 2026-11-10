# Launch profile discovery and command lines (#107)

GUI tab creation, IPC application on the UI thread, and deferred initialization
read `cached_profiles()`. Built-in profiles are always in the first snapshot.
WSL discovery runs in one background worker, never under the cache mutex.
Successful snapshots remain fresh for 60 seconds; failed attempts retain the
last snapshot and have a 30-second retry cooldown. Requests only refresh an
expired snapshot; invalid names cannot bypass the cooldown. A single-slot
notification wakes the one Jump List worker after successful refreshes, so
WSL profiles also become visible in the taskbar without restarting Mica.
Shell COM initialization and installation run on that worker and are paired
with COM teardown. Notification overflow coalesces to the latest snapshot.

`wsl.exe --list --all` has a 3-second execution/collection deadline and a
64-KiB stdout limit. Stderr goes to the null device. Windows polls available
pipe bytes with `PeekNamedPipe`; Unix uses a nonblocking descriptor. Neither
path starts a reader thread or waits for pipe EOF from descendants. Timeout,
excess output, and other failures terminate and reap the directly owned child.
Termination/reaping follows the deadline synchronously: OS termination latency
is cleanup overhead, not a hard real-time guarantee under kernel/driver faults.
The entire collection loop checks its deadline even for a continuously writing
process, and CREATE_NO_WINDOW avoids default-terminal delegation on Windows.

The standalone CLI `list-profiles` and `wsl` listing retain blocking, bounded
discovery. If no GUI exists, an explicit non-built-in startup profile is resolved
on the CLI thread before the message loop and primes the first GUI snapshot.
An invalid/unavailable profile exits with an error instead of launching the
default shell under the requested name.

For an existing GUI, explicit profile lookup on the IPC worker joins the same
refresh for at most 750 ms. A pending refresh returns `timeout` before the
request enters the UI queue; it does not open a tab. This leaves most of the
existing 3-second client budget for PTY creation and response delivery. The
server subtracts time spent waiting for discovery from its UI reply budget.
Only this pre-queue timeout is safe to retry without risking duplicate tabs;
a transport or post-queue timeout retains the existing uncertain-result
semantics. The current wire error code does not distinguish those phases.

Command strings use Windows CRT double-quote and backslash rules, including
empty arguments, escaped quotes, and backslashes before delimiters. A whole
existing executable path still works as a `shell` setting without quoting.
For PowerShell/pwsh, ordinary unquoted `-Command` source preserves the remainder
as source code, so its string quotes are not removed by native argv parsing.
One explicitly quoted script argument is decoded as one argument. Generated
WSL commands and Jump List task arguments quote profile names as single args.
Header/default-marker filtering retains real names such as `kali-linux`,
`AlmaLinux`, and `Ubuntu (Work)` rather than treating those words or parentheses
as localization metadata.

Regression coverage includes real timeout injection and direct-child exit,
output overflow, concurrent cache reads during a blocked refresh, cooldown and
last-good snapshot retention, coalesced update notification, Windows quoting,
and a real PTY child copied to a path with spaces that reports its exact argv.
A real PowerShell `-Command` launch checks that script quotes survive as well.
PowerShell protocol tests construct completion markers at runtime so PSReadLine
input echo cannot stand in for actual command execution.

On Windows 10.0.26300.9711, the existing OSC 133 test failed 3/3 runs both in
the original `5217d0a` source snapshot and with these changes because it accepted
the injected command's PSReadLine echo as completion. Runtime-built completion
markers restore actual-execution assertions; all three PowerShell protocol
tests then passed without changing the OS-build expectations or user profile.

An isolated GUI fault-injection run used a 30-second-sleeping `wsl.exe` fixture
beside the separately built test Mica executable. While enumeration was still
alive, default `new-tab` succeeded in 180 ms, an unknown profile returned
pre-queue `timeout` in 856 ms, and `Command Prompt` succeeded in 156 ms. After
the enumeration deadline the fixture PID had exited, the invalid profile
returned `bad_profile` in 96 ms, and the invocation log still contained exactly
one discovery attempt. The GUI had its own PID, USERNAME, APPDATA and
LOCALAPPDATA; only that process tree was cleaned up and the fixture was removed.
