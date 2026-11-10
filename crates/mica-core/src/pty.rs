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

    /// 带超时的三态接收(T08 转发线程用):Timeout 用于空闲期做唤醒
    /// 重试,与断开(退场)严格区分——recv_timeout 把两者折叠成 None,
    /// 无法支撑"Post 失败后在空闲期自愈"。
    pub fn recv_event(&self, timeout: std::time::Duration) -> RecvEvent {
        match self.rx.recv_timeout(timeout) {
            Ok(chunk) => RecvEvent::Data(chunk),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => RecvEvent::Timeout,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => RecvEvent::Closed,
        }
    }
}

/// `recv_event` 的三态:数据 / 超时(继续等) / 断开(通道关闭)。
#[derive(Debug, PartialEq, Eq)]
pub enum RecvEvent {
    Data(Vec<u8>),
    Timeout,
    Closed,
}

/// 写端句柄(T08 二次返工):**只持写端 Arc 互斥,不持会话**——
/// writer 线程阻塞写时只卡这把锁,kill/resize/轮询走会话自身的字段,
/// 永不被阻塞写拖住(复核 3 的锁分离)。
pub struct PtyWriterHandle {
    writer: std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
}

impl PtyWriterHandle {
    pub fn write(&self, data: &[u8]) -> std::io::Result<()> {
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(data)
    }
}

/// A running child on a pseudo-terminal.
pub struct PtySession {
    /// 写端独立 Arc 互斥:句柄可在会话外分发(writer 线程),会话的
    /// write 同源;控制面(kill/resize/wait)不碰这把锁。
    writer: std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    /// None = detached 占位(重启失败后的 Stopped 态,见 [`Self::detached`])。
    master: Option<Box<dyn MasterPty + Send>>,
    /// None = detached 占位:kill/try_wait 退化为无害操作,child_pid
    /// 恒 None(孤儿注册表/回收按"无子"处理)。
    child: Option<Box<dyn Child + Send + Sync>>,
}

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
    let cmd = cmd.trim_matches([' ', '\t']);
    // A whole `shell = C:\Program Files\...\shell.exe` setting may name
    // an executable directly, even without command-line quotes.
    if std::path::Path::new(cmd).is_file() {
        return CommandBuilder::new(cmd);
    }
    let Some((exe, mut rest)) = next_windows_arg(cmd) else {
        return default_shell_command();
    };
    let powershell = exe.rsplit(['/', '\\']).next().is_some_and(|name| {
        ["powershell", "powershell.exe", "pwsh", "pwsh.exe"]
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
    });
    let mut builder = CommandBuilder::new(&exe);
    while let Some((arg, tail)) = next_windows_arg(rest) {
        let script =
            powershell && (arg.eq_ignore_ascii_case("-Command") || arg.eq_ignore_ascii_case("-c"));
        builder.arg(arg);
        rest = tail;
        if script {
            // PowerShell treats the remainder as source code. Parsing each
            // token would remove quotes inside ordinary -Command scripts.
            let source = rest.trim_start_matches([' ', '\t']);
            if let Some((arg, tail)) = next_windows_arg(source) {
                if source.starts_with('"') && tail.trim().is_empty() {
                    builder.arg(arg); // one explicitly quoted script argument
                } else {
                    builder.arg(source);
                }
            }
            break;
        }
    }
    builder
}

/// Windows CRT argument quoting: whitespace separates outside quotes;
/// 2n backslashes before a quote produce n backslashes + a quote delimiter,
/// 2n+1 produce n backslashes + a literal quote. Empty quoted args survive.
fn next_windows_arg(input: &str) -> Option<(String, &str)> {
    let input = input.trim_start_matches([' ', '\t']);
    if input.is_empty() {
        return None;
    }
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut quoted = false;
    let mut i = 0;
    while i < bytes.len() {
        if !quoted && matches!(bytes[i], b' ' | b'\t') {
            break;
        }
        let start = i;
        while i < bytes.len() && bytes[i] == b'\\' {
            i += 1;
        }
        let slashes = i - start;
        if i < bytes.len() && bytes[i] == b'"' {
            out.extend(std::iter::repeat_n(b'\\', slashes / 2));
            if slashes % 2 == 1 {
                out.push(b'"');
            } else if quoted && bytes.get(i + 1) == Some(&b'"') {
                out.push(b'"');
                i += 1;
            } else {
                quoted = !quoted;
            }
            i += 1;
        } else {
            out.extend(std::iter::repeat_n(b'\\', slashes));
            if !quoted && bytes.get(i).is_some_and(|b| matches!(b, b' ' | b'\t')) {
                break;
            }
            if i < bytes.len() {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    Some((
        String::from_utf8(out).expect("only ASCII delimiters removed"),
        &input[i..],
    ))
}

/// Quote one generated argument using the inverse of the Windows CRT rules.
pub fn quote_windows_arg(arg: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
        } else {
            out.extend(std::iter::repeat_n(
                '\\',
                if c == '"' { slashes * 2 + 1 } else { slashes },
            ));
            out.push(c);
            slashes = 0;
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
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

        // T08 有界读通道:满则读线程阻塞在 send → ConPTY 管道写端顶住
        // → shell 暂停产出(源头背压,正常字节零丢弃)。64 块 × 8KB =
        // 512KB 通道缓冲,叠加 app 侧 PtyBuf 预算。
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
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
                writer: std::sync::Arc::new(std::sync::Mutex::new(writer)),
                master: Some(pair.master),
                child: Some(child),
            },
            PtyReader { rx },
        ))
    }

    /// 无会话占位(**Stopped 态**,T08 r5-2:重启失败后 pane 持有它直到
    /// 下次重试成功)。master/child 均空——kill/try_wait 无害空操作、
    /// child_pid 恒 None、resize 报错、Drop 无阻塞面(无 ConPTY 可关);
    /// 写端是黑洞(Vec):管道不存在,写入无人消费也不报错。
    pub fn detached() -> Self {
        Self {
            writer: std::sync::Arc::new(std::sync::Mutex::new(Box::new(Vec::new()))),
            master: None,
            child: None,
        }
    }

    /// Bytes typed by the user. `&self`(T08):写端内部互斥。
    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(data)
    }

    /// 取独立写端句柄(T08):writer 线程持它写,锁与控制面分离。
    pub fn writer_handle(&self) -> PtyWriterHandle {
        PtyWriterHandle {
            writer: std::sync::Arc::clone(&self.writer),
        }
    }

    /// 测试面:锁住写端(验证 kill/resize 不被阻塞写拖住)。
    #[doc(hidden)]
    pub fn block_writer_for_test(&self) -> std::sync::MutexGuard<'_, Box<dyn Write + Send>> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 子进程 PID(孤儿会话注册表按 PID+创建时间证明归属,2026-11 T02)。
    /// detached 占位恒 None。
    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.process_id())
    }

    pub fn resize(&self, columns: u16, rows: u16) -> anyhow::Result<()> {
        let Some(master) = self.master.as_ref() else {
            anyhow::bail!("detached 会话(Stopped 态)无 pty 可 resize");
        };
        master.resize(PtySize {
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
    /// detached 占位无子,空操作。
    pub fn kill_child(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.try_wait();
        }
    }

    /// detached 占位恒 `Ok(None)`(无子可等——Stopped 态 pane 不触发
    /// 退出事件,重试入口保持可用)。
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // 杀子进程:测试不留僵尸 shell。注:子进程真实退出的 exited 态尚未
        // 实现——shell 自行退出后窗口仍存活、输入静默失败;观察项在 M1-B
        // 计划 T9,UX 处理归 M2。
        // portable-pty 的 kill 是 SIGHUP→轮询→SIGKILL 升级,但 SIGKILL 路径
        // 不 wait —— 补一次收割,否则 trap 了 HUP 的子进程每个留一个僵尸。
        // detached 占位无子,空操作(Drop 无阻塞面)。
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.try_wait();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn command_line_keeps_non_ascii_whitespace_as_argument_data() {
        let cmd = super::command_from_str("shell.exe value\u{3000}");
        assert_eq!(cmd.get_argv()[1], "value\u{3000}");
    }
    #[test]
    fn command_line_backslash_does_not_escape_unquoted_whitespace() {
        let cmd = super::command_from_str(r#"shell.exe a\ b "a""b" ''"#);
        assert_eq!(
            cmd.get_argv(),
            &["shell.exe", "a\\", "b", "a\"b", "''"].map(std::ffi::OsString::from)
        );
    }
    #[test]
    fn command_line_preserves_windows_quotes_backslashes_and_empty_arguments() {
        let cmd = super::command_from_str(
            r#""C:\Program Files\shell.exe" "two words" "" "say \"hi\"" "C:\dir\\" plain"#,
        );
        let argv: Vec<_> = cmd
            .get_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            [
                r"C:\Program Files\shell.exe",
                "two words",
                "",
                "say \"hi\"",
                "C:\\dir\\",
                "plain"
            ]
        );
    }

    #[test]
    fn command_line_preserves_unquoted_powershell_command_script() {
        let cmd = super::command_from_str(
            r#"powershell.exe -NoLogo -Command Write-Output "two words"; $x = 'keep me'; Write-Output $x"#,
        );
        assert_eq!(
            cmd.get_argv()[3],
            r#"Write-Output "two words"; $x = 'keep me'; Write-Output $x"#
        );
    }

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
