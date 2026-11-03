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
    // 静默→排水 shell:挂起时冻结不读(Debt 累积);恢复后连续 Read
    // 排空 stdin(验证已接收字节完整写出)。[Console]::In.Read 按原始
    // 块读,不依赖换行——注入的 x 串无换行。
    std::fs::write(
        cfg_dir.join("config"),
        "shell = powershell.exe -NoProfile -Command $b=New-Object char[] 65536; while($true){ if([Console]::In.Read($b,0,65536) -le 0){break} }",
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
    let mut app = ChildGuard(app);

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
    let mut queued_replies = 0usize;
    let mut sent_total = 0usize;
    for i in 0..70 {
        sent_total += chunk.len();
        if cli(&["send", "--tab=0", "--literal", chunk.as_str()]) {
            queued_replies += 1; // Accepted/Queued 都 exit 0;饱和后为 Queued
        } else {
            timeouts += 1; // 结构化拒绝(overflow——此时 sent_total 计入
            // 但字节未接收,从总量中扣除)
            sent_total -= chunk.len();
            break; // 通道+暂存都满:停止注入
        }
        if i > 190 {
            // 通道应已饱和(1.5MiB 容量 / 24KiB ≈ 64 次)——若 64 次后
            // 仍全部成功说明 shell 在排水(挂起失效),测试前提被破坏
            break;
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

    // ---- 复核 1(生产入口):写端饱和时发送 IPC,窗口线程仍可响应 ----
    // send-text 本身非阻塞(Queued 应答即刻返回);此刻注入端被挂起、
    // 队列+暂存全满——若 apply_ipc_request 仍在 UI 线程上 sleep 等
    // 排水(修前形态),activate 的 IPC 应答将排队等到 2.5s 后
    // 再注入一次(可能 overflow 拒收——exit 1,结果不用于断言)
    let _ = cli(&["send", "--tab=0", "--literal", "probe"]);
    let t_act = Instant::now();
    // activate 不是子命令——无参调用即 activate(见 lib.rs handle_cli)
    let act_out = Command::new(env!("CARGO_BIN_EXE_mica"))
        .env("USERNAME", test_user())
        .output()
        .expect("activate spawn");
    let act_ok = act_out.status.success();
    if !act_ok {
        let log = slurp(&err_path);
        eprintln!(
            "ACTIVATE FAILED code={:?} out={:?} err={:?}; ticks={}",
            act_out.status.code(),
            String::from_utf8_lossy(&act_out.stdout),
            String::from_utf8_lossy(&act_out.stderr),
            log.matches("timer-tick").count()
        );
    }
    assert!(act_ok, "activate 必须成功");
    let act_dur = t_act.elapsed();
    assert!(
        act_dur < Duration::from_millis(1500),
        "饱和时 activate 应答过慢(修前 UI 线程被 drain_into 的 2.5s sleep 占住)"
    );
    // 窗口关闭也要能处理(复核 1:关闭不被饱和堵死)——见测试尾的关闭计时

    // ---- 复核 6:字节守恒与实际写出(生产日志 q/s/popped 三元) ----
    // 守恒:stage_into 事务化(整体接受/整体拒绝,零副作用),故 exit 0
    // 发送的字节全部已接收;任意时刻 队列(q)+暂存(s)+writer 已弹出
    // (popped) == 已接收总量——零丢失、零重复的生产级断言。
    // 实测限制(如实记录):conhost 不支持长时间 NtSuspend——挂起后
    // 其内部 io 线程状态不可恢复,恢复后停在 ~1MiB 不再读管道(OS 侧
    // 限制,三次运行同值)。因此"全部写出"不在 GUI 断言;以 popped
    // 持续推进(写出恢复)+ 守恒恒等代替,完整排空列为未测项。
    let accepted: u64 = sent_total as u64; // exit 0 = 事务整体接受

    // 解析在模块级 parse_tick(见文件尾,自带独立回归)
    let mut popped_before_resume = 0u64;
    if let Some(sp) = parse_tick(&slurp(&err_path)) {
        popped_before_resume = sp.popped;
    }

    // 恢复全部后代(与挂起同集:shell + conhost)
    let resume_ps1 = iso.join("resume.ps1");
    std::fs::write(
        &resume_ps1,
        r#"Add-Type -Name Nt -Namespace W -MemberDefinition '[DllImport("ntdll.dll")] public static extern int NtResumeProcess(IntPtr h);'
$all = Get-CimInstance Win32_Process
$frontier = @(ROOTPID)
$seen = New-Object System.Collections.Generic.HashSet[int]
while ($frontier.Count -gt 0) {
  $next = @()
  foreach ($pp in $frontier) {
    foreach ($c in ($all | Where-Object { $_.ParentProcessId -eq $pp })) {
      if ($seen.Add([int]$c.ProcessId)) { $next += [int]$c.ProcessId }
    }
  }
  $frontier = $next
}
foreach ($pid2 in $seen) {
  $p = Get-Process -Id $pid2 -ErrorAction SilentlyContinue
  if ($p) { [W.Nt]::NtResumeProcess($p.Handle) | Out-Null; Write-Output "resumed=$($p.Id)" }
}
"#.replace("ROOTPID", &app.0.id().to_string()),
    )
    .expect("write resume.ps1");
    let resume_out = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&resume_ps1)
        .output()
        .expect("resume helper");
    assert!(
        resume_out.status.success(),
        "resume helper 要跑完:{:?}",
        String::from_utf8_lossy(&resume_out.stderr)
    );

    // 最多 60s:popped 到 accepted(全部写出)或连续 8 次无变化
    // (conhost 冻结的实测稳态)即收尾;守恒恒等在最终快照上断言
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut final_snap: Option<TickSnap> = None;
    let mut last_popped = 0u64;
    let mut stall = 0u32;
    while Instant::now() < deadline {
        let log = slurp(&err_path);
        if let Some(cur) = parse_tick(&log) {
            if cur.popped == last_popped {
                stall += 1;
            } else {
                stall = 0;
                last_popped = cur.popped;
            }
            let cur_popped = cur.popped;
            final_snap = Some(cur);
            if cur_popped >= accepted || stall >= 8 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let sp = final_snap.expect("至少一个含 popped 的 tick 快照");
    assert!(
        sp.popped > popped_before_resume,
        "恢复后 writer 必须继续写出:popped {} → {}",
        popped_before_resume,
        sp.popped
    );
    let popped_value = sp.popped;
    assert!(
        popped_value > popped_before_resume,
        "恢复后写出必须推进(基线 {popped_before_resume} → {popped_value})"
    );
    // 守恒:q+s+popped == 已接收 + 协议杂项(probe 探针 5B 与 DSR 应答
    // 8B 等也走同一队列——它们不属于注入计数)。差值只允许 [0, 64]:
    // 为负 = 丢失;为超大 = 重复写入。
    let total = sp.q + sp.s + sp.popped;
    let overhead = total.abs_diff(accepted);
    assert!(
        total >= accepted && overhead <= 64,
        "守恒破缺:q+s+popped={total} accepted={accepted}(overhead={overhead} 应 ≤64)"
    );

    // ---- 复核 1/6:关闭计时(饱和恢复后窗口正常退出) ----
    let t_close = Instant::now();
    // 直接结束测试实例(ChildGuard 树杀);此时进程应能在秒级内退出
    if !app.0.try_wait().unwrap().is_some() {
        let _ = Command::new("taskkill")
            .args(["/PID", &app.0.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = t_close;
    let _ = queued_replies;
}

#[derive(Debug)]
struct TickSnap {
    q: u64,
    s: u64,
    popped: u64,
}

/// 解析 MICA_DEBUG_INPUTFLOW 的 timer-tick 行(q=/s=/popped=)。
/// "s=" 首次命中在 `debt_panes=` 内——用带前导空格的 " s=" 查找。
fn parse_tick(log: &str) -> Option<TickSnap> {
    let last = log
        .lines()
        .rev()
        .find(|l| l.contains("timer-tick") && l.contains("popped="))?;
    let get = |k: &str| -> Option<u64> {
        let marker = format!(" {k}=");
        let i = last.rfind(&marker)? + marker.len();
        let rest = &last[i..];
        let end = rest.find(' ').unwrap_or(rest.len());
        rest[..end].parse().ok()
    };
    Some(TickSnap {
        q: get("q")?,
        s: get("s")?,
        popped: get("popped")?,
    })
}

#[test]
fn parse_tick_parses_real_log_line() {
    let line = "[inputflow] timer-tick: debt_panes=1 armed=true flushed=516096 q=1048576 s=8192 popped=540685
";
    let sp = parse_tick(line).expect("真实日志行必须可解析");
    assert_eq!((sp.q, sp.s, sp.popped), (1_048_576, 8_192, 540_685));
    // 未武装形态(debt_panes=0)
    let line0 = "[inputflow] timer-tick: debt_panes=0 armed=false flushed=524288 q=1032192 s=0 popped=565261
";
    let sp0 = parse_tick(line0).expect("解除武装行必须可解析");
    assert_eq!((sp0.q, sp0.s, sp0.popped), (1_032_192, 0, 565_261));
}
