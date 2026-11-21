//! M5b/T1:panic → 本地 minidump 落盘(永不网络上传——无任何网络调用)。
#![cfg(windows)]

use std::path::PathBuf;

struct TempLocalAppData(PathBuf);

impl TempLocalAppData {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "mica-crash-test-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn crash_dir(&self) -> PathBuf {
        self.0.join("mica").join("crashes")
    }
}

impl Drop for TempLocalAppData {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn panic_test_writes_minidump() {
    let iso = TempLocalAppData::new("panic");
    let dir = iso.crash_dir();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mica"))
        .arg("--panic-test")
        .env("LOCALAPPDATA", &iso.0)
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
        .filter(|p| p.extension().is_some_and(|e| e == "dmp"))
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

/// P2-3(1.0.9):SEH native 崩溃档案——--crash-native-test 触发 AV,
/// 断言 mica-native-*.txt 落盘(异常码 + 地址)。
#[test]
fn native_crash_test_writes_exception_info() {
    let iso = TempLocalAppData::new("native");
    let dir = iso.crash_dir();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mica"))
        .arg("--crash-native-test")
        .env("LOCALAPPDATA", &iso.0)
        .env("MICA_CRASH_NOPROMPT", "1")
        .output()
        .expect("spawn mica");
    assert!(!out.status.success(), "--crash-native-test 应崩");
    let after: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    let txt = after
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
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
