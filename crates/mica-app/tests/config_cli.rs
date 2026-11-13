//! Configuration commands must finish without starting a GUI or shell.
#![cfg(windows)]

#[path = "support/process.rs"]
mod process;

use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn cli(args: &[&str], source: Option<&str>) -> (Output, std::path::PathBuf) {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("mica-config-cli-{id}"));
    let path = root.join("mica").join("config");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    if let Some(source) = source {
        std::fs::write(&path, source).unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_mica"));
    command
        .args(args)
        .env("APPDATA", &root)
        .env("LOCALAPPDATA", &root)
        .env("USERNAME", format!("micacfgcli{id}"));
    let output =
        process::run_with_deadline(&mut command, Duration::from_secs(8), &root.join("logs"))
            .expect("configuration CLI must terminate without opening a GUI");
    (output, path)
}

#[test]
fn exports_authoritative_defaults_and_personal_path() {
    let (output, _) = cli(&["+show-config", "--default"], None);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        mica_core::config::defaults::SOURCE
    );
    let (output, path) = cli(&["+show-config", "--path"], None);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        path.to_string_lossy()
    );
    let (output, _) = cli(&["+show-config", "--unsupported"], None);
    assert!(!output.status.success());
}

#[test]
fn validates_bom_and_reports_invalid_values_without_starting_a_session() {
    let (output, _) = cli(
        &["+validate-config"],
        Some("\u{feff}font-size = 16\ncopy-on-select = false\n"),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (output, _) = cli(
        &["+validate-config"],
        Some("background-opacity = NaN\nwindow-width = 0\n"),
    );
    assert!(!output.status.success());
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(errors.contains("background-opacity"), "{errors}");
    assert!(errors.contains("window-width"), "{errors}");
    let (output, _) = cli(&["+validate-config"], None);
    assert!(output.status.success());
}
