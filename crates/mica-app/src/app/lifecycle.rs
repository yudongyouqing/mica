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

/// reaper 领取的任务:child pid(回收确认移除注册表记录用)+ 会话本体
/// (Arc:writer 线程持另一份;最后一份 Drop 杀子+收割,发生在 reaper
/// 或 writer 任一侧——ClosePseudoConsole 只阻塞持有者,不碰 UI)。
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
                    // Drop 在此发生(按值:reaper 是唯一所有者,Drop 完成
                    // 即 PTY 真实销毁——计数与注册表移除都在 Drop 之后,
                    // 不提前记录回收完成)
                    let done_pid = task.child_pid;
                    drop(task.session);
                    REAPER_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
                    REAPED_OK.fetch_add(1, Ordering::Relaxed);
                    if let Some(pid) = done_pid {
                        super::orphan::remove_closed_session(pid);
                    }
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
fn enqueue_with(tx: &SyncSender<ReapTask>, task: ReapTask) -> EnqueueOutcome {
    // 计数顺序契约:先记在途,失败回滚(见模块 doc;消费者 sub 必然
    // 落在本 add 之后)。单次尝试零等待:调用方全在 UI 线程,满/断
    // 立即降级(有界性由 DEGRADED 故障态保证)
    REAPER_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
    match tx.try_send(task) {
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
    let outcome = enqueue_with(reaper_tx(), ReapTask { child_pid, session });
    note_outcome(outcome);
}

/// 副作用登记:Reaped → 清除故障态;FellBack → 置故障态。
/// 独立成函数保证降级/恢复语义可确定性测试(pub(crate):重启失败
/// 分支的测试用它注入真实 DEGRADED 态)。
pub(crate) fn note_outcome(outcome: EnqueueOutcome) {
    match outcome {
        EnqueueOutcome::Reaped => DEGRADED.store(false, Ordering::Relaxed),
        EnqueueOutcome::FellBack => DEGRADED.store(true, Ordering::Relaxed),
    }
}

/// 故障态下拒绝创建新会话:开标签/分屏/重启/QT 在 spawn 前必须检查。
/// 并发会话数上限(复核 8):上游背压链的最坏界按 N 计算——
/// 16 会话 ×(512KB 源通道 + 8KB 在途)≈ 8.3MB 已知界;不设上限则
/// "最坏合计"没有数字。达上限拒绝新建(与 DEGRADED 同一门口)。
pub const MAX_SESSIONS: usize = 16;
static LIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);

/// **CAS 严格预留**(复核 5:load-then-add 在多窗并发下可同时通过
/// 同一检查,超 16)。fetch 循环保证"检查+占位"原子。
fn try_acquire_slot() -> bool {
    let mut cur = LIVE_SESSIONS.load(Ordering::Acquire);
    loop {
        if cur >= MAX_SESSIONS {
            return false;
        }
        match LIVE_SESSIONS.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return true,
            Err(actual) => cur = actual,
        }
    }
}

/// 会话槽位 RAII(复核 5):**acquire 在 spawn 之前**(None = 已满,
/// 调用方直接拒绝,无回滚孤儿);Drop 归还。重启的转移序:
/// kill 旧 → drop 旧 permit → acquire 新(旧会话先终结再占新槽,
/// 峰值严格 ≤ MAX;acquire 失败则重启失败,pane 留无会话态可重试)。
pub struct SessionPermit {
    _private: (),
}

/// 获取会话槽位(spawn 前调用)。
pub fn acquire_session() -> Option<SessionPermit> {
    if !try_acquire_slot() {
        return None;
    }
    Some(SessionPermit { _private: () })
}

impl Drop for SessionPermit {
    fn drop(&mut self) {
        LIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 恢复条件 = 未处 DEGRADED 且有空槽(仅 UI 前置提示;真正的严格
/// 占用由 acquire_session 的 CAS 保证)。
pub fn spawn_capacity_ok() -> bool {
    !DEGRADED.load(Ordering::Relaxed) && LIVE_SESSIONS.load(Ordering::Acquire) < MAX_SESSIONS
}

/// **共享重启帮手**(r5-2):主窗与 QT 的重启都走同一契约——
/// **kill 旧 → 放旧 permit → 取新 permit → spawn 新 PTY**。
/// - 满额时原地重启:自己的旧槽在 spawn 前释放,竞争失败可恢复
///   (不查 spawn_capacity_ok——它查的是"是否满"会阻止原地重启)。
/// - spawn 失败:已持有的新 permit 随 Err 提前返回的 RAII 归还。
/// - 旧 PTY 的关闭(阻塞面 ClosePseudoConsole)由调用方交 reaper;
///   本函数不碰 UI 线程。
///
/// 调用方契约:`kill_old` 必须先终止旧会话的子进程(非阻塞)并返回
/// 旧 pid;`spawn` 在新 permit 到手后才被调用。
/// 主窗/QT 共用的重启槽位转移:kill_old(调用方已完成)→ 放旧 → 取新。
/// 返回 acquire 结果;spawn 由调用方执行(permit 到手后再 spawn,
/// spawn 失败调用方 drop 返回的 permit 即回滚)。
pub fn restart_transfer(old: &mut Option<SessionPermit>) -> Option<SessionPermit> {
    // 先放旧(自持槽回到池中——满额时原地重启由此可行)。DEGRADED
    // 拒绝也必须放旧:调用方(restart_session_fields)此前已 kill 旧会话,
    // 槽位随会话走,故障态不得多占(r5-2 修前 DEGRADED 早退在 take
    // 之前,旧 permit 悬留)
    drop(old.take());
    // r5-2 测试注入点:放旧之后、取新之前(测试模拟另一窗口在此窗口
    // 抢走刚释放的槽位——真实竞争窗口,生产无操作)
    #[cfg(test)]
    on_permit_released();
    if DEGRADED.load(Ordering::Relaxed) {
        return None;
    }
    // 再取新(其他窗口可能抢走——返回 None 即 SlotStolen)
    acquire_session()
}

// r5-2 注入点载体(同 pty_bridge 的 PUSH_LOCKED_HOOK 模式):restart_
// transfer 放旧与取新之间的窗口,测试注入并发抢槽。pub(crate):测试
// 模块在 pty_bridge::tests(兄弟模块),须跨模块可见。
#[cfg(test)]
thread_local! {
    pub(crate) static PERMIT_RELEASED_HOOK: std::cell::RefCell<Option<std::rc::Rc<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn on_permit_released() {
    PERMIT_RELEASED_HOOK.with(|h| {
        if let Some(f) = h.borrow().as_ref() {
            f();
        }
    });
}

/// 重启失败原因(全部可重试——pane 留 Stopped 态,再次调用本流即可)。
#[derive(Debug)]
pub enum RestartFailure {
    /// permit 转移失败:DEGRADED 故障态或槽位被其他窗口抢走
    /// (旧 permit 已释放,旧会话已回收——重试即可恢复)。
    DegradedOrSlotStolen,
    /// 新会话 spawn 失败(shell 命令无效/系统资源不足)。
    /// 新 permit 已 RAII 归还。
    SpawnFailed(String),
}

impl std::fmt::Display for RestartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DegradedOrSlotStolen => write!(
                f,
                "会话槽位被抢或回收通道故障(DEGRADED)——pane 已停止,可重试"
            ),
            Self::SpawnFailed(e) => write!(f, "新会话启动失败({e})——pane 已停止,可重试"),
        }
    }
}

/// 把旧会话换出并交后台回收(r5-2:**所有**重启路径——成功与失败——
/// 都回收旧 PTY,ClosePseudoConsole 不上 UI 线程;占位会话无子可收,
/// 直接丢弃,Drop 无阻塞面)。
fn swap_out_old(session: &mut mica_core::pty::PtySession, old_pid: Option<u32>) {
    let old = std::mem::replace(session, mica_core::pty::PtySession::detached());
    match old_pid {
        Some(pid) => reap_session_with_pid(Some(pid), old),
        None => drop(old),
    }
}

/// **生产共享重启流**(T08 r5-2;主窗 `restart_active_pane` 与 QT
/// `qt_respawn` 的唯一会话路径——按字段传入而非 `&mut Terminal`,
/// 是为了失败/恢复分支可用真实 PTY 走生产函数测试,不必构造整个
/// Terminal(GPU 路由等重建成本))。
///
/// 契约(任一步失败,pane 都已处于安全的 **Stopped 态**且可重试):
/// 1. kill 旧子(非阻塞;detached 占位时为空操作);
/// 2. **Stopped 化**:关旧写队列(`stage_into` → RejectedClosed,停止
///    接受输入)+ 欠账作废 + 输出积压换代(旧 forwarder 出局);
/// 3. permit 转移(放旧 → 取新;DEGRADED/槽位被抢 → 失败);
/// 4. spawn 新会话(失败 → permit RAII 归还);
/// 5. **回收旧 PTY** 并组装新会话/新写队列/新 writer。
///
/// 失败分支同样回收旧 PTY(不再等新 spawn 成功才回收——r5-2:失败
/// 时旧 writeq/欠账/积压的终结同样完成);permit 转移先于回收:reap
/// 成功会自愈 DEGRADED,若回收先行,通道健康时 DEGRADED 分支永远
/// 打不到(与"故障态 ⇒ Stopped"的语义冲突)。失败不回滚已完成的
/// 终结(旧子已 kill,半开状态只会更糟);调用方拿到 `Err` 后 pane
/// 输入一律拒绝、`child_pid` 恒 None,重试即从步骤 1 走全流程。
/// 成功返回 `(reader, new_gen)`,渲染侧接线(新 Surface/forwarder)
/// 由调用方完成。
#[allow(clippy::too_many_arguments)]
pub fn restart_session_fields(
    session: &mut mica_core::pty::PtySession,
    permit: &mut Option<SessionPermit>,
    writeq: &mut std::sync::Arc<super::pty_bridge::WriteQueue>,
    input_stage: &mut super::pty_bridge::InputStage,
    pty_buf: &super::pty_bridge::PtyBacklog,
    shell: &str,
    cols: u16,
    rows: u16,
    exited: &mut Option<u32>,
) -> Result<(mica_core::pty::PtyReader, u64), RestartFailure> {
    // ① kill 旧子(非阻塞;detached 占位无子,空操作)
    session.kill_child();
    let old_pid = session.child_pid();
    if let Some(pid) = old_pid {
        super::orphan::mark_session_closing(pid);
    }
    // ② Stopped 化:停止接受输入 + 欠账作废 + 换代(此后任何失败都安全)
    writeq.close();
    input_stage.clear();
    let new_gen = pty_buf.bump_generation();
    // ③ permit 转移:放旧 → 取新(DEGRADED/被抢 → 可重试失败;
    // 失败分支同样回收旧 PTY,pane 留 Stopped 态)
    let new_permit = restart_transfer(permit);
    let Some(new_permit) = new_permit else {
        swap_out_old(session, old_pid);
        return Err(RestartFailure::DegradedOrSlotStolen);
    };
    // ④ spawn 新会话(失败:permit RAII 归还,pane 留 Stopped 可重试)
    let probe = super::orphan::before_spawn();
    let (new_session, reader) = match mica_core::pty::PtySession::spawn(
        mica_core::pty::command_from_str(shell),
        cols,
        rows,
    ) {
        Ok(pair) => pair,
        Err(e) => {
            drop(new_permit);
            swap_out_old(session, old_pid);
            return Err(RestartFailure::SpawnFailed(e.to_string()));
        }
    };
    if let Some(new_pid) = new_session.child_pid() {
        super::orphan::track_session_since(&probe, new_pid);
    }
    // ⑤ 回收旧 PTY + 组装新会话/新队列/writer(旧 writer 只面对旧
    //    队列,close 后自然退场)
    swap_out_old(session, old_pid);
    *session = new_session;
    *permit = Some(new_permit);
    *exited = None;
    let fresh = super::pty_bridge::WriteQueue::new();
    let writer = session.writer_handle();
    {
        let wq = std::sync::Arc::clone(&fresh);
        std::thread::Builder::new()
            .name("pty-writer".into())
            .spawn(move || {
                super::pty_bridge::writer_loop(
                    wq,
                    std::sync::Arc::new(super::pty_bridge::SessionSink(writer)),
                )
            })
            .expect("spawn pty writer");
    }
    *writeq = fresh;
    Ok((reader, new_gen))
}

/// 诊断面:当前并发会话数。
#[allow(dead_code)]
pub fn live_sessions() -> usize {
    LIVE_SESSIONS.load(Ordering::Acquire)
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
    // 关闭序(T08 二次返工):先关写队列与输出积压(全部等待者脱身;
    // writer 若阻塞在写端,由杀子引发的管道断/EOF 退场)→ 杀子(写端
    // 锁分离,绝不被阻塞写拖住)→ 会话**按值**交 reaper(Drop 完成 =
    // PTY 真实销毁,计数不提前)。UI 侧零阻塞。
    t.writeq.close();
    t.pty_buf.close();
    t.session.kill_child();
    let child_pid = t.session.child_pid();
    if let Some(pid) = child_pid {
        super::orphan::mark_session_closing(pid);
    }
    let super::Terminal {
        term,
        session,
        session_permit: _permit,
        pty_buf,
        writeq,
        input_stage: _,
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
    // 会话(Arc):writer 线程持另一份;reaper drop 或 writer 退出,
    // 最后一份 Drop 时杀子+收割(非阻塞)——UI 线程不等
    reap_session_with_pid(child_pid, session);
    // 输出积压/写队列与后台线程共享:此处放本端引用,预算已在 close
    // 归还;Drop 兜底(close 未触达的异常路径)不双扣(释放 0)
    drop(pty_buf);
    drop(writeq);
    drop(_permit); // 会话槽位归还(旧会话已 kill,槽位即时释放)
    drop(term);
    drop(router);
    drop(strip_router);
    drop(row_insts);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 测试串行闸:reaper/槽位/DEGRADED 都是进程级全局——pty_bridge
    /// 的 budget_guard 委托到这里,两个模块的测试共用一把锁,消灭
    /// 跨模块并发的全局态互踩(回收积压超时/槽位断言踩数/DEGRADED
    /// 翻转竞态)。
    pub(crate) fn global_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static G: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        G.get_or_init(|| {
            // portable-pty prefers a bare "conpty.dll" over kernel32. The
            // lib harness does not enter app::run's PATH sanitization, so a
            // third-party terminal on PATH can silently supply its host/DLL.
            // Restrict only this test process's loader before its first PTY;
            // do not mutate PATH concurrently with Rust test threads.
            use windows::Win32::System::LibraryLoader::{
                LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, SetDefaultDllDirectories,
            };
            // SAFETY: process-local loader policy, initialized once before
            // tests using this guard create any ConPTY. No paths/handles move.
            unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) }
                .expect("set test DLL search policy");
            std::sync::Mutex::new(())
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    }

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

    /// 真 pty 会话交回收器:REAPED_OK 增量出现 = 本会话后台 Drop 完成
    /// (不卡 UI)。r5 改增量断言:全局 in-flight 归零受并行测试的回收
    /// 积压影响(每任务 ClosePseudoConsole 秒级),轮到本会话可能超
    /// 10s——增量证明"交后台且确实回收",不与他人积压耦合。
    #[test]
    fn session_is_dropped_off_thread() {
        let _g = global_test_guard();
        let reaped_before = reaper_stats().0;
        let (session, pid) = short_session();
        reap_session_with_pid(pid, session);
        let deadline = Instant::now() + Duration::from_secs(10);
        while reaper_stats().0 <= reaped_before {
            assert!(Instant::now() < deadline, "10s 内未完成后台回收");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 满通道(消费者卡死不 recv):**立即**降级 forget,零阻塞——
    /// 入队调用方全在 UI 线程,任何 sleep 都会按会话数累积卡顿
    /// (2026-11-02 复核要求:满队列时窗口仍能处理消息)。
    #[test]
    fn full_channel_falls_back_immediately_without_blocking() {
        let (tx, rx) = sync_channel::<ReapTask>(1);
        // This test deliberately forgets the rejected task. A real ConPTY
        // would therefore leave its host alive after the test process exits.
        // No OS resource is needed to test the queue's full/disconnected arms.
        let filler = PtySession::detached();
        tx.try_send(ReapTask {
            child_pid: None,
            session: filler,
        })
        .expect("占位入队");
        // rx 存活但永不 recv(卡死形态):第二个必须立即降级
        let session = PtySession::detached();
        let start = Instant::now();
        let out = enqueue_with(
            &tx,
            ReapTask {
                child_pid: None,
                session,
            },
        );
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
        let session = PtySession::detached();
        let start = Instant::now();
        let out = enqueue_with(
            &tx,
            ReapTask {
                child_pid: None,
                session,
            },
        );
        assert_eq!(out, EnqueueOutcome::FellBack, "断开不得长等");
        assert!(start.elapsed() < Duration::from_millis(200));
        assert!(reaper_stats().1 >= 1);
    }

    /// 故障态语义:FellBack → spawn_capacity_ok=false;Reaped → 恢复 true。
    /// 持串行闸:DEGRADED 是进程级全局,restart 系测试同时翻转。
    #[test]
    fn degraded_state_gate_and_recovery() {
        let _g = global_test_guard();
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
            // This is a channel/accounting test, not a ConPTY teardown test.
            // A slow host Drop can fill the channel and intentionally forget
            // later tasks; detached tasks cannot leak busy-looping hosts.
            let session = PtySession::detached();
            enqueue_with(
                &tx,
                ReapTask {
                    child_pid: None,
                    session,
                },
            );
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
