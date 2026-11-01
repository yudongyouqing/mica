//! T02 孤儿清杀的真进程集成回归:只用测试自己创建的进程,绝不触碰
//! 系统里其他任何进程(dry-run 与真实终止两个面都要锁)。

#![cfg(windows)]

use std::process::{Command, Stdio};

use mica_app::app::orphan::{self, Registry, SessionRecord};

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

/// 拿一个"旧 owner 已终结"的 PID:起速死进程等它退出并关闭全部句柄。
/// 不要求 PID 此刻未被复用——若被新进程占用,计划器按"复用者"路径
/// 处理(创建时间不匹配 → 旧 owner 判死 + 宿主上界约束),语义同样正确;
/// 并行测试的 spawn 风暴会立刻复用刚释放的 PID,强求 None 只会 flaky。
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

/// dry-run(execute=false):注册表指向活 sleeper,计划里应有它,但真实
/// 进程必须毫发无损,注册表也不被改写。
#[test]
fn dry_run_leaves_real_processes_and_registry_untouched() {
    let reg = temp_registry("dry");
    let mut child = sleeper();
    let pid = child.id();
    let start = orphan::query_creation(pid).expect("sleeper creation time");
    let rec = SessionRecord {
        mica_pid: dead_pid(),
        mica_start: start.saturating_sub(1),
        child_pid: pid,
        child_start: start,
    };
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

/// 真实清杀(execute=true):owner 已死的 sleeper 被终止,记录被清。
#[test]
fn real_sweep_terminates_owned_orphan_only() {
    let reg = temp_registry("real");
    let mut child = sleeper();
    let pid = child.id();
    let start = orphan::query_creation(pid).expect("sleeper creation time");
    reg.track(SessionRecord {
        mica_pid: dead_pid(),
        mica_start: start.saturating_sub(1),
        child_pid: pid,
        child_start: start,
    });

    let killed = orphan::sweep_owned(&reg, true);
    assert!(killed >= 1, "应至少终止孤儿 sleeper");
    // 等它退出(TerminateProcess 异步落定)
    let _ = child.wait();
    assert!(reg.records().is_empty(), "已处置的记录应从注册表清除");
}

/// owner(本测试进程)活着:同 registry 里的活会话绝不清杀。
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
    };
    reg.track(rec);

    let killed = orphan::sweep_owned(&reg, true);
    assert_eq!(killed, 0, "存活实例的会话绝不清杀");
    assert!(child.try_wait().unwrap().is_none());
    assert_eq!(reg.records(), vec![rec], "记录保留");

    let _ = child.kill();
    let _ = child.wait();
}
