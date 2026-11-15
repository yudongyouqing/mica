use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub fn run_with_deadline(
    command: &mut Command,
    timeout: Duration,
    log_dir: &Path,
) -> std::io::Result<Output> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(log_dir)?;
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let stem = format!("cli-{}-{id}", std::process::id());
    let stdout_path = log_dir.join(format!("{stem}.stdout.log"));
    let stderr_path = log_dir.join(format!("{stem}.stderr.log"));
    // Files avoid a second unbounded wait in read_to_end if a descendant
    // inherits a pipe handle. Keep them after failure for diagnosis.
    command
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW: no default-terminal delegation
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout_path)?)
        .stderr(std::fs::File::create(&stderr_path)?);
    let started = Instant::now();
    let mut child = command.spawn()?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            // This Child still owns the process handle, so its PID cannot be
            // reused while we terminate its tree. Never kill by process name.
            let mut killer = Command::new("taskkill")
                .args(["/PID", &child.id().to_string(), "/T", "/F"])
                .creation_flags(0x0800_0000)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let stop_by = Instant::now() + Duration::from_secs(1);
            while killer.try_wait()?.is_none() && Instant::now() < stop_by {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = killer.kill();
            let _ = killer.try_wait();
            let _ = child.kill();
            // A broken native driver may make even forced termination slow.
            // Do not replace the original CLI hang with an unbounded wait().
            let reap_by = Instant::now() + Duration::from_millis(500);
            while child.try_wait()?.is_none() && Instant::now() < reap_by {
                std::thread::sleep(Duration::from_millis(10));
            }
            let stderr = std::fs::read(&stderr_path).unwrap_or_default();
            let tail = &stderr[stderr.len().saturating_sub(4096)..];
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "CLI {:?} pid={} exceeded {timeout:?}; logs: {}, {}; stderr: {}",
                    command.get_program(),
                    child.id(),
                    stdout_path.display(),
                    stderr_path.display(),
                    String::from_utf8_lossy(tail),
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: std::fs::read(stdout_path)?,
        stderr: std::fs::read(stderr_path)?,
    })
}
