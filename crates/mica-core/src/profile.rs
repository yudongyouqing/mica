//! Profile 体系(M3a,spec §3):可启动的 shell 集合 = 静态项 + WSL 枚举。
//!
//! 纯逻辑 + 一次性子进程 IO(`wsl.exe --list --all`);WSL 缺失/无发行版/
//! 解析失败一律静默降级为空列表,终端绝不因枚举失败报错。

use std::io::{self, Read};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

struct CacheState {
    distributions: Vec<String>,
    refreshing: bool,
    next_refresh: Instant,
    updates: Option<std::sync::mpsc::SyncSender<()>>,
}

struct ProfileCache {
    state: Mutex<CacheState>,
    ready: Condvar,
    load: Box<dyn Fn() -> std::io::Result<Vec<String>> + Send + Sync>,
}

impl ProfileCache {
    fn new(load: impl Fn() -> std::io::Result<Vec<String>> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CacheState {
                distributions: Vec::new(),
                refreshing: false,
                next_refresh: Instant::now(),
                updates: None,
            }),
            ready: Condvar::new(),
            load: Box::new(load),
        })
    }

    fn subscribe(&self) -> std::sync::mpsc::Receiver<()> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.state.lock().unwrap_or_else(|e| e.into_inner()).updates = Some(tx);
        rx
    }

    fn snapshot_and_refresh(self: &Arc<Self>) -> Vec<Profile> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let profiles = profiles_from_distributions(state.distributions.clone());
        if state.refreshing || Instant::now() < state.next_refresh {
            return profiles;
        }
        state.refreshing = true;
        drop(state);
        let cache = self.clone();
        if std::thread::Builder::new()
            .name("wsl-profiles".into())
            .spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (cache.load)()))
                        .unwrap_or_else(|_| Err(io::Error::other("profile enumeration panicked")));
                let mut state = cache.state.lock().unwrap_or_else(|e| e.into_inner());
                let cooldown = if let Ok(names) = result {
                    state.distributions = names;
                    if matches!(
                        state.updates.as_ref().map(|tx| tx.try_send(())),
                        Some(Err(std::sync::mpsc::TrySendError::Disconnected(())))
                    ) {
                        state.updates = None;
                    }
                    Duration::from_secs(60)
                } else {
                    Duration::from_secs(30)
                };
                state.next_refresh = Instant::now() + cooldown;
                state.refreshing = false;
                cache.ready.notify_all();
            })
            .is_err()
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.refreshing = false;
            state.next_refresh = Instant::now() + Duration::from_secs(30);
            self.ready.notify_all();
        }
        profiles
    }

    fn find(
        self: &Arc<Self>,
        name: &str,
        wait: Duration,
    ) -> Result<Option<Profile>, ProfilePending> {
        if let Some(profile) = self
            .snapshot_and_refresh()
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
        {
            return Ok(Some(profile));
        }
        let deadline = Instant::now() + wait;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.refreshing {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(ProfilePending);
            };
            state = self
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(profiles_from_distributions(state.distributions.clone())
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(name)))
    }
}

fn cache() -> &'static Arc<ProfileCache> {
    static CACHE: OnceLock<Arc<ProfileCache>> = OnceLock::new();
    CACHE.get_or_init(|| ProfileCache::new(enumerate_wsl))
}

/// UI-safe snapshot: built-in profiles are always immediately available.
/// WSL refresh is single-flight; failed attempts retain the previous snapshot
/// and cool down for 30s. Successful snapshots are refreshed after 60s.
pub fn cached_profiles() -> Vec<Profile> {
    cache().snapshot_and_refresh()
}

/// One coalesced notification slot for the application's Jump List worker.
/// Subscribe before reading the initial snapshot to avoid missing a refresh.
/// A new subscriber replaces the previous subscriber; snapshots stay authoritative.
pub fn subscribe_profile_updates() -> std::sync::mpsc::Receiver<()> {
    cache().subscribe()
}

/// Enumeration is still running after the caller's wait budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfilePending;

/// Explicit profile lookup for CLI/bootstrap and the IPC worker only. Waiting
/// joins the same refresh as UI snapshots; unknown names cannot force scans.
pub fn find_profile(name: &str, wait: Duration) -> Result<Option<Profile>, ProfilePending> {
    cache().find(name, wait)
}

fn profiles_from_distributions(distributions: Vec<String>) -> Vec<Profile> {
    let mut profiles = scan_static();
    profiles.extend(distributions.into_iter().map(|name| Profile {
        command: format!("wsl.exe -d {}", crate::pty::quote_windows_arg(&name)),
        name,
    }));
    profiles
}

/// 一个可启动的终端 profile。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// 显示名(标签占位标题/jump list 项)
    pub name: String,
    /// 启动命令行(整串,交 pty)
    pub command: String,
}

fn powershell() -> Profile {
    Profile {
        name: "PowerShell".into(),
        command: "powershell.exe -NoLogo".into(),
    }
}

fn cmd() -> Profile {
    Profile {
        name: "Command Prompt".into(),
        command: "cmd.exe".into(),
    }
}

/// 静态项(任何 Windows 都有;顺序即 jump list 顺序)。
pub fn scan_static() -> Vec<Profile> {
    vec![powershell(), cmd()]
}

/// wsl.exe --list --all 的 stdout → 发行版名列表。
///
/// 真机实证(2026-10-01):输出 **UTF-16LE 无 BOM**;表头随系统语言本地化
/// (中文"适用于 Linux 的 Windows 子系统:");默认发行版标记是本地化后缀
/// "(默认)"而非文档宣称的 `*` 前缀(两者都容错);行尾 \r\n。
/// 解码启发:前 64 字节的奇数索引位置大量 0x00 → UTF-16LE,否则 UTF-8
/// (新 wsl 版本有 UTF-8 输出形态)。
pub fn parse_wsl_output(raw: &[u8]) -> Vec<String> {
    let text = decode_wsl_bytes(raw);
    text.lines().filter_map(parse_distrow_line).collect()
}

/// 按启发解码 wsl.exe 输出(UTF-16LE / UTF-8 双形态)。
fn decode_wsl_bytes(raw: &[u8]) -> String {
    let probe = &raw[..raw.len().min(64)];
    let zeros_odd = probe.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
    // BOM 直接判定(FF FE/FE FF);启发:奇数索引大量 0x00 = UTF-16LE
    let has_bom = raw.starts_with(&[0xff, 0xfe]) || raw.starts_with(&[0xfe, 0xff]);
    let likely_utf16 = has_bom || (!probe.is_empty() && zeros_odd * 2 >= probe.len() / 2);
    if likely_utf16 {
        // 去 BOM(FF FE);奇数长度容忍截断
        let raw = raw.strip_prefix(&[0xff, 0xfe]).unwrap_or(raw);
        let (pairs, _) = raw.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    }
}

/// 单行 → 发行版名:跳过冒号结尾的本地化表头,
/// 剥 `* ` 前缀与已知 "(默认)/(Default)" 后缀。
fn parse_distrow_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // Linux is also part of valid names (kali-linux, AlmaLinux). Do not
    // reject a distribution just because it shares a word with a header.
    let lower = line.to_lowercase();
    if line.ends_with([':', '：'])
        || lower == "windows subsystem for linux distributions"
        || lower == "linux distributions"
    {
        return None;
    }
    let line = line.strip_prefix("* ").unwrap_or(line);
    // Parentheses can be part of a real name: only strip known default marks.
    let name = match (line.rfind('('), line.ends_with(')')) {
        (Some(i), true)
            if ["默认", "默认值", "default"]
                .iter()
                .any(|mark| line[i + 1..line.len() - 1].eq_ignore_ascii_case(mark)) =>
        {
            &line[..i]
        }
        _ => line,
    }
    .trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Blocking CLI scan: static + WSL. Also primes the GUI snapshot for CLI
/// bootstrap, before the message loop starts. UI code uses `cached_profiles`.
pub fn scan_all() -> Vec<Profile> {
    let result = enumerate_wsl();
    let mut state = cache().state.lock().unwrap_or_else(|e| e.into_inner());
    // A standalone CLI has no background worker. Do not overwrite an active
    // refresh if another non-UI caller happens to use this blocking API.
    if !state.refreshing {
        match &result {
            Ok(names) => {
                state.distributions = names.clone();
                state.next_refresh = Instant::now() + Duration::from_secs(60);
            }
            Err(_) => state.next_refresh = Instant::now() + Duration::from_secs(30),
        }
    }
    profiles_from_distributions(result.unwrap_or_default())
}

/// 子进程枚举(3s 超时;任何失败 → 空)。CREATE_NO_WINDOW 必须带:
/// 不带的裸 console 子进程在 defterm=WT 的 Win11 上被系统委托给
/// Windows Terminal 托管——GUI 启动时弹 WT 窗口(用户实报"双击弹三个
/// 窗口"之一),defterm 目标异常时则弹 0xC0000142。1.0.2 前的"M3c
/// 打磨票"定性完全错了,这是启动体验级 bug。
pub fn list_wsl_distributions() -> Vec<String> {
    enumerate_wsl().unwrap_or_default()
}

fn enumerate_wsl() -> io::Result<Vec<String>> {
    let mut cmd = Command::new("wsl.exe");
    cmd.args(["--list", "--all"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW(见函数 doc)
    }
    bounded_output(&mut cmd, Duration::from_secs(3), 64 * 1024)
        .map(|output| parse_wsl_output(&output))
}

/// Owns the child through every error path. No reader threads are started:
/// descendants inheriting stdout cannot leave an unjoinable blocked reader.
/// The deadline bounds execution/collection. Killing and reaping is performed
/// synchronously afterwards; OS termination latency is not a real-time bound.
struct EnumerationChild(Child);

impl Drop for EnumerationChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn bounded_output(cmd: &mut Command, timeout: Duration, limit: usize) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = EnumerationChild(cmd.spawn()?);
    let mut stdout = child.0.stdout.take().expect("piped stdout");
    prepare_stdout(&stdout)?;
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    let mut exited: Option<std::process::ExitStatus> = None;
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "WSL enumeration exceeded deadline",
            ));
        }
        let available = read_available(&mut stdout, &mut buffer)?;
        if available > 0 {
            if available > limit.saturating_sub(output.len()) {
                return Err(io::Error::other("WSL enumeration output exceeded limit"));
            }
            output.extend_from_slice(&buffer[..available]);
            continue; // recheck deadline even if the producer never stops
        }
        if let Some(status) = exited {
            return if status.success() {
                Ok(output)
            } else {
                Err(io::Error::other("WSL enumeration failed"))
            };
        }
        exited = child.0.try_wait()?;
        if exited.is_some() {
            continue; // drain bytes written between the peek and process exit
        }
        std::thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(windows)]
fn prepare_stdout(_: &ChildStdout) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn prepare_stdout(stdout: &ChildStdout) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: stdout owns a valid pipe FD; retain its other file flags.
    let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn read_available(stdout: &mut ChildStdout, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut available = 0;
    // SAFETY: ChildStdout owns the readable pipe; no other thread reads it.
    if unsafe {
        PeekNamedPipe(
            stdout.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
            Ok(0)
        } else {
            Err(error)
        };
    }
    let count = buffer.len().min(available as usize);
    if count == 0 {
        return Ok(0);
    }
    stdout.read(&mut buffer[..count])
}

#[cfg(unix)]
fn read_available(stdout: &mut ChildStdout, buffer: &mut [u8]) -> io::Result<usize> {
    match stdout.read(buffer) {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(0),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_distribution_names_keep_linux_words_spaces_and_parentheses() {
        assert_eq!(parse_wsl_output(b"Linux Distributions:\r\nkali-linux\r\nAlmaLinux\r\nUbuntu (Work)\r\nUbuntu Work (Default)\r\n"), ["kali-linux", "AlmaLinux", "Ubuntu (Work)", "Ubuntu Work"]);
    }

    #[test]
    fn enumeration_probe_child() {
        if let Some(path) = std::env::var_os("MICA_PROFILE_PROBE_PID_FILE") {
            std::fs::write(path, std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[cfg(windows)]
    #[test]
    fn enumeration_timeout_terminates_and_reaps_real_child() {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mica-enumeration-{}-{unique}.pid",
            std::process::id()
        ));
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "profile::tests::enumeration_probe_child",
            "--nocapture",
        ])
        .env("MICA_PROFILE_PROBE_PID_FILE", &path)
        .creation_flags(0x0800_0000);
        let result = bounded_output(&mut cmd, Duration::from_millis(500), 1024);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        let pid: u32 = std::fs::read_to_string(&path)
            .expect("child reached sleep before deadline")
            .parse()
            .unwrap();
        std::fs::remove_file(path).unwrap();
        // SAFETY: only query/wait on the exact probe PID; never terminate an
        // unrelated process. A closed PID or a signaled handle proves exit.
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if process.is_null() {
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(ERROR_INVALID_PARAMETER as i32)
            );
        } else {
            let state = unsafe { WaitForSingleObject(process, 0) };
            unsafe { CloseHandle(process) };
            assert_eq!(
                state, WAIT_OBJECT_0,
                "timed-out child must have exited before return"
            );
        }
    }

    #[test]
    fn successful_refresh_notifies_subscriber_and_coalesces_updates() {
        let cache = ProfileCache::new(|| Ok(vec!["Ubuntu With Space".into()]));
        let updates = cache.subscribe();
        cache.snapshot_and_refresh();
        updates
            .recv_timeout(Duration::from_secs(3))
            .expect("successful refresh must update Jump List");
        for _ in 0..3 {
            cache.state.lock().unwrap().next_refresh = Instant::now();
            assert_eq!(cache.find("invalid", Duration::from_secs(3)), Ok(None));
        }
        updates.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(
            updates.try_recv().is_err(),
            "notifications must coalesce rather than grow an unbounded queue"
        );
    }

    fn probe_command(script: &str) -> Command {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut cmd = Command::new("powershell.exe");
            cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
            cmd.creation_flags(0x0800_0000);
            cmd
        }
        #[cfg(not(windows))]
        {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script]);
            cmd
        }
    }

    #[test]
    fn enumeration_timeout_is_enforced_for_real_child() {
        #[cfg(windows)]
        let mut cmd = probe_command("Start-Sleep -Milliseconds 1500; Write-Output done");
        #[cfg(not(windows))]
        let mut cmd = probe_command("sleep 1.5; echo done");
        let started = Instant::now();
        let result = bounded_output(&mut cmd, Duration::from_millis(100), 1024);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "deadline must kill and reap the child promptly"
        );
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn enumeration_output_is_bounded() {
        #[cfg(windows)]
        let mut cmd = probe_command("[Console]::Write('x' * 8192)");
        #[cfg(not(windows))]
        let mut cmd = probe_command("printf '%8192s' x");
        let result = bounded_output(&mut cmd, Duration::from_secs(3), 1024);
        assert!(result.is_err(), "oversized output must be rejected");
    }

    #[test]
    fn cache_returns_builtin_profiles_without_waiting_and_refreshes_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let cache = ProfileCache::new(move || {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            release_rx.lock().unwrap().recv().unwrap();
            Ok(vec!["Ubuntu With Space".into()])
        });
        let (snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
        let first_cache = cache.clone();
        let first = std::thread::spawn(move || {
            snapshot_tx
                .send(first_cache.snapshot_and_refresh())
                .unwrap()
        });
        let snapshot = snapshot_rx.recv_timeout(Duration::from_secs(3));
        // Always release the probe, even if the implementation blocks.
        if snapshot.is_err() {
            release_tx.send(()).unwrap();
        }
        assert_eq!(
            snapshot.expect("UI snapshot waited for enumeration"),
            scan_static()
        );
        first.join().unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let cache = &cache;
                scope.spawn(move || {
                    for _ in 0..100 {
                        assert_eq!(cache.snapshot_and_refresh(), scan_static());
                    }
                });
            }
        });
        assert_eq!(
            cache.find("Ubuntu With Space", Duration::from_millis(20)),
            Err(ProfilePending)
        );
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let profiles = cache.snapshot_and_refresh();
            if profiles.len() == 3 {
                let argv = crate::pty::command_from_str(&profiles[2].command);
                assert_eq!(argv.get_argv()[2], "Ubuntu With Space");
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_refresh_keeps_previous_profiles_and_cools_down_retries() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let cache = ProfileCache::new(move || {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("injected enumeration failure"))
        });
        cache.state.lock().unwrap().distributions = vec!["Debian".into()];
        assert!(
            cache
                .snapshot_and_refresh()
                .iter()
                .any(|p| p.name == "Debian")
        );
        assert_eq!(cache.find("invalid", Duration::from_secs(3)), Ok(None));
        for _ in 0..100 {
            assert!(cache.find("debian", Duration::ZERO).unwrap().is_some());
            assert_eq!(cache.find("invalid", Duration::ZERO), Ok(None));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "invalid profile names must not force retries"
        );
        cache.state.lock().unwrap().next_refresh = Instant::now();
        assert_eq!(cache.find("invalid", Duration::from_secs(3)), Ok(None));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "retry becomes eligible only after cooldown"
        );
    }

    /// 真机字节(2026-10-01,中文系统):无 BOM、中文表头、"(默认)"后缀。
    const REAL_ZH: &[u8] = &[
        0x02, 0x90, 0x28, 0x75, 0x8e, 0x4e, 0x20, 0x00, 0x4c, 0x00, 0x69, 0x00, 0x6e, 0x00, 0x75,
        0x00, 0x78, 0x00, 0x20, 0x00, 0x84, 0x76, 0x20, 0x00, 0x57, 0x00, 0x69, 0x00, 0x6e, 0x00,
        0x64, 0x00, 0x6f, 0x00, 0x77, 0x00, 0x73, 0x00, 0x20, 0x00, 0x50, 0x5b, 0xfb, 0x7c, 0xdf,
        0x7e, 0x06, 0x52, 0xd1, 0x53, 0x3a, 0x00, 0x0d, 0x00, 0x0a, 0x00, 0x55, 0x00, 0x62, 0x00,
        0x75, 0x00, 0x6e, 0x00, 0x74, 0x00, 0x75, 0x00, 0x20, 0x00, 0x28, 0x00, 0xd8, 0x9e, 0xa4,
        0x8b, 0x3c, 0x50, 0x29, 0x00, 0x0d, 0x00, 0x0a, 0x00, 0x64, 0x00, 0x6f, 0x00, 0x63, 0x00,
        0x6b, 0x00, 0x65, 0x00, 0x72, 0x00, 0x2d, 0x00, 0x64, 0x00, 0x65, 0x00, 0x73, 0x00, 0x6b,
        0x00, 0x74, 0x00, 0x6f, 0x00, 0x70, 0x00, 0x0d, 0x00, 0x0a, 0x00,
    ];

    #[test]
    fn real_machine_chinese_output_parses() {
        assert_eq!(
            parse_wsl_output(REAL_ZH),
            vec!["Ubuntu".to_string(), "docker-desktop".to_string()],
            "中文表头跳过、(默认)后缀剥除"
        );
    }

    #[test]
    fn english_output_with_asterisk_default() {
        let text: Vec<u8> = "Windows Subsystem for Linux Distributions:\r\n\
             * Ubuntu-22.04\r\n\
             Debian\r\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(
            parse_wsl_output(&text),
            vec!["Ubuntu-22.04".to_string(), "Debian".to_string()],
            "* 前缀剥除"
        );
    }

    #[test]
    fn utf8_output_also_supported() {
        assert_eq!(
            parse_wsl_output(b"Linux Distributions:\r\nArch\r\n"),
            vec!["Arch".to_string()],
            "新 wsl 的 UTF-8 形态(无奇数位 NUL → 按UTF-8)"
        );
    }

    #[test]
    fn empty_and_garbage_degrade_silently() {
        assert_eq!(parse_wsl_output(&[]), Vec::<String>::new());
        assert_eq!(
            parse_wsl_output(b"\xff\xfe"),
            Vec::<String>::new(),
            "纯 BOM"
        );
        assert_eq!(
            parse_wsl_output(&[0x55, 0x00]), // 奇数截断的单 u16 "U"
            vec!["U".to_string()],
            "截断容忍"
        );
    }

    #[test]
    fn header_only_yields_empty() {
        let text: Vec<u8> = "Windows Subsystem for Linux Distributions:\r\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(parse_wsl_output(&text), Vec::<String>::new());
    }

    #[test]
    fn static_profiles_cover_both_shells() {
        let s = scan_static();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "PowerShell");
        assert!(s[1].command.starts_with("cmd.exe"));
    }

    #[test]
    fn scan_all_extends_static_with_wsl() {
        let all = scan_all();
        // 真机有 Ubuntu/docker-desktop;无 WSL 环境也能过(静态 2 项保底)
        assert!(all.len() >= 2);
        assert!(all.iter().any(|p| p.name == "PowerShell"));
    }
}
