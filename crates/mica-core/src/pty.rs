//! Pseudo-terminal sessions backed by portable-pty (ConPTY on Windows,
//! openpty elsewhere). A session owns the child; dropping it kills the child
//! so neither tests nor the app leak shells.

use std::io::{self, Read, Write};
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

/// Reader half: child output, delivered in chunks from a background thread.
pub struct PtyReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
}

impl PtyReader {
    /// Next chunk of output, or `None` on timeout *or* pty close —
    /// use `PtySession::try_wait` to tell a child exit apart from a quiet pty.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Drain everything currently buffered without blocking.
    pub fn drain(&self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(chunk) = self.rx.try_recv() {
            out.extend_from_slice(&chunk);
        }
        out
    }
}

/// A running child on a pseudo-terminal.
pub struct PtySession {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

/// The shell a new tab gets: PowerShell on Windows (spec M0), sh elsewhere
/// (so core tests run on the macOS dev box).
pub fn default_shell_command() -> CommandBuilder {
    #[cfg(windows)]
    {
        let mut cmd = CommandBuilder::new("powershell.exe");
        cmd.arg("-NoLogo");
        cmd
    }
    #[cfg(not(windows))]
    {
        CommandBuilder::new("sh")
    }
}

impl PtySession {
    /// Spawn `cmd` on a new pty of `columns`x`rows`; returns the session and a
    /// reader half fed by a background thread.
    pub fn spawn(
        cmd: CommandBuilder,
        columns: u16,
        rows: u16,
    ) -> anyhow::Result<(Self, PtyReader)> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols: columns,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let child = pair.slave.spawn_command(cmd)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = buf[..n].to_vec();
                            if tx.send(chunk).is_err() {
                                break; // receiver gone: app shut down
                            }
                        }
                    }
                }
            })?;

        Ok((
            Self {
                writer,
                master: pair.master,
                child,
            },
            PtyReader { rx },
        ))
    }

    /// Bytes typed by the user.
    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.write_all(data)
    }

    pub fn resize(&self, columns: u16, rows: u16) -> anyhow::Result<()> {
        self.master.resize(PtySize {
            rows,
            cols: columns,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // 杀子进程:测试不留僵尸 shell;真实退出流程由 Task 7 的 exited 态处理。
        // portable-pty 的 kill 是 SIGHUP→轮询→SIGKILL 升级,但 SIGKILL 路径
        // 不 wait —— 补一次收割,否则 trap 了 HUP 的子进程每个留一个僵尸。
        let _ = self.child.kill();
        let _ = self.child.try_wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::CommandBuilder;
    use std::time::{Duration, Instant};

    const MARKER: &[u8] = b"wgmark_777";
    const TIMEOUT: Duration = Duration::from_secs(10);

    fn echo_cmd() -> CommandBuilder {
        #[cfg(windows)]
        {
            let mut cmd = CommandBuilder::new("cmd.exe");
            cmd.arg("/c");
            cmd.arg("echo wgmark_777");
            cmd
        }
        #[cfg(not(windows))]
        {
            let mut cmd = CommandBuilder::new("sh");
            cmd.arg("-c");
            cmd.arg("echo wgmark_777");
            cmd
        }
    }

    /// 读到超时为止,返回全部输出。
    fn read_until(reader: &PtyReader, needle: &[u8], budget: Duration) -> Vec<u8> {
        let deadline = Instant::now() + budget;
        let mut all = Vec::new();
        while Instant::now() < deadline {
            if let Some(chunk) = reader.recv_timeout(Duration::from_millis(500)) {
                all.extend_from_slice(&chunk);
                if all.windows(needle.len()).any(|w| w == needle) {
                    return all;
                }
            }
        }
        panic!(
            "never saw {:?}; got {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&all)
        );
    }

    #[test]
    fn child_output_reaches_reader() {
        let (_session, reader) = PtySession::spawn(echo_cmd(), 80, 24).unwrap();
        read_until(&reader, MARKER, TIMEOUT);
    }

    #[test]
    fn resize_is_accepted() {
        let (session, _reader) = PtySession::spawn(echo_cmd(), 80, 24).unwrap();
        session.resize(100, 30).expect("resize after spawn");
    }

    #[cfg(windows)]
    #[test]
    fn keystrokes_reach_cmd_shell() {
        let (mut session, reader) =
            PtySession::spawn(CommandBuilder::new("cmd.exe"), 80, 24).unwrap();
        session.write(b"echo wgtype_555\r\n").unwrap();
        read_until(&reader, b"wgtype_555", TIMEOUT);
    }

    #[cfg(not(windows))]
    #[test]
    fn keystrokes_reach_cat() {
        let (mut session, reader) = PtySession::spawn(CommandBuilder::new("cat"), 80, 24).unwrap();
        session.write(b"wgtype_555\n").unwrap();
        read_until(&reader, b"wgtype_555", TIMEOUT);
    }
}
