//! 有界 PTY 桥(T08/F10,2026-11-02;2026-11-03 二次返工):
//! **零丢弃背压 + 严格守恒预算 + 会话隔离的写队列 + 独立唤醒重试**。
//!
//! 二次返工否决的形态(复核七条):
//! - enqueue 不唤醒 writer(入队成功后不 notify,writer 睡死);
//! - UI 调阻塞 enqueue——队列满即卡 UI,同线程连关闭都做不了;
//! - SessionSink 持会话锁做阻塞写,close/kill 同锁死锁;
//! - 写队列按"弹后再验代"——旧 writer 会持续丢弃新会话数据;
//! - pending_paste 无界、可乱序、静默 shell 下停灌;
//! - 唤醒重试挂在转发线程上——背压堵 push / EOF 退场后无人重试。
//!
//! 现行契约:
//! 1. **零丢弃输出链**:`PtyBacklog::push` 满则阻塞等待(pane 满/进程
//!    满分别等待),close/换代取消;与 core 源通道构成完整背压链。
//! 2. **严格守恒预算**:持锁判限/加减,按实际字节记账;Drop/close/
//!    换代各释放一次,debug 断言防减穿。上游界:源通道 512KB×N +
//!    在途 ≤8KB×N,N 由 `MAX_SESSIONS` 上限约束(见 lifecycle)。
//! 3. **会话隔离的写队列**:`WriteQueue` 每会话一个,随会话创建/关闭
//!    ——不存在跨代共享,旧 writer 只面对旧队列(关闭即退场),绝不
//!    触碰新队列。**UI 侧全非阻塞**:`try_push` 装多少收多少,装不下
//!    的进调用方的有界输入暂存(`InputStage`,统一 FIFO,消灭按键
//!    越过粘贴欠账的乱序)。
//! 4. **独立唤醒重试**:唤醒状态机区分"已投递待消费"与"待重试"
//!    (retry_needed 只在投递失败时置位);专职 monitor 线程周期补投,
//!    不依赖转发线程(背压堵 push / EOF 退场后仍自愈)。
//! 5. 会话控制面(杀子/resize/轮询)与写端锁分离:core `PtySession`
//!    的 writer 独立 Arc 互斥,`PtyWriterHandle` 只持写端——阻塞写
//!    永远卡不到 kill_child。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// 单 pane 应用缓冲预算(字节)。
pub const PANE_BUDGET_BYTES: usize = 2 * 1024 * 1024;
/// 全进程应用缓冲预算(字节;严格上限,持锁判限)。
pub const PROCESS_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
/// UI 单次调度批上限(字节)。
pub const UI_BATCH_BYTES: usize = 256 * 1024;
/// 单会话写队列预算(字节)。
pub const WRITE_QUEUE_BYTES: usize = 1024 * 1024;
/// 单 pane 输入暂存预算(字节):写队列满时的 UI 侧有界欠账。
pub const INPUT_STAGE_BYTES: usize = 512 * 1024;

// ---------------------------------------------------------------------------
// 全局预算(严格:持锁判限 + 取消检查持同一把锁,杜绝丢唤醒)
// ---------------------------------------------------------------------------

struct Budget {
    held: Mutex<u64>,
    space: Condvar,
}

static BUDGET: Budget = Budget {
    held: Mutex::new(0),
    space: Condvar::new(),
};

/// 当前全进程已预留字节(测试与诊断面)。
pub fn reserved_bytes() -> u64 {
    *BUDGET.held.lock().unwrap_or_else(|e| e.into_inner())
}

impl Budget {
    /// 阻塞预留。取消检查发生在**持有预算锁**期间:close/换代的
    /// notify_all 也要拿同一把锁——检查与进入等待之间不存在无锁窗口,
    /// 丢唤醒竞态在锁序层面排除(复核 8 的结构性回答)。
    fn reserve_blocking(&self, need: u64, cancelled: &dyn Fn() -> bool) -> bool {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if cancelled() {
                return false;
            }
            if *held + need <= PROCESS_BUDGET_BYTES {
                *held += need;
                return true;
            }
            held = self.space.wait(held).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn release(&self, n: u64) {
        if n == 0 {
            return;
        }
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert!(
            *held >= n,
            "预算守恒被破坏:held={held} 释放 {n}(重复释放或多余释放)"
        );
        *held = held.saturating_sub(n);
        self.space.notify_all();
    }

    /// **持锁通知**(丢唤醒屏障):等待者在持 `held` 期间检查取消谓词,
    /// `wait()` 原子释放锁挂起;通知方先取 `held` 再 notify——若等待者
    /// 刚检查完尚未挂起,通知方阻塞在锁上直到其挂起,通知必然送达。
    /// 修前无锁 notify 在"检查后、挂起前"窗口发射,受控交错复现丢失。
    fn notify_all(&self) {
        let _held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        self.space.notify_all();
    }
}

// ---------------------------------------------------------------------------
// 唤醒面
// ---------------------------------------------------------------------------

/// 唤醒投递面:返回 false = 投递失败(队列满/窗口死),进入重试。
pub trait WakeSink: Send + Sync {
    fn wake(&self, pane_id: u64) -> bool;
}

/// 生产实现:向窗口投渲染消息。
pub struct WindowWake {
    pub slot: crate::app::SharedHwnd,
    pub msg: u32,
}

impl WakeSink for WindowWake {
    fn wake(&self, pane_id: u64) -> bool {
        let hwnd = *self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let Some(raw) = hwnd else {
            return false;
        };
        // SAFETY: hwnd 来自登记窗口;Post 失败即 false,交由重试
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(windows::Win32::Foundation::HWND(
                    raw as *mut std::ffi::c_void,
                )),
                self.msg,
                windows::Win32::Foundation::WPARAM(pane_id as usize),
                windows::Win32::Foundation::LPARAM(0),
            )
        }
        .is_ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    Closed,
    StaleGeneration,
}

struct BlState {
    data: Vec<u8>,
    /// 一条 wake 在途(已投递待消费)或待重试。消费批清标记。
    wake_pending: bool,
    /// 投递失败待重试。与 wake_pending 同在 state 锁——monitor 的
    /// "读→补投→清位"与 arm 的"投递→置位"串行化,旧成功不清新失败。
    retry_needed: bool,
}

/// 单 pane 输出积压:转发线程 `push`(阻塞,可取消),UI `drain_batch`。
pub struct PtyBacklog {
    pane_id: u64,
    pane_cap: usize,
    wake: Arc<dyn WakeSink>,
    closed: AtomicBool,
    cur_gen: AtomicU64,
    state: Mutex<BlState>,
    space: Condvar,
}

impl PtyBacklog {
    pub fn new(pane_id: u64, pane_cap: usize, wake: Arc<dyn WakeSink>) -> Arc<Self> {
        Arc::new(Self {
            pane_id,
            pane_cap,
            wake,
            closed: AtomicBool::new(false),
            cur_gen: AtomicU64::new(0),
            state: Mutex::new(BlState {
                data: Vec::new(),
                wake_pending: false,
                retry_needed: false,
            }),
            space: Condvar::new(),
        })
    }

    /// 专职唤醒重试线程:独立于转发线程(背压堵 push / EOF 退场后仍
    /// 自愈);仅在 retry_needed 时补投,已成功投递的绝不重发。
    /// 会话关闭(close)后自然退场。
    pub fn spawn_monitor(self: &Arc<Self>) {
        let b = Arc::clone(self);
        std::thread::Builder::new()
            .name("wake-monitor".into())
            .spawn(move || {
                loop {
                    if b.closed.load(Ordering::Acquire) {
                        return;
                    }
                    // 整个"读 retry→补投→清位"在 state 锁内:与
                    // arm_wake 的"投递→置位"互斥,旧成功不清新失败(复核 6)
                    {
                        let mut st = b.state.lock().unwrap_or_else(|e| e.into_inner());
                        if st.retry_needed && !st.data.is_empty() && b.wake.wake(b.pane_id) {
                            st.retry_needed = false;
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
            })
            .expect("spawn wake monitor");
    }

    fn cancelled(&self, epoch: u64) -> bool {
        self.closed.load(Ordering::Acquire) || self.cur_gen.load(Ordering::Acquire) != epoch
    }

    /// 转发线程追加:满则阻塞,close/换代取消;绝不截尾。
    pub fn push(&self, epoch: u64, chunk: &[u8]) -> Result<(), PushError> {
        if chunk.is_empty() {
            return Ok(());
        }
        loop {
            // 阶段一(持 state 锁):pane 容量等待(锁在块尾释放——预算
            // 等待绝不持 state 锁,否则 drain 死锁)
            {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if self.cancelled(epoch) {
                        return Err(self.err_kind());
                    }
                    if st.data.len() + chunk.len() <= self.pane_cap {
                        break;
                    }
                    st = self.space.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            }
            // 阶段二(无锁):进程预算阻塞预留,取消检查持预算锁
            if !BUDGET.reserve_blocking(chunk.len() as u64, &|| self.cancelled(epoch)) {
                return Err(self.err_kind());
            }
            // 阶段三(持 state 锁):入队 + 二次复核
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if self.cancelled(epoch) {
                BUDGET.release(chunk.len() as u64);
                return Err(self.err_kind());
            }
            if st.data.len() + chunk.len() > self.pane_cap {
                BUDGET.release(chunk.len() as u64);
                continue;
            }
            st.data.extend_from_slice(chunk);
            self.arm_wake_locked(&mut st);
            return Ok(());
        }
    }

    fn err_kind(&self) -> PushError {
        if self.closed.load(Ordering::Acquire) {
            PushError::Closed
        } else {
            PushError::StaleGeneration
        }
    }

    /// 装配唤醒(调用方持 state 锁;投递动作本身不持锁——Post 无锁回
    /// 调风险为零):pending(在途或待重试)则合并;失败置 retry_needed。
    fn arm_wake_locked(&self, st: &mut BlState) {
        if st.wake_pending {
            return;
        }
        st.wake_pending = true;
        let ok = self.wake.wake(self.pane_id);
        if !ok {
            st.retry_needed = true;
        }
    }

    /// UI 取一批(≤ max)。清 pending/retry → 释放预算 → 复查补通知。
    pub fn drain_batch(&self, max: usize) -> Option<Vec<u8>> {
        let bytes = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.data.is_empty() {
                st.wake_pending = false;
                st.retry_needed = false;
                return None;
            }
            let take = st.data.len().min(max);
            let out: Vec<u8> = st.data.drain(..take).collect();
            st.wake_pending = false;
            st.retry_needed = false;
            out
        };
        BUDGET.release(bytes.len() as u64);
        self.space.notify_all();
        if self.has_more() {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.arm_wake_locked(&mut st);
        }
        Some(bytes)
    }

    /// 测试/诊断面:是否有待重试的唤醒。
    #[allow(dead_code)]
    pub fn retry_pending(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retry_needed
    }

    pub fn has_more(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .data
            .is_empty()
    }

    /// 会话换代:**清空与代递增在同一同步边界(state 锁)内完成**——
    /// 旧代 push 要么在锁内看到新代被拒,要么在递增前完成入队而被
    /// 本次 clear 一并清掉,不存在"验旧代通过→换代→旧数据入新纪元"
    /// 的窗口。预算与全部等待者在锁外统一释放/唤醒。
    pub fn bump_generation(&self) -> u64 {
        // 清空 + 代递增在同一 state 锁临界区内(原子边界,见 doc)
        let released = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let n = st.data.len() as u64;
            st.data.clear();
            st.wake_pending = false;
            st.retry_needed = false;
            let g = self.cur_gen.fetch_add(1, Ordering::AcqRel) + 1;
            BUDGET.release(n); // 释放可与锁内递增同界:预算锁在 state 锁外层无持有,安全
            g
        };
        BUDGET.notify_all();
        self.space.notify_all();
        released
    }

    /// 会话终结:关闭 + 释放存量 + 唤醒全部等待者。
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let released = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let n = st.data.len() as u64;
            st.data.clear();
            st.wake_pending = false;
            st.retry_needed = false;
            n
        };
        BUDGET.release(released);
        BUDGET.notify_all();
        self.space.notify_all();
    }
}

impl Drop for PtyBacklog {
    fn drop(&mut self) {
        let n = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.data.len() as u64
        };
        BUDGET.release(n);
        BUDGET.notify_all();
    }
}

// ---------------------------------------------------------------------------
// 会话隔离的写队列(每会话一个;UI 全非阻塞)
// ---------------------------------------------------------------------------

struct WqState {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
}

pub struct WriteQueue {
    cap: usize,
    closed: AtomicBool,
    /// writer 累计弹出(=已交给 write_all 的字节;GUI 验收守恒断言用)。
    popped_total: AtomicU64,
    state: Mutex<WqState>,
    space: Condvar,
}

impl WriteQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cap: WRITE_QUEUE_BYTES,
            closed: AtomicBool::new(false),
            popped_total: AtomicU64::new(0),
            state: Mutex::new(WqState {
                queue: VecDeque::new(),
                bytes: 0,
            }),
            space: Condvar::new(),
        })
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// UI 非阻塞入队:装多少收多少,返回**接收的字节数**(装不下的
    /// 由调用方暂存)。关闭后接收 0。入队成功即唤醒 writer(复核 1:
    /// 旧 enqueue 不 notify,writer 睡死)。
    pub fn try_push(&self, bytes: &[u8]) -> usize {
        if bytes.is_empty() || self.is_closed() {
            return 0;
        }
        let accepted = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let room = self.cap.saturating_sub(st.bytes);
            let take = room.min(bytes.len());
            if take == 0 {
                return 0;
            }
            st.bytes += take;
            st.queue.push_back(bytes[..take].to_vec());
            take
        };
        self.space.notify_all(); // 唤醒 writer(成功入队必通知)
        accepted
    }

    /// writer 取一块(阻塞;None = 关闭且取空)。
    fn pop_blocking(&self) -> Option<Vec<u8>> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(front) = st.queue.pop_front() {
                st.bytes -= front.len();
                // popped_total **不在弹出时计**(复核 r5-3):弹出只证明
                // writer 取走,不证明写入成功——计数在 writer_loop 的
                // write_all 成功后由 note_written() 增加
                self.space.notify_all(); // 腾出容量:通知续灌侧
                return Some(front);
            }
            if self.is_closed() {
                return None;
            }
            st = self.space.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// 关闭:writer 在取空后自然退场;新数据一律拒收。
    pub fn close(&self) {
        {
            // 持锁置位(丢唤醒屏障,与 Budget.notify_all 同理)
            let _st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.closed.store(true, Ordering::Release);
        }
        self.space.notify_all();
    }

    #[allow(dead_code)]
    pub fn is_closed_pub(&self) -> bool {
        self.is_closed()
    }

    #[allow(dead_code)]
    /// writer 累计弹出字节(守恒断言:stage+queued+popped == 已接收)。
    #[allow(dead_code)]
    pub fn popped_total(&self) -> u64 {
        self.popped_total.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub fn queued_bytes(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).bytes
    }

    #[allow(dead_code)]
    pub fn free_bytes(&self) -> usize {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.cap.saturating_sub(st.bytes)
    }

    /// writer 报告一段写出成功(在 write_all 返回 Ok 后调用)。
    /// popped_total 的语义 = "已成功交给 pty 管道"。
    fn note_written(&self, bytes: usize) {
        self.popped_total.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// writer 线程主体:独占消费写队列写 pty(经 `sink`)。写失败(管道
/// 断)→ 关队列。**每个会话一个队列一个 writer**——旧 writer 只面对
/// 旧队列,绝不接触新会话的数据(复核 4:按代筛选会误删新数据)。
pub fn writer_loop(queue: Arc<WriteQueue>, sink: Arc<dyn BytesSink + Send + Sync>) {
    while let Some(item) = queue.pop_blocking() {
        if let Err(e) = sink.write_all(&item) {
            queue.close();
            eprintln!("[pty-writer] 写失败(管道断?):{e}");
            return;
        }
        // 写出成功才计数(不是弹出时计)——popped 的语义是"已交给
        // popped 的语义 = "已成功交给 pty 管道"(不是"writer 取走了")
        queue.note_written(item.len());
    }
}

/// 写目标抽象(生产 = core PtyWriterHandle;测试 = 可挂起假件)。
pub trait BytesSink: Send + Sync {
    fn write_all(&self, bytes: &[u8]) -> std::io::Result<()>;
}

/// 生产适配器:**只持写端句柄,不持会话**——阻塞写只卡写端互斥,
/// kill/resize/轮询走会话自己的字段,永不被写阻塞拖住(复核 3)。
pub struct SessionSink(pub mica_core::pty::PtyWriterHandle);

impl BytesSink for SessionSink {
    fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write(bytes)
    }
}

// ---------------------------------------------------------------------------
// UI 侧有界输入暂存(统一 FIFO:按键/DSR/IPC/粘贴同序,消灭乱序)
// ---------------------------------------------------------------------------

/// UI 输入流的结构化结果(复核 4:区分 Closed / 容量不足 / 已接受,
/// 不再用"返回拒收字节数"折叠语义)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFlow {
    /// 全部字节已进写队列(writer 将写出)。
    Accepted,
    /// 已接收但部分/全部在有界暂存中(返回当前欠账字节数)。
    /// **不是失败**——心跳续灌会推进;但 Applied 语义只属于 Accepted。
    Debt(usize),
    /// 暂存预算已满,字节被拒收(计数见 overflow_drops)。
    RejectedOverflow,
    /// 会话已关闭(旧代/已终结)——字节不可投递。
    RejectedClosed,
}

/// 因暂存溢出被拒收的字节累计(诊断面:退出日志可见,不静默)。
static OVERFLOW_DROPS: AtomicU64 = AtomicU64::new(0);

/// 诊断面:暂存溢出累计拒收字节。
pub fn overflow_drops() -> u64 {
    OVERFLOW_DROPS.load(Ordering::Relaxed)
}

/// **生产输入入口的共享核心**(app::stage_input 与测试共用,复核 7:
/// 测试必须打生产路径而非手工复刻理想序)。顺序规则(复核 2):
/// 暂存非空(存在旧欠账)时,**新输入只追加到暂存尾部**——绝不
/// try_push 绕过(writer 腾出的空间只允许 flush 消费旧欠账使用,
/// 否则新输入越过旧欠账造成乱序,复核探针实测 NEW→OLD)。
pub fn stage_into(stage: &mut InputStage, queue: &WriteQueue, bytes: &[u8]) -> InputFlow {
    if bytes.is_empty() {
        return if queue.is_closed() {
            InputFlow::RejectedClosed
        } else {
            InputFlow::Accepted
        };
    }
    if queue.is_closed() {
        return InputFlow::RejectedClosed;
    }
    // **事务语义(复核 2/3)**:先按与提交完全相同的容量规则预判,
    // 装不下 → 整体拒绝且**零副作用**(队列/暂存原样)。容量规则与
    // 提交路径同源:暂存空 = 队列空余+暂存空余;暂存非空(旧欠账
    // 在,FIFO 禁绕过)= 仅暂存空余。
    let capacity = if stage.is_empty() {
        queue.free_bytes() + stage.free_room()
    } else {
        stage.free_room()
    };
    if bytes.len() > capacity {
        OVERFLOW_DROPS.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        return InputFlow::RejectedOverflow;
    }
    // 容量内:提交必然完整落位(不再有部分接收后溢出的形态)
    if stage.is_empty() {
        let acc = queue.try_push(bytes);
        if acc == bytes.len() {
            return InputFlow::Accepted;
        }
        // **关闭竞态**(复核 r5-1):入口的 closed 检查与 try_push 之间
        // writer 可能已 close——try_push 返回 0 且 acc==0 时复查;若
        // 已关闭,**整体拒绝零副作用**(队列未收任何字节,暂存未动)。
        // 修前直接塞暂存:600000 字节塞 512KiB 暂存,debug 断言 panic,
        // release 丢 75712 字节还报 Debt(假接收)
        if acc == 0 && queue.is_closed() {
            return InputFlow::RejectedClosed;
        }
        let rest = &bytes[acc..];
        let rejected = stage.stage(rest);
        debug_assert_eq!(
            rejected, 0,
            "事务预判通过后不允许部分拒收(容量计算与提交不一致的 bug)"
        );
        return InputFlow::Debt(stage.len());
    }
    // 旧欠账在:只追加,不绕过。追加前同样复查关闭(旧欠账在暂存,
    // 关闭后 clear() 也会清——但新字节在已关闭会话里没有意义)
    if queue.is_closed() {
        return InputFlow::RejectedClosed;
    }
    let rejected = stage.stage(bytes);
    debug_assert_eq!(rejected, 0, "事务预判通过后不允许部分拒收");
    InputFlow::Debt(stage.len())
}

/// CLI/测试用的阻塞推进(生产 UI/IPC 已改异步 completer,不再调用)。
/// 期限语义:Ok = 全部进队列;Err(Debt) = 期限未尽(结果未知——字节
/// 已接收,仍会随后续排水写出,调用方按"仍在推进"报告,不得重发);
/// Err(RejectedClosed/Overflow) = 未接收任何字节。内部按 64KiB 分块
/// 提交,大输入在事务语义下也能渐进(每块整体接受或整体拒绝)。
#[cfg_attr(not(test), allow(dead_code))]
pub fn drain_into(
    stage: &mut InputStage,
    queue: &WriteQueue,
    bytes: &[u8],
    deadline: std::time::Instant,
) -> Result<(), InputFlow> {
    const CHUNK: usize = 64 * 1024;
    for chunk in bytes.chunks(CHUNK) {
        // 先冲旧欠账(块提交前尽量腾暂存)
        while !stage.is_empty() {
            if std::time::Instant::now() >= deadline {
                return Err(InputFlow::Debt(stage.len()));
            }
            if queue.is_closed() {
                return Err(InputFlow::RejectedClosed);
            }
            stage.flush_into(queue);
            if !stage.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        match stage_into(stage, queue, chunk) {
            InputFlow::Accepted | InputFlow::Debt(_) => {}
            InputFlow::RejectedClosed => return Err(InputFlow::RejectedClosed),
            InputFlow::RejectedOverflow => return Err(InputFlow::RejectedOverflow),
        }
    }
    // 尾段欠账清空
    while !stage.is_empty() {
        if std::time::Instant::now() >= deadline {
            return Err(InputFlow::Debt(stage.len()));
        }
        if queue.is_closed() {
            return Err(InputFlow::RejectedClosed);
        }
        stage.flush_into(queue);
        if !stage.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    Ok(())
}

/// 输入暂存:写队列满时的有界欠账。语义:
/// - `stage`:追加(非阻塞;超预算拒收尾部并返回拒收字节数);
/// - `flush_into`:把队头灌进写队列(装多少灌多少,余下保序留待下轮);
/// - 会话关闭/换代:`clear`(旧会话的欠账不污染新会话)。
///
/// 顺序纪律见 `stage_into`(生产入口,勿直接绕过)。
pub struct InputStage {
    data: Vec<u8>,
    cap: usize,
    /// 累计灌入写队列的字节(GUI 验收观测面:MICA_DEBUG_INPUTFLOW 日志)。
    flushed_total: std::cell::Cell<u64>,
}

impl InputStage {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            cap: INPUT_STAGE_BYTES,
            flushed_total: std::cell::Cell::new(0),
        }
    }

    /// 累计灌入写队列的字节(GUI 验收:最终应等于全部已接收输入)。
    pub fn flushed_total(&self) -> u64 {
        self.flushed_total.get()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// 非阻塞追加:返回拒收字节数(超出预算的尾部)。**永不阻塞 UI**。
    pub fn stage(&mut self, bytes: &[u8]) -> usize {
        let room = self.cap.saturating_sub(self.data.len());
        let take = room.min(bytes.len());
        self.data.extend_from_slice(&bytes[..take]);
        bytes.len() - take
    }

    /// 灌入写队列(保序:队头先走)。返回仍剩的暂存量。
    pub fn flush_into(&mut self, q: &WriteQueue) -> usize {
        if self.data.is_empty() {
            return 0;
        }
        let accepted = q.try_push(&self.data);
        if accepted > 0 {
            self.flushed_total
                .set(self.flushed_total.get() + accepted as u64);
            self.data.drain(..accepted);
        }
        self.data.len()
    }

    /// 会话终结/换代:旧欠账作废。
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// 尚可追加的字节数(粘贴整-fit 预检用)。
    pub fn free_room(&self) -> usize {
        self.cap.saturating_sub(self.data.len())
    }

    /// 测试面:暂存内容的只读视图(零副作用断言用)。
    #[cfg(test)]
    pub fn last_bytes(&self) -> &[u8] {
        &self.data
    }
}

impl Default for InputStage {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
