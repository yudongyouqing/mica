"""Isolated, bounded real-Mica tab lifecycle measurement; no global input."""
import argparse
import csv
import ctypes as C
from ctypes import wintypes as W
import hashlib
import itertools
import json
import os
from pathlib import Path
import platform
import subprocess as S
import sys
import time
import psutil

NO_WINDOW = 0x08000000
u = C.WinDLL("user32", use_last_error=True)
u.PostMessageW.argtypes = [W.HWND, W.UINT, W.WPARAM, W.LPARAM]
u.PostMessageW.restype = W.BOOL
u.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
u.GetClassNameW.argtypes = [W.HWND, W.LPWSTR, C.c_int]
u.IsWindowVisible.argtypes = [W.HWND]
CALLBACK = C.WINFUNCTYPE(W.BOOL, W.HWND, W.LPARAM)
u.EnumWindows.argtypes = [CALLBACK, W.LPARAM]
k = C.WinDLL("kernel32", use_last_error=True)
k.OpenProcess.argtypes = [W.DWORD, W.BOOL, W.DWORD]
k.OpenProcess.restype = W.HANDLE
k.GetProcessTimes.argtypes = [W.HANDLE] + [C.POINTER(W.FILETIME)]*4
k.WaitForSingleObject.argtypes = [W.HANDLE, W.DWORD]
k.WaitForSingleObject.restype = W.DWORD
k.TerminateProcess.argtypes = [W.HANDLE, W.UINT]
k.CloseHandle.argtypes = [W.HANDLE]


class Identity:
    """Retain one exact process object; never infer ownership from its name/PID."""
    def __init__(self, pid, created):
        self.pid, self.created = pid, created
        self.handle = k.OpenProcess(0x1000 | 0x100000 | 1, False, pid)
        if not self.handle:
            if C.get_last_error() == 87:  # already exited, no live PID to inspect
                return
            raise C.WinError(C.get_last_error())
        if self.creation_time() != created:
            k.CloseHandle(self.handle)
            self.handle = None  # PID was reused; that process is not ours

    def creation_time(self):
        values = [W.FILETIME() for _ in range(4)]
        if not k.GetProcessTimes(self.handle, *[C.byref(v) for v in values]):
            raise C.WinError(C.get_last_error())
        return (values[0].dwHighDateTime << 32) | values[0].dwLowDateTime

    def alive(self):
        if not self.handle:
            return False
        state = k.WaitForSingleObject(self.handle, 0)
        if state == 0xFFFFFFFF:
            raise C.WinError(C.get_last_error())
        return state != 0

    def terminate(self):
        if self.alive():
            if self.creation_time() != self.created:
                raise RuntimeError("held process handle identity changed")
            if not k.TerminateProcess(self.handle, 1):
                raise C.WinError(C.get_last_error())
            if k.WaitForSingleObject(self.handle, 5000) != 0:
                raise RuntimeError(f"owned child did not exit: {self.pid}")

    def close(self):
        if self.handle:
            k.CloseHandle(self.handle)
            self.handle = None


def windows(pid):
    found = []
    @CALLBACK
    def visit(hwnd, _):
        owner = W.DWORD()
        u.GetWindowThreadProcessId(hwnd, C.byref(owner))
        name = C.create_unicode_buffer(128)
        u.GetClassNameW(hwnd, name, len(name))
        if owner.value == pid and name.value == "mica_app_class":
            found.append(hwnd)
        return True
    u.EnumWindows(visit, 0)
    return found


def post(hwnd, message, key=0):
    if not u.PostMessageW(hwnd, message, key, 1):
        raise C.WinError(C.get_last_error())


def stop_tree(child):
    # The live Popen retains its Windows process handle throughout taskkill.
    if child.poll() is None:
        try:
            S.run(["taskkill", "/PID", str(child.pid), "/T", "/F"],
                  creationflags=NO_WINDOW, capture_output=True, timeout=5)
        finally:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--source-head", required=True)
    a = ap.parse_args()
    if not 1 <= a.rounds <= 200:
        ap.error("rounds must be between 1 and 200")
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    isolation = out / "isolation"
    (isolation / "mica").mkdir(parents=True)
    config = ["title = Mica isolated resource measurement", "cursor-blink = false",
              "strip-animation-duration = 0", "strip-focus-animation-duration = 0"]
    for mask in itertools.product([False, True], repeat=4):
        trigger = "+".join([name for name, enabled in zip(
            ["ctrl", "alt", "shift", "win"], mask) if enabled] + ["delete"])
        config.append(f"keybind = {trigger} = close_tab")
    config = "\n".join(config) + "\n"
    (isolation / "mica" / "config").write_text(config, encoding="utf-8")
    env = os.environ.copy()
    env.update(APPDATA=str(isolation), LOCALAPPDATA=str(isolation),
               USERNAME=f"mica_curve_{os.getpid()}_{time.time_ns()}",
               MICA_CRASH_NOPROMPT="1", MICA_DEBUG_TIMING="1")
    registry = isolation / "mica" / "pty-sessions.txt"
    exe = a.exe.resolve()
    meta = dict(exe=str(exe), exe_sha256=hashlib.sha256(exe.read_bytes()).hexdigest(),
                source_head=a.source_head, python=sys.version, psutil=psutil.__version__,
                os=platform.platform(), windows_version=platform.win32_ver(),
                logical_cpus=psutil.cpu_count(), physical_ram=psutil.virtual_memory().total,
                config=config, isolated_username=env["USERNAME"], rounds=a.rounds,
                startup_idle_s=15, final_idle_s=30, render_wait_s=0.3,
                after_close_wait_s=0.1, registry_timeout_s=10, cli_timeout_s=8,
                visibility="normal visible window; no focus/input injection",
                stop_limits=dict(private_or_ws_mib=1536, private_growth_mib=512,
                                 handles_growth=500, threads_growth=64,
                                 registry_sessions=3, minimum_available_ram_mib=2048))
    (out / "environment.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")
    log = (out / "mica.stderr.log").open("wb")
    trace = (out / "driver.events.jsonl").open("w", encoding="utf-8")
    data = (out / "samples.csv").open("w", newline="", encoding="utf-8")
    fields = ["elapsed_s", "round", "phase", "private_bytes", "working_set", "handles",
              "threads", "cpu_s", "children", "registry_total", "registry_running",
              "registry_closing", "visible", "system_available"]
    writer = csv.DictWriter(data, fields)
    writer.writeheader()
    app = None
    started = time.perf_counter()
    baseline = None
    rows = []
    completed = 0
    failure = None
    identities = {}

    def event(kind, **values):
        trace.write(json.dumps(dict(elapsed_s=time.perf_counter()-started,
                                    kind=kind, **values), ensure_ascii=False) + "\n")
        trace.flush()

    def records():
        if not registry.exists():
            return []
        parsed = []
        for line in registry.read_text(encoding="utf-8").splitlines():
            tokens = line.split()
            if len(tokens) != 7 or tokens[6] not in ["R", "C"]:
                raise RuntimeError(f"unexpected registry record: {line!r}")
            if int(tokens[0]) == app.pid:
                parsed.append(tokens)
                for pi, ti in [(2, 3), (4, 5)]:
                    key = (int(tokens[pi]), int(tokens[ti]))
                    if key[0] and key not in identities:
                        if not key[1]:
                            raise RuntimeError(f"missing strong child/host identity: {tokens}")
                        identities[key] = Identity(*key)
            else:
                raise RuntimeError("foreign owner in isolated registry")
        if len(parsed) > 3:
            raise RuntimeError(f"session safety cap exceeded: {parsed}")
        return parsed

    def sample(n, phase):
        if app.poll() is not None:
            raise RuntimeError(f"Mica exited unexpectedly: {app.returncode}")
        rec = records()
        mi = proc.memory_info()
        cpu = proc.cpu_times()
        row = dict(elapsed_s=round(time.perf_counter()-started, 6), round=n, phase=phase,
                   private_bytes=mi.private, working_set=mi.rss, handles=proc.num_handles(),
                   threads=proc.num_threads(), cpu_s=round(cpu.user+cpu.system, 6),
                   children=len(proc.children(recursive=True)), registry_total=len(rec),
                   registry_running=sum(r[6] == "R" for r in rec),
                   registry_closing=sum(r[6] == "C" for r in rec),
                   visible=int(any(u.IsWindowVisible(h) for h in windows(app.pid))),
                   system_available=psutil.virtual_memory().available)
        writer.writerow(row)
        data.flush()
        rows.append(row)
        if max(row["private_bytes"], row["working_set"]) > 1536*1024**2 or row["system_available"] < 2048*1024**2:
            raise RuntimeError(f"memory safety cap: {row}")
        if baseline and (row["private_bytes"] > baseline["private_bytes"] + 512*1024**2
                         or row["handles"] > baseline["handles"] + 500
                         or row["threads"] > baseline["threads"] + 64):
            raise RuntimeError(f"growth safety cap: {row}")
        return row

    def wait_records(target, n):
        deadline = time.monotonic() + 10
        while True:
            rec = records()
            sample(n, "waiting_registry")
            if len(rec) == target and all(r[6] == "R" for r in rec):
                return rec
            if time.monotonic() >= deadline:
                raise RuntimeError(f"registry did not settle to {target}: {rec}")
            time.sleep(0.05)

    def cli(args):
        begin = time.perf_counter()
        child = S.Popen([str(exe), *args], env=env, creationflags=NO_WINDOW,
                        stdin=S.DEVNULL, stdout=S.PIPE, stderr=S.PIPE)
        try:
            stdout, stderr = child.communicate(timeout=8)
        except S.TimeoutExpired:
            stop_tree(child)
            raise RuntimeError(f"CLI timeout: {args}")
        event("cli", args=args, exit=child.returncode, duration_s=time.perf_counter()-begin,
              stdout=stdout.decode("utf-8", "replace"), stderr=stderr.decode("utf-8", "replace"))
        if child.returncode:
            raise RuntimeError(f"CLI failed: {args}: {stderr!r}")

    try:
        app = S.Popen([str(exe), "--multi"], env=env, creationflags=NO_WINDOW,
                      stdin=S.DEVNULL, stdout=S.DEVNULL, stderr=log)
        proc = psutil.Process(app.pid)
        meta.update(pid=app.pid, process_create_time=proc.create_time())
        (out / "environment.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")
        for _ in range(15):
            sample(0, "startup_idle")
            time.sleep(1)
        stderr = (out / "mica.stderr.log").read_text(encoding="utf-8", errors="replace")
        if "[timing] 延迟段 " not in stderr:
            raise RuntimeError("deferred initialization did not complete during startup idle")
        baseline = sample(0, "baseline")
        initial_records = records()
        expected = len(initial_records)
        if expected not in [1, 2] or baseline["registry_closing"] or len(windows(app.pid)) != 1:
            raise RuntimeError(f"unexpected startup fixture: {initial_records}")
        initial_children = {(int(r[2]), int(r[3])) for r in initial_records}
        initial_processes = {(int(r[p]), int(r[t])) for r in initial_records
                             for p, t in [(2, 3), (4, 5)] if int(r[p])}
        event("baseline_registry", records=initial_records, hwnd=windows(app.pid)[0])
        cli(["send", "--tab=0", "--literal", "\r"])
        for n in range(1, a.rounds+1):
            cli(["new-tab", "Command Prompt"])
            opened = wait_records(expected+1, n)
            transient = {(int(r[2]), int(r[3])) for r in opened} - initial_children
            transient_processes = {(int(r[p]), int(r[t])) for r in opened
                                   for p, t in [(2, 3), (4, 5)] if int(r[p])} - initial_processes
            if len(transient) != 1:
                raise RuntimeError(f"new tab not identifiable: {opened}")
            event("opened_registry", round=n, records=opened)
            sample(n, "open")
            cli(["send", "--tab=1", "--literal", f"echo 资源回收验证-{n:04d}\r"])
            time.sleep(0.3)
            sample(n, "rendered")
            own_windows = windows(app.pid)
            if len(own_windows) != 1:
                raise RuntimeError(f"unexpected Mica windows: {own_windows}")
            post(own_windows[0], 0x0100, 0x2E)  # WM_KEYDOWN, VK_DELETE: config close_tab
            post(own_windows[0], 0x0101, 0x2E)
            closed = wait_records(expected, n)
            if {(int(r[2]), int(r[3])) for r in closed} != initial_children:
                raise RuntimeError("close_tab removed the baseline tab instead")
            deadline = time.monotonic() + 2
            while any(identities[key].alive() for key in transient_processes) and time.monotonic() < deadline:
                time.sleep(0.05)
            if any(identities[key].alive() for key in transient_processes):
                raise RuntimeError(f"closed tab child/host still exists: {transient_processes}")
            event("closed_registry", round=n, records=closed)
            time.sleep(0.1)
            sample(n, "closed")
            completed = n
            if n <= 5 or n % 10 == 0:
                print(f"round {n}/{a.rounds}: sessions={expected}, registry clean", flush=True)
        for _ in range(30):
            sample(completed, "final_idle")
            time.sleep(1)
    except BaseException as error:
        failure = f"{type(error).__name__}: {error}"
        event("failure", detail=failure)
        print(failure, flush=True)
    finally:
        cleanup_errors = []
        try:
            if app and app.poll() is None:
                for hwnd in windows(app.pid):
                    try:
                        post(hwnd, 0x0010)  # WM_CLOSE only our class/PID
                    except OSError as error:
                        event("close_message_race", detail=str(error))
                try:
                    app.wait(timeout=10)
                except S.TimeoutExpired:
                    event("forced_cleanup", pid=app.pid)
                    stop_tree(app)
            deadline = time.monotonic() + 5
            while any(item.alive() for item in identities.values()) and time.monotonic() < deadline:
                time.sleep(0.05)
            leftovers = [item for item in identities.values() if item.alive()]
            if leftovers:
                cleanup_errors.append(f"normal shutdown left children/hosts: {[p.pid for p in leftovers]}")
                for item in leftovers:
                    item.terminate()  # same held handle + FILETIME, never by name/PID alone
            event("app_exit", code=app.poll() if app else None,
                  tracked_identities=len(identities), registry=records() if app else [])
        except BaseException as error:
            cleanup_errors.append(f"cleanup: {error}")
            if app and app.poll() is None:
                try:
                    stop_tree(app)
                except BaseException as nested:
                    cleanup_errors.append(f"forced cleanup: {nested}")
        finally:
            for item in identities.values():
                item.close()
        if cleanup_errors:
            failure = "; ".join(filter(None, [failure, *cleanup_errors]))
        data.close()
        trace.close()
        log.close()
        result = dict(success=failure is None, completed_rounds=completed, requested_rounds=a.rounds,
                      failure=failure, elapsed_s=time.perf_counter()-started, samples=len(rows))
        (out / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    return 0 if failure is None else 1


if __name__ == "__main__":
    raise SystemExit(main())
