//! 孤儿清杀的真进程集成回归:只用测试自己创建的进程,绝不触碰系统里
//! 其他任何进程。覆盖:dry-run 零副作用、真实清杀、存活 owner 不动、
//! **登记宿主端到端清杀**、**执行器身份核验**(expect_start 不符绝不击杀)。

#![cfg(windows)]

use std::process::{Command, Stdio};

use mica_app::app::orphan::{self, KillStep, Registry, SessionRecord, Terminator, WinTerminator};
use mica_core::pty::PtySession;

fn temp_registry(tag: &str) -> Registry {
    let dir = std::env::temp_dir().join(format!(
        "mica-orphan-int-{}-{tag}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    Registry::new(dir.join("sessions.txt"))
}

/// 拿一个"旧 owner 已终结"的 PID(等速死进程退出并关闭句柄;并行测试
/// 的 spawn 风暴会立刻复用刚释放的 PID,强求不可复用只会 flaky——计划
/// 器对复用 owner 按身份处理,语义同样正确)。
fn dead_pid() -> u32 {
    let mut cmd = Command::new("cmd.exe")
        .arg("/c")
        .arg("exit 0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn short-lived cmd");
    let pid = cmd.id();
    let _ = cmd.wait();
    pid
}

/// 长活 sleeper:powershell Start-Sleep(单进程,无子进程)。
fn sleeper() -> std::process::Child {
    Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 120"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleeper")
}

/// host 身份未知的记录(旧版形态:只处置 child)。
fn legacy_style_record(mica_pid: u32, child_pid: u32, child_start: u64) -> SessionRecord {
    SessionRecord {
        mica_pid,
        mica_start: 1,
        child_pid,
        child_start,
        host_pid: 0,
        host_start: 0,
        state: orphan::RecordState::Running,
    }
}

/// dry-run:计划里有真孤儿,但真实进程毫发无损,注册表不被改写。
#[test]
fn dry_run_leaves_real_processes_and_registry_untouched() {
    let reg = temp_registry("dry");
    let mut child = sleeper();
    let pid = child.id();
    let start = orphan::query_creation(pid).expect("sleeper creation time");
    let rec = legacy_style_record(dead_pid(), pid, start);
    reg.track(rec);

    let killed = orphan::sweep_owned(&reg, false);
    assert_eq!(killed, 0, "dry-run 不得终止任何进程");
    assert!(
        child.try_wait().unwrap().is_none(),
        "dry-run 后 sleeper 必须仍存活"
    );
    assert_eq!(reg.records(), vec![rec], "dry-run 不得改写注册表");

    let _ = child.kill();
    let _ = child.wait();
}

/// 真实清杀:owner 已死的孤儿 child 被终止,记录被清。
#[test]
fn real_sweep_terminates_owned_orphan_only() {
    let reg = temp_registry("real");
    let mut child = sleeper();
    let pid = child.id();
    let start = orphan::query_creation(pid).expect("sleeper creation time");
    reg.track(legacy_style_record(dead_pid(), pid, start));

    let killed = orphan::sweep_owned(&reg, true);
    assert!(killed >= 1, "应至少终止孤儿 child");
    let _ = child.wait();
    assert!(reg.records().is_empty(), "击杀成功的记录应移除");
}

/// 存活 owner 的会话绝不清杀,记录保留。
#[test]
fn live_owner_session_is_never_swept() {
    let reg = temp_registry("live-owner");
    let mut child = sleeper();
    let pid = child.id();
    let start = orphan::query_creation(pid).expect("sleeper creation time");
    let me = std::process::id();
    let my_start = orphan::query_creation(me).expect("self creation time");
    let rec = SessionRecord {
        mica_pid: me,
        mica_start: my_start,
        child_pid: pid,
        child_start: start,
        host_pid: 0,
        host_start: 0,
        state: orphan::RecordState::Running,
    };
    reg.track(rec);

    assert_eq!(orphan::sweep_owned(&reg, true), 0, "存活实例的会话绝不清杀");
    assert!(child.try_wait().unwrap().is_none());
    assert_eq!(reg.records(), vec![rec], "记录保留");
    let _ = child.kill();
    let _ = child.wait();
}

/// **端到端**:SpawnProbe 差分登记的宿主身份,在 owner 死亡后被 sweep
/// 连同 child 一起清掉(真实 ConPTY 会话,非 mock)。模拟强杀形态:
/// master/reader 句柄**泄漏不 Drop**(forget)——干净 Drop 会正常关闭
/// conhost,那不是孤儿场景;强杀后管道保持打开,conhost 才会残留。
#[test]
fn registered_host_is_swept_end_to_end() {
    let reg = temp_registry("host-e2e");
    // 真 pty:probe 在 spawn 前取,host_identity 差分出真实 conhost
    let probe = orphan::before_spawn();
    let (session, reader) =
        PtySession::spawn(mica_core::pty::command_from_str("cmd.exe"), 80, 24).expect("pty");
    let child_pid = session.child_pid().expect("child pid");
    let child_start = orphan::query_creation(child_pid).expect("child creation");
    let (host_pid, host_start) = probe.host_identity().expect("差分出宿主身份");
    assert_ne!(host_pid, 0, "真会话必须有宿主身份");
    assert!(
        orphan::query_creation(host_pid).is_some(),
        "登记宿主此刻必须存活"
    );
    // 强杀形态:句柄泄漏(master 不关 → conhost 不退),child 留活给 sweep
    std::mem::forget(session);
    std::mem::forget(reader);
    // owner 记为已死进程:构成孤儿;child 与宿主身份都是真实的
    reg.track(SessionRecord {
        mica_pid: dead_pid(),
        mica_start: 1,
        child_pid,
        child_start,
        host_pid,
        host_start,
        state: orphan::RecordState::Running,
    });

    let killed = orphan::sweep_owned(&reg, true);
    assert!(killed >= 2, "child 与宿主都应清杀(得 {killed})");
    // 终止异步落定,轮询退出码(注意:本测试故意泄漏的句柄让 PID 在
    // OpenProcess 后仍可查——判定死活必须看退出码而非"能否打开",
    // 这正是 dry-run/清杀身份核验所依赖的 Windows 语义)
    const STILL_ACTIVE: u32 = 259;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let dead = |pid: u32| orphan::query_exit_code(pid).is_some_and(|c| c != STILL_ACTIVE);
    while !(dead(host_pid) && dead(child_pid)) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(dead(host_pid), "登记宿主必须被清杀");
    assert!(dead(child_pid), "孤儿 child 必须被清杀");
    assert!(reg.records().is_empty(), "清杀成功后记录移除");
}

/// **执行器身份核验**:expect_start 与真实创建时间不符(PID 已易主)时,
/// 同一句柄上核验失败,绝不 TerminateProcess——真实进程存活。
#[test]
fn identity_mismatch_never_terminates() {
    let mut child = sleeper();
    let pid = child.id();
    let real_start = orphan::query_creation(pid).expect("creation");
    let wrong_start = real_start ^ 1; // 恒不等

    let mut ex = WinTerminator;
    assert!(
        !ex.terminate(&KillStep {
            pid,
            expect_start: wrong_start,
            why: "test mismatch"
        }),
        "身份不符必须返回失败"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "身份不符绝不能终止真实进程"
    );

    // 对照:身份一致时击杀成功(顺序:先验不符,再验相符)
    assert!(
        ex.terminate(&KillStep {
            pid,
            expect_start: real_start,
            why: "test match"
        }),
        "身份一致应击杀成功"
    );
    let _ = child.wait();
}
