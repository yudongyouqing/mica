//! pane/会话关闭的资源回收(T03,2026-11-01;同日按复核加固有界性)。
//!
//! 历史问题:关标签/关分屏/重启 shell 一律 `mem::forget` 整份 Terminal,
//! 长期运行内存/句柄只增不减(实测每轮 Private +3.4MB/线程 +1.9)。
//!
//! 最小阻塞对象分析(portable-pty 0.9 源码核实):`PtySession::drop` 唯一
//! 可能长时间阻塞的是 master drop 的 **ClosePseudoConsole**(等 conhost
//! IO 排空,管道半死时陷内核);kill/try_wait/COM 释放均非阻塞。wgpu
//! teardown 是独立挂点,只属进程退出绕行。
//!
//! 结构:**有界回收器** + **降级故障态**。
//! - 回收通道容量 16,单一 reaper 线程;入队限时等待(默认 2s)。
//! - 通道满/断 = reaper 卡死或过载 → 降级 forget + 计数,同时进入
//!   **DEGRADED 故障态:拒绝再创建新会话**(开标签/分屏/重启返回错误),
//!   直到通道重新有空间自动恢复。这保证总泄漏有界:
//!   ≤ 容量(队列)+ 存活 pane 数 + 1(在途),不随关闭次数无限增长。
//! - 回收确认:reaper Drop 完成后移除注册表记录(Closing → Closed);
//!   降级 forget 的记录留 Closing 态,下次启动按登记宿主身份清残留。
//!
//! 计数顺序契约:`REAPER_IN_FLIGHT` 在 try_send **之前**递增(失败回滚)
//! ——消费者 recv 必然发生在 send 之后,send 又发生在递增之后(程序序),
//! 因此消费者的 sub 永远落在 add 之后,不会出现瞬时下溢。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

use mica_core::pty::PtySession;

/// 回收通道容量:同时在途的待关闭会话上限。
const REAPER_CAPACITY: usize = 16;

static REAPED_OK: AtomicUsize = AtomicUsize::new(0);
static REAPED_FALLBACK_FORGET: AtomicUsize = AtomicUsize::new(0);
static REAPER_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// 故障态:回收通道满/断。置位期间拒绝创建新会话(见 spawn_capacity_ok)。
static DEGRADED: AtomicBool = AtomicBool::new(false);

/// reaper 领取的任务:child pid(回收确认移除注册表记录用)+ 会话本体。
struct ReapTask {
    child_pid: Option<u32>,
    session: PtySession,
}

fn reaper_tx() -> &'static SyncSender<ReapTask> {
    static TX: std::sync::OnceLock<SyncSender<ReapTask>> = std::sync::OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<ReapTask>(REAPER_CAPACITY);
        std::thread::Builder::new()
            .name("pty-reaper".into())
            .spawn(move || {
                for task in rx {
                    // Drop 在此发生:ClosePseudoConsole 卡多久都只卡本线程
                    drop(task.session);
                    REAPER_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
                    REAPED_OK.fetch_add(1, Ordering::Relaxed);
                    if let Some(pid) = task.child_pid {
                        // 回收确认:Closing → Closed,记录移除
                        super::orphan::remove_closed_session(pid);
                    }
                }
            })
            .expect("spawn pty reaper");
        tx
    })
}

/// 入队结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// 已进入后台回收。
    Reaped,
    /// 降级 forget(通道满超时/通道断开)。
    FellBack,
}

/// 核心入队:限时等待 + 降级。断开(接收端没了)立即降级;满载限时
/// 满/断立即降级。独立成函数是为了满通道/断通道路径可用自有通道
/// 确定性测试。
fn enqueue_with(
    tx: &SyncSender<ReapTask>,
    child_pid: Option<u32>,
    session: PtySession,
) -> EnqueueOutcome {
    // 计数顺序契约:先记在途,失败回滚(见模块 doc;消费者 sub 必然
    // 落在本 add 之后)。单次尝试零等待:调用方全在 UI 线程,满/断
    // 立即降级(有界性由 DEGRADED 故障态保证)
    REAPER_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
    match tx.try_send(ReapTask { child_pid, session }) {
        Ok(()) => EnqueueOutcome::Reaped,
        Err(TrySendError::Disconnected(s)) | Err(TrySendError::Full(s)) => {
            REAPER_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            let ReapTask { session, .. } = s;
            fallback_forget(session);
            EnqueueOutcome::FellBack
        }
    }
}

/// 降级 forget + 计数(通道满超时/断开):泄漏有界由 DEGRADED 故障态
/// 保证——降级后拒绝新会话,泄漏上界 = 队列容量 + 存活 pane 数 + 1。
fn fallback_forget(session: PtySession) {
    REAPED_FALLBACK_FORGET.fetch_add(1, Ordering::Relaxed);
    eprintln!(
        "[lifecycle] 回收通道满/断(降级 forget 累计 {} 次)——reaper 卡死或过载",
        REAPED_FALLBACK_FORGET.load(Ordering::Relaxed)
    );
    std::mem::forget(session);
}

/// 把会话交给后台回收。调用方必须已 `kill_child`(先杀子再关 master,
/// 防 defterm 弹窗接管——1.0.4 实证);child pid 由调用方提供
/// (close_terminal 已取过,reap 侧不必重复查询)。
pub fn reap_session_with_pid(child_pid: Option<u32>, session: PtySession) {
    let outcome = enqueue_with(reaper_tx(), child_pid, session);
    note_outcome(outcome);
}

/// 副作用登记:Reaped → 清除故障态;FellBack → 置故障态。
/// 独立成函数保证降级/恢复语义可确定性测试。
fn note_outcome(outcome: EnqueueOutcome) {
    match outcome {
        EnqueueOutcome::Reaped => DEGRADED.store(false, Ordering::Relaxed),
        EnqueueOutcome::FellBack => DEGRADED.store(true, Ordering::Relaxed),
    }
}

/// 故障态下拒绝创建新会话:开标签/分屏/重启/QT 在 spawn 前必须检查。
/// 恢复条件 = 某次回收重新入队成功(通道有空间);持久卡死则保持拒绝,
/// 泄漏上界 = 队列容量 + 存活 pane 数 + 1,不随关闭次数增长。
pub fn spawn_capacity_ok() -> bool {
    !DEGRADED.load(Ordering::Relaxed)
}

/// 诊断面:已后台回收 / 降级 forget / 在途会话数 / 故障态。
pub fn reaper_stats() -> (usize, usize, usize, bool) {
    (
        REAPED_OK.load(Ordering::Relaxed),
        REAPED_FALLBACK_FORGET.load(Ordering::Relaxed),
        REAPER_IN_FLIGHT.load(Ordering::Relaxed),
        DEGRADED.load(Ordering::Relaxed),
    )
}

/// 关闭单个 pane 的终端态——所有关闭路径(关标签/关分屏/进程退出)的
/// 统一入口。顺序契约:
/// 1. kill_child(TerminateProcess,非阻塞;先杀子再关 master,防 defterm
///    弹窗接管——1.0.4 实证);
/// 2. 注册表标 **Closing**(记录保留——后台回收确认才移除;降级 forget
///    时记录留存,下次启动按登记宿主身份清残留);
/// 3. 拆分回收:session 交 reaper(唯一阻塞面 ClosePseudoConsole 在后台),
///    Surface/行缓存/路由(COM)/调色板等 CPU 状态当场 Drop——config
///    热重载每轮都这么换 router,无内核阻塞面,有先例。
pub fn close_terminal(mut t: super::Terminal) {
    t.session.kill_child();
    let child_pid = t.session.child_pid();
    if let Some(pid) = child_pid {
        super::orphan::mark_session_closing(pid);
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
    reap_session_with_pid(child_pid, session);
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
    use std::time::{Duration, Instant};

    fn short_session() -> (PtySession, Option<u32>) {
        let (mut s, r) = PtySession::spawn(
            mica_core::pty::command_from_str("cmd.exe /c exit 0"),
            80,
            24,
        )
        .expect("spawn pty");
        s.kill_child();
        drop(r);
        let pid = s.child_pid();
        (s, pid)
    }

    /// 真 pty 会话交回收器:在途归零 = 后台 Drop 完成(不卡 UI)。
    #[test]
    fn session_is_dropped_off_thread() {
        let (session, pid) = short_session();
        reap_session_with_pid(pid, session);
        let deadline = Instant::now() + Duration::from_secs(10);
        while reaper_stats().2 > 0 {
            assert!(Instant::now() < deadline, "10s 内未完成后台回收");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(reaper_stats().0 >= 1);
    }

    /// 满通道(消费者卡死不 recv):**立即**降级 forget,零阻塞——
    /// 入队调用方全在 UI 线程,任何 sleep 都会按会话数累积卡顿
    /// (2026-11-02 复核要求:满队列时窗口仍能处理消息)。
    #[test]
    fn full_channel_falls_back_immediately_without_blocking() {
        let (tx, rx) = sync_channel::<ReapTask>(1);
        let (filler, _) = short_session();
        tx.try_send(ReapTask {
            child_pid: None,
            session: filler,
        })
        .expect("占位入队");
        // rx 存活但永不 recv(卡死形态):第二个必须立即降级
        let (session, _) = short_session();
        let start = Instant::now();
        let out = enqueue_with(&tx, None, session);
        assert_eq!(out, EnqueueOutcome::FellBack);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "入队零等待(UI 线程安全),耗时 {:?}",
            start.elapsed()
        );
        assert!(reaper_stats().1 >= 1, "降级必须计数");
        drop(rx); // 释放占位会话(reaper 不碰这条测试通道)
        drop(tx);
    }

    /// 断通道:立即降级 forget + 计数(确定性,不与全局 reaper 赛跑)。
    #[test]
    fn disconnected_channel_falls_back_immediately() {
        let (tx, rx) = sync_channel::<ReapTask>(4);
        drop(rx);
        let (session, _) = short_session();
        let start = Instant::now();
        let out = enqueue_with(&tx, None, session);
        assert_eq!(out, EnqueueOutcome::FellBack, "断开不得长等");
        assert!(start.elapsed() < Duration::from_millis(200));
        assert!(reaper_stats().1 >= 1);
    }

    /// 故障态语义:FellBack → spawn_capacity_ok=false;Reaped → 恢复 true。
    #[test]
    fn degraded_state_gate_and_recovery() {
        note_outcome(EnqueueOutcome::FellBack);
        assert!(!spawn_capacity_ok(), "降级后必须拒绝新会话");
        note_outcome(EnqueueOutcome::Reaped);
        assert!(spawn_capacity_ok(), "回收成功后自动恢复");
    }

    /// 计数顺序:消费者收到任务时,在途计数必须已包含它(≥1)——证明
    /// 递增先于移交,sub 不会先于 add(下溢)。200 次迭代放大旧序的错序
    /// 窗口;正确顺序下本断言恒成立(程序序 + 通道同步的 happens-before)。
    #[test]
    fn in_flight_counts_before_handover() {
        let (tx, rx) = sync_channel::<ReapTask>(4);
        let violations = std::sync::Arc::new(AtomicUsize::new(0));
        let v = std::sync::Arc::clone(&violations);
        let consumer = std::thread::spawn(move || {
            for task in rx {
                if REAPER_IN_FLIGHT.load(Ordering::Relaxed) == 0 {
                    v.fetch_add(1, Ordering::Relaxed);
                }
                drop(task.session);
                REAPER_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            }
        });
        for _ in 0..50 {
            let (session, _) = short_session();
            enqueue_with(&tx, None, session);
        }
        drop(tx);
        consumer.join().unwrap();
        assert_eq!(
            violations.load(Ordering::Relaxed),
            0,
            "消费者不得观察到未计数的在途任务"
        );
    }
}
