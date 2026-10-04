//! M5b/T1:panic → 本地 minidump 落盘(永不网络上传——无任何网络调用)。
#![cfg(windows)]

use std::path::PathBuf;

#[test]
fn panic_test_writes_minidump() {
    // 记录调用前的既有档案,避免误把旧文件当新产物
    let dir = crash_dir();
    let before: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mica"))
        .arg("--panic-test")
        .env("MICA_CRASH_NOPROMPT", "1")
        .output()
        .expect("spawn mica");
    assert!(
        !out.status.success(),
        "--panic-test 应非零退出;stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let after: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    let new: Vec<_> = after
        .iter()
        .filter(|p| !before.contains(p) && p.extension().is_some_and(|e| e == "dmp"))
        .collect();
    assert!(
        !new.is_empty(),
        "应有新 .dmp 落盘(dir={:?},stderr={})",
        dir,
        String::from_utf8_lossy(&out.stderr)
    );
    let txt = new[0].with_extension("txt");
    let detail = std::fs::read_to_string(&txt).unwrap_or_default();
    assert!(
        detail.contains("--panic-test"),
        "panic 详情 .txt 应含 panic 信息,得 {detail:?}"
    );
}

fn crash_dir() -> std::path::PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("mica").join("crashes"))
        .unwrap_or_else(|| PathBuf::from("crashes"))
}

/// P2-3(1.0.9):SEH native 崩溃档案——--crash-native-test 触发 AV,
/// 断言 mica-native-*.txt 落盘(异常码 + 地址)。
#[test]
fn native_crash_test_writes_exception_info() {
    let dir = crash_dir();
    let before: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mica"))
        .arg("--crash-native-test")
        .env("MICA_CRASH_NOPROMPT", "1")
        .output()
        .expect("spawn mica");
    assert!(!out.status.success(), "--crash-native-test 应崩");
    let after: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    let txt = after
        .iter()
        .filter(|p| !before.contains(p) && p.extension().is_some_and(|e| e == "txt"))
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("mica-native"))
        });
    let Some(txt) = txt else {
        panic!(
            "应落盘 mica-native-*.txt;stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let info = std::fs::read_to_string(txt).unwrap_or_default();
    assert!(
        info.contains("native exception"),
        "txt 含异常信息,得 {info:?}"
    );
    assert!(
        info.contains("0xC0000005"),
        "access violation 码,得 {info:?}"
    );
}
