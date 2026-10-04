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

    /// Block until the next chunk arrives. Unlike `recv_timeout`, a `None`
    /// here is unambiguous: it means *only* that the pty reader thread has
    /// hung up (channel disconnected — pty closed or app shut down). A
    /// quiet-but-alive pty keeps this parked. Pure channel plumbing, no GUI:
    /// this is what lets the app's forwarder thread park here and still
    /// notice shutdown without polling.
    pub fn recv_block(&self) -> Option<Vec<u8>> {
        self.rx.recv().ok()
    }
}

/// A running child on a pseudo-terminal.
pub struct PtySession {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

/// 命令行字符串(exe + 空格分隔参数)→ CommandBuilder(M3a profile 用;
/// 引号语义不支持——profile 命令都是受控生成,不含带空格路径)。
/// ConPTY 宿主消毒(1.0.6):Windows 的 CreatePseudoConsole 在系统内部
/// 解析宿主 exe(OpenConsole/conhost),解析会扫进程 PATH——装了 WezTerm
/// 的机器上 F:\WezTerm 的 OpenConsole 抢位成功,我们的 ConPTY 会话寄生
/// 在第三方宿主里(与 WezTerm 版本无契约,行为不可控,实测现场)。
/// 解法零系统侵入:只在 Mica 进程内剔除"含 OpenConsole.exe 的 PATH 目录",
/// 解析回退系统 conhost。WezTerm 本体、用户系统 PATH 均不受影响。
pub fn sanitize_conpty_host_path() {
    let Some(path) = std::env::var_os("PATH") else {
        return;
    };
    let kept: Vec<std::path::PathBuf> = std::env::split_paths(&path)
        .filter(|dir| {
            // 目录下躺着 OpenConsole.exe = 第三方 ConPTY 宿主注入点,剔除
            !dir.join("OpenConsole.exe").is_file()
        })
        .collect();
    let joined = std::env::join_paths(kept).unwrap_or(path);
    // SAFETY: run() 早期、单线程窗口创建前调用,无并发读者(调用点契约)
    unsafe {
        std::env::set_var("PATH", joined);
    }
}

pub fn command_from_str(cmd: &str) -> CommandBuilder {
    let mut parts = cmd.split_whitespace();
    let Some(exe) = parts.next() else {
        return default_shell_command();
    };
    let mut builder = CommandBuilder::new(exe);
    for arg in parts {
        builder.arg(arg);
    }
    builder
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

    /// 非阻塞杀子(1.0.4):退出/关闭路径在 mem::forget 前调用——
    /// 泄漏 Terminal 连子进程一起漏,powershell 成孤儿;下次启动孤儿
    /// 清杀只杀 OpenConsole,powershell 失宿主被 defterm 重派终端
    /// (WT 弹窗/0xC0000142,"第二次启动报错"的真身)。kill 是
    /// TerminateProcess 非阻塞,try_wait 收割不等待——永不挂。
    pub fn kill_child(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.try_wait();
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // 杀子进程:测试不留僵尸 shell。注:子进程真实退出的 exited 态尚未
        // 实现——shell 自行退出后窗口仍存活、输入静默失败;观察项在 M1-B
        // 计划 T9,UX 处理归 M2。
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
    use std::sync::mpsc;
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

    /// 读到超时为止,返回全部输出。收到 DSR 光标查询(`ESC[6n`)时以
    /// `ESC[1;1R` 应答——cmd/powershell 启动即查询并阻塞等回包,
    /// 测试就是"终端",得替它答(与 EventProxy take_pty_writes 同一契约)。
    fn read_until(
        session: &mut PtySession,
        reader: &PtyReader,
        needle: &[u8],
        budget: Duration,
    ) -> Vec<u8> {
        let deadline = Instant::now() + budget;
        let mut all = Vec::new();
        let mut answered = 0; // 已应答的查询数:扫描的是累计缓冲,防重复回包
        while Instant::now() < deadline {
            if let Some(chunk) = reader.recv_timeout(Duration::from_millis(500)) {
                all.extend_from_slice(&chunk);
                let pending = all.windows(4).filter(|w| *w == b"\x1b[6n").count();
                for _ in answered..pending {
                    session.write(b"\x1b[1;1R").expect("reply to DSR query");
                }
                answered = pending;
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
        let (mut session, reader) = PtySession::spawn(echo_cmd(), 80, 24).unwrap();
        read_until(&mut session, &reader, MARKER, TIMEOUT);
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
        read_until(&mut session, &reader, b"wgtype_555", TIMEOUT);
    }

    #[cfg(not(windows))]
    #[test]
    fn keystrokes_reach_cat() {
        let (mut session, reader) = PtySession::spawn(CommandBuilder::new("cat"), 80, 24).unwrap();
        session.write(b"wgtype_555\n").unwrap();
        read_until(&mut session, &reader, b"wgtype_555", TIMEOUT);
    }

    /// T7:`recv_block` 的 None 必须只代表"读线程挂断(通道断开)",不得像
    /// recv_timeout 那样把"暂时没数据"也折叠成 None——转发线程泊在它上面,
    /// 靠这个区分继续泊车与退场。纯通道构造,不起真 pty,mac 上即可跑。
    #[test]
    fn recv_block_none_only_on_disconnect() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let reader = PtyReader { rx };
        tx.send(b"chunk".to_vec()).expect("send chunk");
        assert_eq!(reader.recv_block().as_deref(), Some(&b"chunk"[..]));
        drop(tx); // 读线程挂断 = 发送端消失
        assert!(reader.recv_block().is_none());
    }
}
