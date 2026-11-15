//! Exercise the real PTY spawn path from an executable whose path has spaces.
use mica_core::pty::{PtySession, command_from_str};
use std::time::{Duration, Instant};

#[test]
fn argv_child_probe() {
    if let Some(path) = std::env::var_os("MICA_ARGV_PROBE_FILE") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        std::fs::write(path, serde_json::to_vec(&args).unwrap()).unwrap();
    }
}

#[test]
fn launches_spaced_executable_with_exact_argv() {
    // No global environment mutation: only this child's CommandBuilder gets
    // the probe variable, and the copied test executable runs one test.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mica argv probe {} {unique}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let exe = dir.join(if cfg!(windows) {
        "argv helper.exe"
    } else {
        "argv helper"
    });
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    assert_eq!(
        command_from_str(exe.to_str().unwrap()).get_argv(),
        &[exe.as_os_str().to_owned()],
        "a whole shell setting may name an unquoted executable path"
    );
    let result_path = dir.join("result.json");
    let command = format!(
        r#""{}" --exact argv_child_probe --nocapture --test-threads=1 -- "" "two words" "say \"hi\"" "C:\dir\\" "中文""#,
        exe.display()
    );
    let mut cmd = command_from_str(&command);
    cmd.env("MICA_ARGV_PROBE_FILE", &result_path);
    let result = PtySession::spawn(cmd, 80, 24);
    if let Err(error) = result {
        std::fs::remove_dir_all(&dir).unwrap();
        panic!("failed to launch spaced executable: {error:#}");
    }
    let (mut session, reader) = result.unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut output = Vec::new();
    while session.try_wait().unwrap().is_none() {
        if let Some(chunk) = reader.recv_timeout(Duration::from_millis(20)) {
            output.extend(chunk);
            if output.windows(4).any(|part| part == b"\x1b[6n") {
                session.write(b"\x1b[1;1R").unwrap();
                output.clear();
            }
        }
        if Instant::now() >= deadline {
            drop(session);
            std::fs::remove_dir_all(&dir).unwrap();
            panic!("argv probe timed out");
        }
    }
    drop(session);
    let args: Vec<String> =
        serde_json::from_slice(&std::fs::read(&result_path).expect("child wrote argv")).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        args,
        [
            "--exact",
            "argv_child_probe",
            "--nocapture",
            "--test-threads=1",
            "--",
            "",
            "two words",
            "say \"hi\"",
            "C:\\dir\\",
            "中文"
        ]
    );
}

#[cfg(windows)]
#[test]
fn powershell_command_keeps_script_quotes_during_real_launch() {
    let cmd = command_from_str(
        r#"powershell.exe -NoLogo -NoProfile -NonInteractive -Command [Console]::Write("MICA_SCRIPT:" + "two words" + ':' + (6*7)); exit 0"#,
    );
    let (session, reader) = PtySession::spawn(cmd, 80, 24).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut output = Vec::new();
    let mut answered = 0;
    loop {
        if let Some(chunk) = reader.recv_timeout(Duration::from_millis(20)) {
            output.extend(chunk);
            let pending = output.windows(4).filter(|part| *part == b"\x1b[6n").count();
            for _ in answered..pending {
                session.write(b"\x1b[1;1R").unwrap();
            }
            answered = pending;
            if output
                .windows(b"MICA_SCRIPT:two words:42".len())
                .any(|part| part == b"MICA_SCRIPT:two words:42")
            {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "script output missing: {}",
            String::from_utf8_lossy(&output)
        );
    }
}
