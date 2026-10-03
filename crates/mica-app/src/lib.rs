//! Windows shell. All interesting code lives in `app`; non-Windows builds are
//! empty because the windowing APIs do not exist there.

#[cfg(windows)]
mod app;

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
    use mica_core::ipc::IpcMessage;
    // 崩溃档案(M5b/T1):任何后续 panic 都有 dmp——先于一切安装
    app::crash::install();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // 隐藏自检:集成测试 spawn 断言 dmp 落盘(crash.rs 测试用)
        Some("--panic-test") => {
            panic!("--panic-test 自检");
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
                    dispatch_ipc(IpcMessage::new_tab(Some(name)), String::new())
                }
            }
        }
        Some("new-tab") => {
            // mica new-tab [profile] [--font-size N](T9 每标签字号)
            let mut profile = None;
            let mut font_size = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--font-size" | "-f" => {
                        if let Some(v) = args.get(i + 1).and_then(|a| a.parse::<f32>().ok()) {
                            font_size = Some(v);
                            i += 1;
                        }
                    }
                    _ if profile.is_none() => profile = args.get(i).cloned(),
                    _ => {}
                }
                i += 1;
            }
            dispatch_ipc(
                IpcMessage::new_tab_with(profile, font_size),
                "powershell.exe -NoLogo".into(),
            )
        }
        Some(other) => {
            eprintln!(
                "mica [new-tab [profile] | wsl [distro] | list-profiles]
未知命令:{other}"
            );
            2
        }
        None => dispatch_ipc(IpcMessage::activate(), "powershell.exe -NoLogo".into()),
    }
}

/// IPC 分发核心:连得上→发帧退出;连不上→有 profile 就带环境变量自启动,
/// 无 profile 就纯 GUI 启动。
#[cfg(windows)]
fn dispatch_ipc(msg: mica_core::ipc::IpcMessage, fallback_shell: String) -> i32 {
    if app::ipc::send(&msg, 3_000).is_ok() {
        return 0; // 已有实例受理
    }
    // 连不上:自启动(GUI);profile 经环境变量带给 run()(比改 run 签名轻;
    // 启动后即读即清,后续标签不受影响)。set_var 在多线程前调用(单线程 CLI)安全。
    let _ = &fallback_shell;
    if let Some(p) = &msg.profile {
        unsafe { std::env::set_var("MICA_START_PROFILE", p) };
    }
    app::run();
    0
}

#[cfg(not(windows))]
pub fn run() {
    eprintln!("mica-app only runs on Windows.");
}

#[cfg(not(windows))]
pub fn handle_cli() -> i32 {
    eprintln!("mica-app only runs on Windows.");
    1;
    unreachable!()
}
