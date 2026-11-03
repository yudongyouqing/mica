//! T08 三次返工 · 复核 3:WM_TIMER 输入续灌的**真实 GUI 接线**证明。
//! 静默 shell(Start-Sleep,不读 stdin)+ 1.2MB IPC 注入 → 欠账 +
//! 定时器武装 → MICA_DEBUG_INPUTFLOW 下 stderr 出现 timer-tick 日志
//! (带 pane 计数)→ 后台标签同样覆盖(向 --tab=0 注入而活跃是 1)。
//! 绝不在测试里手工 flush。

#![cfg(windows)]

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
fn guard() -> &'static Mutex<()> {
    GUARD.get_or_init(|| Mutex::new(()))
}

static TEST_USER: OnceLock<Mutex<String>> = OnceLock::new();
fn test_user() -> String {
    TEST_USER
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().map(|s| s.is_none()).unwrap_or(false) {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn cli(args: &[&str]) -> bool {
    Command::new(env!("CARGO_BIN_EXE_mica"))
        .args(args)
        .env("USERNAME", test_user())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn wait_ready(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cli(&["send", "--literal", "x"]) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// 读尽 stderr 文件(轮询期间 app 持续写入)。
fn slurp(path: &std::path::Path) -> String {
    let mut buf = String::new();
    if let Ok(mut f) = std::fs::File::open(path) {
        let _ = f.read_to_string(&mut buf);
    }
    buf
}

/// 静默 shell 下的大输入:欠账产生 → WM_TIMER 真接线推进(后台 pane
/// 覆盖)。判定:stderr 出现 `[inputflow] timer-tick`(MICA_DEBUG_INPUTFLOW
/// 门控的生产日志,只在 flush_all_panes 被真实调用时打印——即
/// WM_TIMER id 8 分支真的接通)。
#[test]
fn wm_timer_flush_is_really_wired_silent_shell() {
    let _g = guard().lock().unwrap_or_else(|e| e.into_inner());
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let iso = std::env::temp_dir().join(format!("mica-inflow-{unique}"));
    let cfg_dir = iso.join("mica");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    // 静默 shell:Start-Sleep 期间不读 stdin(输入通道不排水)
    std::fs::write(
        cfg_dir.join("config"),
        "shell = powershell.exe -NoProfile -Command Start-Sleep 600",
    )
    .unwrap();
    let err_path = iso.join("stderr.log");
    let user = format!("micainflow{unique}");
    *TEST_USER
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = user.clone();

    let app = Command::new(env!("CARGO_BIN_EXE_mica"))
        .arg("--multi")
        .env("APPDATA", &iso)
        .env("LOCALAPPDATA", &iso)
        .env("USERNAME", &user)
        .env("MICA_DEBUG_INPUTFLOW", "1")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .expect("launch mica");
    let app = ChildGuard(app);

    assert!(wait_ready(Duration::from_secs(30)), "实例就绪");
    // 开第二个标签(让 --tab=0 成为后台 pane)
    assert!(cli(&["new-tab"]), "开副标签");
    std::thread::sleep(Duration::from_millis(1500));

    // **确定性 Debt 注入**:挂起全部 shell 子进程(NtSuspendProcess)——
    // 挂起的 shell 不读 stdin → ConPTY 输入管顶住 → writer 阻塞 →
    // 写队列填满 → Debt → arm → OS 投递 WM_TIMER(8)。只动自己
    // 创建的进程树。
    // 脚本文件而非内联命令:PS 一行式引号嵌套在本路径证明过脆弱
    let ps1 = iso.join("suspend.ps1");
    std::fs::write(
        &ps1,
        r#"param([int]$RootPid)
Add-Type -Name Nt -Namespace W -MemberDefinition '[DllImport("ntdll.dll")] public static extern int NtSuspendProcess(IntPtr h);'
$all = Get-CimInstance Win32_Process
$frontier = @($RootPid)
$seen = New-Object System.Collections.Generic.HashSet[int]
$sus = 0
while ($frontier.Count -gt 0) {
  $next = @()
  foreach ($pp in $frontier) {
    foreach ($c in ($all | Where-Object { $_.ParentProcessId -eq $pp })) {
      if ($seen.Add([int]$c.ProcessId)) { $next += [int]$c.ProcessId }
    }
  }
  $frontier = $next
}
foreach ($pid2 in ($seen | Where-Object { $_ -ne 0 })) {
  $p = Get-Process -Id $pid2 -ErrorAction SilentlyContinue
  if ($p) {
    [W.Nt]::NtSuspendProcess($p.Handle) | Out-Null
    $sus++
    Write-Output "suspended=$($p.Id)"
  }
}
if ($sus -eq 0) { Write-Output "none" }
"#,
    )
    .expect("write suspend.ps1");
    let suspend_out = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&ps1)
        .arg(app.0.id().to_string())
        .output()
        .expect("suspend helper");
    let suspended = String::from_utf8_lossy(&suspend_out.stdout);
    assert!(
        suspended.contains("suspended="),
        "至少挂起一个 shell(输出:{suspended};err:{})",
        String::from_utf8_lossy(&suspend_out.stderr)
    );

    // 灌超过 写队列(1MiB)+ 暂存(512KiB)的总量:命令行单参有
    // ~32KB 上限,分 26 次 60KB 连发(不 sleep——灌入速率须超过
    // shell 排水速率才能形成 Debt → arm_input_flush_timer → OS 30ms
    // 内投递 WM_TIMER(8) → 生产日志)。至少一次 Debt 即武装。
    let chunk = "x".repeat(24 * 1024); // Windows 命令行 ~32KB 上限
    let mut timeouts = 0;
    for _ in 0..70 {
        if !cli(&["send", "--tab=0", "--literal", chunk.as_str()]) {
            timeouts += 1; // drain_into 期限 → timeout(字节已接收为欠账)
            break; // 通道饱和,无需再灌
        }
    }

    // 等待 WM_TIMER tick 出现(30ms 节拍,最多 8s)
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut ticks = 0usize;
    while Instant::now() < deadline {
        let log = slurp(&err_path);
        ticks = log.matches("[inputflow] timer-tick").count();
        if ticks >= 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        ticks >= 3,
        "WM_TIMER id 8 必须真实接通(饱和注入 → arm → OS 投递 → timer-tick 生产日志,得 {ticks} 次;timeouts={timeouts})"
    );
    // 日志应带 pane 计数(后台 pane 覆盖)
    let log = slurp(&err_path);
    assert!(
        log.contains("debt_panes="),
        "tick 日志含 pane 计数(后台覆盖证据)"
    );
}
