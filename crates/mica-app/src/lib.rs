//! Windows shell. All interesting code lives in `app`; non-Windows builds are
//! empty because the windowing APIs do not exist there.

#[cfg(windows)]
pub mod app; // tests 直入(UIA 集成测试要真窗口走真 WM_GETOBJECT)

#[cfg(windows)]
mod cli;

/// GUI 入口(main 无参数 / IPC 连不上时的自启动路径)。
#[cfg(windows)]
pub fn run() {
    app::run();
}

/// CLI 分发(M3a):返回 process exit code(main 使用)。
/// - 无参:先试 IPC activate(已有实例→退出 0),否则自启动
/// - `new-tab [profile]` / `wsl [distro]` / `list-profiles`
#[cfg(windows)]
pub fn handle_cli() -> i32 {
    use mica_core::ipc::IpcRequest;
    use std::sync::atomic::{AtomicU64, Ordering};
    static REQ_ID: AtomicU64 = AtomicU64::new(1);
    fn next_id() -> u64 {
        REQ_ID.fetch_add(1, Ordering::Relaxed)
    }
    // 崩溃档案(M5b/T1):任何后续 panic 都有 dmp——先于一切安装
    app::crash::install();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg != "--multi") {
        attach_cli_console();
    }
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("mica {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("--help" | "-h") => {
            println!(
                "mica [new-tab|new-window] [profile] [--cwd DIR] [--command CMD] [--env KEY=VALUE] [--font-size N] [--json]\n\
mica list [--json]\n\
mica focus (--pane=ID|--tab=N|--qt)\n\
mica send [--pane=ID|--tab=N|--qt] [--literal] [--] TEXT...\n\
mica read [--pane=ID|--tab=N|--qt] [--lines N] [--scrollback] [--json]\n\
mica badge [--pane=ID|--tab=N|--qt] --state none|idle|running|attention [--text TEXT] [--sticky]\n\
mica watch [--pane=ID] [--events exit,cwd,prompt,title,badge,lifecycle]\n\
mica wsl [distro] | list-profiles | +show-config --default | +validate-config"
            );
            0
        }
        Some("+show-config") => {
            let flags = &args[1..];
            if flags == ["--path"] {
                println!("{}", mica_core::config::user_config_path().display());
            } else if flags.iter().any(|s| s == "--default")
                && flags.iter().all(|s| s == "--default" || s == "--docs")
            {
                print!("{}", mica_core::config::defaults::SOURCE);
            } else {
                eprintln!("用法: mica +show-config --default [--docs] | --path");
                return 2;
            }
            0
        }
        Some("+validate-config") => {
            if args.len() != 1 {
                eprintln!("用法: mica +validate-config");
                return 2;
            }
            let path = mica_core::config::user_config_path();
            let source = match std::fs::read_to_string(&path) {
                Ok(source) => source,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => {
                    eprintln!("无法读取 {}: {e}", path.display());
                    return 1;
                }
            };
            match mica_core::config::settings::resolve(&source) {
                Ok(_) => {
                    println!(
                        "配置有效: {}{}",
                        path.display(),
                        if path.exists() {
                            ""
                        } else {
                            " (文件缺失,使用默认配置)"
                        }
                    );
                    0
                }
                Err(errors) => {
                    eprintln!("配置无效: {}", path.display());
                    for error in errors.parse {
                        eprintln!("  第 {} 行: {}", error.line, error.reason);
                    }
                    for error in errors.values {
                        eprintln!("  {error}");
                    }
                    1
                }
            }
        }
        // 隐藏自检:集成测试 spawn 断言 dmp 落盘(crash.rs 测试用)
        Some("--panic-test") => {
            panic!("--panic-test 自检");
        }
        // 隐藏自检:SEH native 崩溃档案(P2-3,1.0.9)
        Some("--crash-native-test") => {
            app::crash::trigger_native_crash();
            0 // 不会到达(AV 先死)
        }
        Some("list-profiles") => {
            for p in mica_core::profile::scan_all() {
                println!("{}", p.name);
            }
            0
        }
        Some("wsl") => {
            match args.get(1).cloned() {
                None => {
                    // 无参 wsl:列发行版(无 WSL 提示后退出)
                    let distros = mica_core::profile::list_wsl_distributions();
                    if distros.is_empty() {
                        eprintln!("无 WSL 发行版(wsl 未安装或未初始化)");
                    }
                    for d in distros {
                        println!("{d}");
                    }
                    0
                }
                Some(name) => {
                    // mica wsl <distro> → IPC new-tab(名字直接给,服务端
                    // scan_all 匹配;wsl distro 名本身就是 profile 名)
                    let mut req = IpcRequest::new(next_id(), "new-tab");
                    req.profile = Some(name);
                    dispatch_ipc(req)
                }
            }
        }
        Some("new-tab" | "new-window" | "list" | "focus" | "send" | "read" | "badge" | "watch") => {
            match cli::parse(&args, next_id()) {
                Ok(cli::Command::Control { request, json }) => run_control(*request, json),
                Ok(cli::Command::Watch { pane_id, events }) => {
                    match app::events::watch(pane_id, events.as_deref()) {
                        Ok(()) => 0,
                        Err(error) => {
                            eprintln!("mica watch: {}", error.code());
                            1
                        }
                    }
                }
                Err(error) => {
                    eprintln!("mica: {error}");
                    2
                }
            }
        }
        // 开发旗标:跳过单实例分流强制起独立 GUI(隔离调试用,2026-10-13
        // #38 排查落地——单实例门使第二实例永远量不到首窗行为)。
        // 必须排在 Some(other) 兜底之前
        Some("--multi") => {
            app::run();
            0
        }
        Some(other) => {
            eprintln!(
                "mica [new-tab [profile] | new-window | wsl [distro] | list-profiles]
未知命令:{other}"
            );
            2
        }
        None => dispatch_ipc(IpcRequest::new(next_id(), "activate")),
    }
}

/// Release uses the Windows GUI subsystem. Attach only for CLI commands and
/// preserve explicitly redirected streams, including JSON/NDJSON pipelines.
#[cfg(windows)]
fn attach_cli_console() {
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };
    let streams = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|kind| {
        let handle = unsafe { GetStdHandle(kind) }
            .ok()
            .filter(|h| !h.is_invalid());
        (kind, handle)
    });
    // Fails harmlessly if already attached or the parent has no console.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok() {
        for (kind, handle) in streams {
            if let Some(handle) = handle {
                unsafe {
                    let _ = SetStdHandle(kind, handle);
                }
            }
        }
    }
}

/// Compatibility path for activate and WSL profile launches.
#[cfg(windows)]
fn dispatch_ipc(request: mica_core::ipc::IpcRequest) -> i32 {
    run_control(request, false)
}

#[cfg(windows)]
fn run_control(mut request: mica_core::ipc::IpcRequest, json: bool) -> i32 {
    use mica_core::ipc::IpcError;
    if let Some(cwd) = &request.cwd {
        match std::path::absolute(cwd) {
            Ok(path) => request.cwd = Some(path.to_string_lossy().into_owned()),
            Err(error) => {
                eprintln!("mica: 无法解析启动目录: {error}");
                return 2;
            }
        }
    }
    let response = match app::ipc::send_request(&request, 3_000) {
        Ok(response) => Ok(response),
        Err(IpcError::NoInstance) if matches!(request.op.as_str(), "new-tab" | "new-window") => {
            start_instance(&request)
        }
        Err(IpcError::NoInstance) if request.op == "activate" => {
            app::run();
            return 0;
        }
        Err(error) => Err(error),
    };
    match response {
        Ok(response) if response.status == mica_core::ipc::IpcStatus::Error => {
            eprintln!(
                "mica {}: {}",
                request.op,
                response.detail.as_deref().unwrap_or("unknown")
            );
            1
        }
        Ok(response) => output_response(&request.op, response, json),
        Err(error) => {
            eprintln!("mica {}: {}", request.op, error.code());
            if matches!(error, IpcError::Timeout | IpcError::Disconnected) {
                eprintln!("请求可能已执行,请勿盲目重试");
            }
            1
        }
    }
}

/// A launch command returns when its first pane is ready, including --json.
/// Only NoInstance reaches this path; ambiguous transport failures never spawn.
#[cfg(windows)]
fn start_instance(
    request: &mica_core::ipc::IpcRequest,
) -> Result<mica_core::ipc::IpcResponse, mica_core::ipc::IpcError> {
    use mica_core::ipc::IpcError;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| IpcError::Transport)?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mica-start-{}-{stamp}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|_| IpcError::Transport)?;
    struct StartupFiles(std::path::PathBuf);
    impl Drop for StartupFiles {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0.join("response.json"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let files = StartupFiles(dir);
    let response_path = files.0.join("response.json");
    let child = spawn_gui(request, &response_path)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        // A collision loser writes its forwarded ACK and exits. Check liveness
        // before the file so a just-published ACK cannot be missed at exit.
        let exited = child.exited()?;
        if let Ok(bytes) = std::fs::read(&response_path)
            && let Ok(response) = serde_json::from_slice::<mica_core::ipc::IpcResponse>(&bytes)
        {
            if response.id != request.id {
                return Err(IpcError::BadJson);
            }
            return Ok(response);
        }
        if exited {
            return Err(IpcError::SpawnFailed);
        }
        if Instant::now() >= deadline {
            return Err(IpcError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Do not inherit arbitrary handles from the short-lived CLI: a pipe writer
/// retained by the GUI would keep JSON pipelines open until the window closed.
#[cfg(windows)]
struct GuiProcess(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Drop for GuiProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(windows)]
impl GuiProcess {
    fn exited(&self) -> Result<bool, mica_core::ipc::IpcError> {
        use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        match unsafe { windows::Win32::System::Threading::WaitForSingleObject(self.0, 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(mica_core::ipc::IpcError::Transport),
        }
    }
}

#[cfg(windows)]
fn spawn_gui(
    request: &mica_core::ipc::IpcRequest,
    response_path: &std::path::Path,
) -> Result<GuiProcess, mica_core::ipc::IpcError> {
    use mica_core::ipc::IpcError;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    };
    use windows::core::{PCWSTR, PWSTR};
    let executable = std::env::current_exe().map_err(|_| IpcError::Transport)?;
    let application: Vec<u16> = executable.as_os_str().encode_wide().chain([0]).collect();
    // Windows filenames cannot contain a quote; the sole argument is fixed.
    let mut command: Vec<u16> = std::iter::once(b'"' as u16)
        .chain(executable.as_os_str().encode_wide())
        .chain("\" --multi".encode_utf16())
        .chain([0])
        .collect();
    let mut vars: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            !key.eq_ignore_ascii_case("MICA_START_REQUEST")
                && !key.eq_ignore_ascii_case("MICA_START_RESPONSE")
        })
        .collect();
    vars.push((
        "MICA_START_REQUEST".into(),
        serde_json::to_string(request)
            .map_err(|_| IpcError::BadJson)?
            .into(),
    ));
    vars.push((
        "MICA_START_RESPONSE".into(),
        response_path.as_os_str().to_owned(),
    ));
    vars.sort_by_cached_key(|(key, _)| key.to_string_lossy().to_uppercase());
    let mut environment = Vec::<u16>::new();
    for (key, value) in vars {
        environment.extend(key.encode_wide());
        environment.push(b'=' as u16);
        environment.extend(value.encode_wide());
        environment.push(0);
    }
    environment.push(0);
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: writable command, double-NUL Unicode env and startup buffers live
    // through this call. Child inherits zero handles and creates no console.
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_UNICODE_ENVIRONMENT,
            Some(environment.as_ptr().cast()),
            PCWSTR::null(),
            &startup,
            &mut process,
        )
        .map_err(|_| IpcError::SpawnFailed)?;
        let _ = windows::Win32::Foundation::CloseHandle(process.hThread);
    }
    Ok(GuiProcess(process.hProcess))
}

#[cfg(windows)]
fn output_response(op: &str, response: mica_core::ipc::IpcResponse, json: bool) -> i32 {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let result =
        if json {
            writeln!(
                stdout,
                "{}",
                response.data.unwrap_or(serde_json::Value::Null)
            )
        } else if op == "read-text" {
            match response.data.and_then(|data| {
                serde_json::from_value::<mica_core::ipc::ReadTextResult>(data).ok()
            }) {
                Some(snapshot) => stdout.write_all(snapshot.text.as_bytes()),
                None => {
                    eprintln!("mica read: invalid response");
                    return 1;
                }
            }
        } else if op == "list" {
            let Some(panes) = response.data.and_then(|data| {
                serde_json::from_value::<Vec<mica_core::ipc::PaneInfo>>(data).ok()
            }) else {
                eprintln!("mica list: invalid response");
                return 1;
            };
            fn clean(value: &str) -> String {
                value
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect()
            }
            let mut table = String::from("PANE\tWINDOW\tTAB\tPID\tSTATE\tCWD\tTITLE\n");
            for pane in panes {
                let state = match pane.prompt_state {
                    mica_core::ipc::PromptState::Idle => "idle",
                    mica_core::ipc::PromptState::Running => "running",
                    mica_core::ipc::PromptState::Unknown => "unknown",
                };
                use std::fmt::Write as _;
                let _ = writeln!(
                    table,
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    pane.pane_id,
                    pane.window_id,
                    pane.tab_index,
                    pane.pid
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "-".into()),
                    state,
                    clean(pane.cwd.as_deref().unwrap_or("-")),
                    clean(&pane.title)
                );
            }
            stdout.write_all(table.as_bytes())
        } else {
            Ok(())
        };
    if result.is_err() {
        return 1;
    }
    0
}

#[cfg(not(windows))]
pub fn run() {
    eprintln!("mica-app only runs on Windows.");
}

#[cfg(not(windows))]
pub fn handle_cli() -> i32 {
    eprintln!("mica-app only runs on Windows.");
    1
}
