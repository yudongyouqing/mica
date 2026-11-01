//! pane/会话关闭的资源回收(T03,2026-11-01)。
//!
//! 历史问题:关标签/关分屏/重启 shell 一律 `mem::forget` 整份 Terminal
//! (Surface、行缓存、字体路由、PTY 句柄全部),主窗退出路径的坏驱动
//! 绕行被扩散到了日常操作——长期运行下内存/句柄只增不减。
//!
//! 最小阻塞对象分析(2026-11-01,portable-pty 0.9 源码核实):
//! - `PtySession::drop` = child.kill(TerminateProcess,非阻塞) +
//!   try_wait(GetExitCodeProcess,非阻塞) + writer/master 句柄关闭。
//!   唯一可能长时间阻塞的是 master drop 的 **ClosePseudoConsole**——
//!   它等 conhost 侧 IO 线程排空,管道半死时陷内核(历史"关窗未响应"
//!   的 PTY 侧挂点)。
//! - wgpu teardown 是 GPU 侧独立挂点(坏驱动栈,1.0.2 实证),只属于
//!   进程退出绕行,不覆盖日常关闭。
//! - Surface/row_insts/router(COM Release)/palette 全是 CPU 侧,UI
//!   线程直接 Drop 安全——config 热重载每轮都这样换 router,有先例。
//!
//! 因此日常关闭拆两步:CPU 状态当场 Drop;PtySession 交给**有界回收
//! 器**在后台线程 Drop。回收通道满(理论上线程卡死堆积)则降级 forget
//! 并计数——绝不阻塞 UI,也绝不无限开线程。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

use mica_core::pty::PtySession;

/// 回收通道容量:同时在途的待关闭会话上限。达到上限说明 reaper 线程
/// 已死或驱动栈全面卡死——此后的会话降级 forget 并计数(可见故障,
/// 不是静默吞)。
const REAPER_CAPACITY: usize = 16;

static REAPED_OK: AtomicUsize = AtomicUsize::new(0);
static REAPED_FALLBACK_FORGET: AtomicUsize = AtomicUsize::new(0);
static REAPER_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

fn reaper_tx() -> &'static SyncSender<PtySession> {
    static TX: std::sync::OnceLock<SyncSender<PtySession>> = std::sync::OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = sync_channel(REAPER_CAPACITY);
        std::thread::Builder::new()
            .name("pty-reaper".into())
            .spawn(move || {
                for session in rx {
                    // Drop 在此发生:ClosePseudoConsole 卡多久都只卡本线程
                    drop(session);
                    REAPER_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
                    REAPED_OK.fetch_add(1, Ordering::Relaxed);
                }
            })
            .expect("spawn pty reaper");
        tx
    })
}

/// 把会话塞进回收通道;失败(满/断)走降级 forget + 计数。返回是否
/// 进入后台回收。独立成函数是为了降级路径可用"断开的通道"确定性测试。
fn reap_via(tx: &SyncSender<PtySession>, session: PtySession) -> bool {
    match tx.try_send(session) {
        Ok(()) => {
            REAPER_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(TrySendError::Full(s)) | Err(TrySendError::Disconnected(s)) => {
            REAPED_FALLBACK_FORGET.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[lifecycle] 回收通道满/断(已降级 forget 累计 {} 次)——reaper 可能已卡死",
                REAPED_FALLBACK_FORGET.load(Ordering::Relaxed)
            );
            std::mem::forget(s);
            false
        }
    }
}

/// 把会话交给后台回收。调用方必须已 `kill_child`(顺序:先杀子再关
/// master,防 defterm 弹窗接管——1.0.4 实证)。
pub fn reap_session(session: PtySession) {
    reap_via(reaper_tx(), session);
}

/// 诊断面:已后台回收 / 降级 forget / 在途会话数。
pub fn reaper_stats() -> (usize, usize, usize) {
    (
        REAPED_OK.load(Ordering::Relaxed),
        REAPED_FALLBACK_FORGET.load(Ordering::Relaxed),
        REAPER_IN_FLIGHT.load(Ordering::Relaxed),
    )
}

/// 关闭单个 pane 的终端态——所有关闭路径(关标签/关分屏/进程退出)的
/// 统一入口。顺序契约:
/// 1. kill_child(TerminateProcess,非阻塞;先杀子再关 master,防 defterm
///    弹窗接管——1.0.4 实证);
/// 2. 出孤儿注册表(T02);
/// 3. 拆分回收:session 交 reaper(唯一阻塞面 ClosePseudoConsole 在后台),
///    Surface/行缓存/路由(COM)/调色板等 CPU 状态当场 Drop——config
///    热重载每轮都这么换 router,无内核阻塞面,有先例。
pub fn close_terminal(mut t: super::Terminal) {
    t.session.kill_child();
    if let Some(pid) = t.session.child_pid() {
        super::orphan::untrack_session(pid);
    }
    let super::Terminal {
        term,
        session,
        pty_buf,
        session_gen: _,
        router,
        metrics: _,
        palette: _,
        cols: _,
        rows: _,
        row_insts,
        shell: _,
        exited: _,
        strip_router,
        last_atlas_ver: _,
        force_full: _,
    } = t;
    reap_session(session);
    // pty_buf 是与转发线程共享的 Arc:这里放掉本端引用,转发线程随
    // EOF 退场时放最后一份。其余全部 CPU 侧,当场释放。
    drop(term);
    drop(router);
    drop(strip_router);
    drop(row_insts);
    drop(pty_buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真 pty 会话交回收器:在途计数归零 = 后台 Drop 完成(不卡 UI)。
    /// ConPTY 冷启动慢是预算不是卡死(CLAUDE.md 契约)。
    #[test]
    fn session_is_dropped_off_thread() {
        let (_, reader) = PtySession::spawn(
            mica_core::pty::command_from_str("cmd.exe /c exit 0"),
            80,
            24,
        )
        .expect("spawn pty");
        drop(reader);
        let (mut session, _reader2) = PtySession::spawn(
            mica_core::pty::command_from_str("cmd.exe /c exit 0"),
            80,
            24,
        )
        .expect("spawn pty");
        session.kill_child();
        reap_session(session);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if reaper_stats().2 == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "10s 内未完成后台回收,在途 {}",
                reaper_stats().2
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(reaper_stats().0 >= 1);
    }

    /// 通道满/断 → 降级 forget + 计数,绝不阻塞、绝不 panic。
    /// 用"接收端已断开"的通道确定性地触发同一段降级逻辑(不与全局
    /// reaper 线程赛跑)。
    #[test]
    fn fallback_is_counted_not_blocking() {
        let (mut session, reader) = PtySession::spawn(
            mica_core::pty::command_from_str("cmd.exe /c exit 0"),
            80,
            24,
        )
        .expect("spawn pty");
        session.kill_child();
        drop(reader);
        let (tx, rx) = std::sync::mpsc::sync_channel::<PtySession>(REAPER_CAPACITY);
        drop(rx); // 断开:发送必失败
        let before = reaper_stats().1;
        assert!(!reap_via(&tx, session), "断开的通道必须走降级");
        assert!(reaper_stats().1 > before, "降级 forget 必须计数");
    }
}
