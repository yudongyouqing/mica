//! 有界 PTY 桥(T08/F10,2026-11-02;同日按复核返工):
//! **可取消背压、正常终端字节零丢弃**。
//!
//! 复核否决的初稿形态:背压靠"截尾丢字节"——保头不保协议(丢 UTF-8
//! 后半段/序列终止符/DSR/2026 结束符,后续拼接仍损坏状态);进程预算
//! load-then-add 重复累计且非严格限额;每块 Post 未合并;写路径仍同步。
//!
//! 现行契约:
//! 1. **零丢弃**:`PtyBacklog::push` 在预算满时**阻塞等待容量**(pane 满
//!    等 pane 消费,进程满等任意释放),可被 close/换代取消并返回错误;
//!    绝不截尾。上游(mica-core `sync_channel`)与下游(UI 批消费)构成
//!    完整背压链:缓冲满 → 转发线程在 push 阻塞 → 源通道渐满 → 读线程
//!    在 send 阻塞 → ConPTY 管道顶住 → shell 暂停产出。
//! 2. **严格预算**:`Budget` 全局互斥 + 条件变量,持锁判限/加减——
//!    多 pane 并发下合计严格 ≤ 上限;记账按**实际新增/移除字节**守恒:
//!    push 预留 chunk.len、drain 释放取出量、clear/换代/关闭/Drop 释放
//!    存量且仅一次(状态锁内单点转换)。
//! 3. **预算范围**:应用缓冲(本模块)pane 2 MiB/进程 32 MiB 严格受限;
//!    上游每 pane 源通道 64×8KB=512KB + 转发在途 ≤8KB 是 core 侧已知
//!    界(文档化,不重复计量)。最坏全进程 = 32MiB + 16×(512KB+8KB)
//!    ≈ 40.6 MiB。
//! 4. **唤醒合并**:每 pane 至多一个 pending wake;push 装配(已 pending
//!    则合并)、消费后"清标记→复查 has_more→补通知"、Post 失败由转发
//!    线程轮询 `retry_wake` 恢复(最后一块失败且再无输出也能补上)。
//! 5. **单 writer 写队列**:`WriteQueue` 有界保序(输入/粘贴/IPC 注入/
//!    协议回复同队),writer 线程独占写 pty——shell 不读输入时**UI 不卡**
//!    (enqueue 满则即时返回欠账,由消息循环分批续灌);关闭/换代唤醒
//!    全部等待者。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// 单 pane 应用缓冲预算(字节)。
pub const PANE_BUDGET_BYTES: usize = 2 * 1024 * 1024;
/// 全进程应用缓冲预算(字节;严格上限,Budget 持锁判限)。
pub const PROCESS_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
/// UI 单次调度批上限(字节):超此让出消息循环续批。
pub const UI_BATCH_BYTES: usize = 256 * 1024;
/// 单 pane 写队列预算(字节):粘贴/注入的待写字节上限。
pub const WRITE_QUEUE_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// 全局预算(严格:持锁判限,绝不 load-then-add)
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
    /// 阻塞预留:超限等待,直到有空间或 `cancelled` 为真。返回 false =
    /// 被取消(close/换代)。持锁判限——多 pane 并发下合计严格 ≤ 上限。
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
            // close/换代侧也 notify_all,等待者立刻醒来复查取消
            held = self.space.wait(held).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// 释放(守恒:debug 断言不减穿——重复释放是记账 bug 的显式失败)。
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

    /// 唤醒全部等待者(close/换代用——被取消的等待者必须立即脱身)。
    fn notify_all(&self) {
        self.space.notify_all();
    }
}

// ---------------------------------------------------------------------------
// 唤醒面(生产 = 窗口消息;测试注入假件)
// ---------------------------------------------------------------------------

/// 唤醒投递面:返回 false = 投递失败(队列满/窗口死),调用方进入重试。
pub trait WakeSink: Send + Sync {
    fn wake(&self, pane_id: u64) -> bool;
}

/// 生产实现:向窗口投渲染消息(读 SharedHwnd 哨兵;窗口不在 = false)。
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

// ---------------------------------------------------------------------------
// 有界输出积压(零丢弃)
// ---------------------------------------------------------------------------

/// push/enqueue 的失败形态(取消原因)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// 已关闭(会话终结):调用方(转发线程)退场
    Closed,
    /// 会话已换代:本字节属旧会话,调用方按代退场
    StaleGeneration,
}

struct BlState {
    data: Vec<u8>,
    /// 唤醒合并标记:一条 wake 消息在途或待重试。消费批清标记。
    wake_pending: bool,
}

/// 单 pane 输出积压:转发线程 `push`(阻塞等待容量,可取消),UI
/// `drain_batch`。drop 释放全部预留(守恒)。
pub struct PtyBacklog {
    pane_id: u64,
    pane_cap: usize,
    wake: Arc<dyn WakeSink>,
    // 取消面(原子,与 state 锁分离——预算等待的取消检查不得拿 state 锁,
    // 否则与 drain 的 state 使用构成 ABBA)
    closed: AtomicBool,
    cur_gen: AtomicU64,
    state: Mutex<BlState>,
    /// pane 满等待(drain/close/bump 唤醒)
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
            }),
            space: Condvar::new(),
        })
    }

    fn cancelled(&self, epoch: u64) -> bool {
        self.closed.load(Ordering::Acquire) || self.cur_gen.load(Ordering::Acquire) != epoch
    }

    /// 转发线程追加一块。预算满则**阻塞等待**(pane 满等本 pane 消费,
    /// 进程满等任意 pane 释放),close/换代取消并返回错误;**绝不截尾**。
    pub fn push(&self, epoch: u64, chunk: &[u8]) -> Result<(), PushError> {
        if chunk.is_empty() {
            return Ok(());
        }
        loop {
            // ---- 阶段一(持 state 锁):取消检查 + pane 容量等待 ----
            // 锁在块尾释放:预算等待(阶段二)绝不能持有 state 锁,
            // 否则 drain 拿不到锁 → 死锁(首版 drop(&st) 只丢引用没丢锁,
            // 并发限额测试即死锁于此)
            {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if self.cancelled(epoch) {
                        return Err(self.err_kind());
                    }
                    if st.data.len() + chunk.len() <= self.pane_cap {
                        break; // 有 pane 空间,释放锁去抢进程预算
                    }
                    // pane 满:等本 pane 消费(drain/close/bump 都会唤醒)
                    st = self.space.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            }
            // ---- 阶段二(无锁):进程预算阻塞预留,取消面是原子读 ----
            if !BUDGET.reserve_blocking(chunk.len() as u64, &|| self.cancelled(epoch)) {
                return Err(self.err_kind());
            }
            // ---- 阶段三(持 state 锁):入队 + 二次复核 ----
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if self.cancelled(epoch) {
                BUDGET.release(chunk.len() as u64); // 预留与入队之间被取消:归还
                return Err(self.err_kind());
            }
            if st.data.len() + chunk.len() > self.pane_cap {
                // 消费者在我们放锁到抢到预算的窗口没放水反而……不可能
                // (单生产者只增),防御性重来
                BUDGET.release(chunk.len() as u64);
                continue;
            }
            st.data.extend_from_slice(chunk);
            self.arm_wake(&mut st);
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

    /// 装配唤醒:pending 未置则投一条(已 pending = 合并);投失败留
    /// pending 位,由 `retry_wake` 恢复。
    fn arm_wake(&self, st: &mut BlState) {
        if st.wake_pending {
            return;
        }
        st.wake_pending = true;
        let _ = self.wake.wake(self.pane_id); // 失败:pending 保持,重试接手
    }

    /// 投递失败的重试(转发线程在源通道空闲期轮询):pending 在且还有
    /// 数据就再投——最后一块 Post 失败且再无输出也能在这里补上。
    pub fn retry_wake(&self) {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.wake_pending && !st.data.is_empty() {
            let _ = self.wake.wake(self.pane_id);
        }
    }

    /// UI 取一批(≤ max 字节)。清 pending 标记 → 释放预算 → 复查
    /// has_more 补通知(push 可能恰在取走与清标记之间被合并)。
    pub fn drain_batch(&self, max: usize) -> Option<Vec<u8>> {
        let bytes = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.data.is_empty() {
                st.wake_pending = false; // 空转唤醒:标记一并清掉
                return None;
            }
            let take = st.data.len().min(max);
            let out: Vec<u8> = st.data.drain(..take).collect();
            st.wake_pending = false;
            out
        };
        BUDGET.release(bytes.len() as u64);
        self.space.notify_all(); // pane 满的 push 可以继续
        if self.has_more() {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.arm_wake(&mut st);
        }
        Some(bytes)
    }

    pub fn has_more(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .data
            .is_empty()
    }

    /// 会话换代:释放存量、数据清空、代 +1,唤醒所有等待者(旧代 push
    /// 以 StaleGeneration 脱身)。
    pub fn bump_generation(&self) -> u64 {
        let released = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let n = st.data.len() as u64;
            st.data.clear();
            st.wake_pending = false;
            n
        };
        BUDGET.release(released);
        let next = self.cur_gen.fetch_add(1, Ordering::AcqRel) + 1;
        BUDGET.notify_all();
        self.space.notify_all();
        next
    }

    /// 会话终结:关闭 + 释放存量 + 唤醒等待者。此后 push 一律 Err。
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let released = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let n = st.data.len() as u64;
            st.data.clear();
            st.wake_pending = false;
            n
        };
        BUDGET.release(released);
        BUDGET.notify_all();
        self.space.notify_all();
    }
}

impl Drop for PtyBacklog {
    fn drop(&mut self) {
        // 非空 Drop 归还预算(close/bump 已清空则释放 0——守恒不双扣)
        let n = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.data.len() as u64
        };
        BUDGET.release(n);
        BUDGET.notify_all();
    }
}

// ---------------------------------------------------------------------------
// 单 writer 有界保序写队列
// ---------------------------------------------------------------------------

struct WqState {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
}

/// 单 pane 写队列:UI 侧 enqueue(有界,满则等待或即时欠账),writer
/// 线程独占消费写 pty。保序;关闭/换代唤醒全部等待者。
pub struct WriteQueue {
    cap: usize,
    closed: AtomicBool,
    cur_gen: AtomicU64,
    state: Mutex<WqState>,
    space: Condvar,
}

impl WriteQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cap: WRITE_QUEUE_BYTES,
            closed: AtomicBool::new(false),
            cur_gen: AtomicU64::new(0),
            state: Mutex::new(WqState {
                queue: VecDeque::new(),
                bytes: 0,
            }),
            space: Condvar::new(),
        })
    }

    fn cancelled(&self, epoch: u64) -> bool {
        self.closed.load(Ordering::Acquire) || self.cur_gen.load(Ordering::Acquire) != epoch
    }

    fn err_kind(&self) -> PushError {
        if self.closed.load(Ordering::Acquire) {
            PushError::Closed
        } else {
            PushError::StaleGeneration
        }
    }

    /// 入队(阻塞至有空间;close/换代取消)。小写入(按键/DSR 回复/
    /// IPC 注入)用它——容量充裕,等待极短且总可被关闭唤醒。
    pub fn enqueue(&self, epoch: u64, bytes: &[u8]) -> Result<(), PushError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.cancelled(epoch) {
                return Err(self.err_kind());
            }
            if st.bytes + bytes.len() <= self.cap {
                st.bytes += bytes.len();
                st.queue.push_back(bytes.to_vec());
                return Ok(());
            }
            st = self.space.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// UI 快路径:**绝不阻塞**——装得下全装;装不下装满并返回欠账
    /// (大粘贴:欠账由调用方持有,消息循环分批续灌,UI 保持响应)。
    pub fn enqueue_or_defer(&self, epoch: u64, bytes: &[u8]) -> Result<Vec<u8>, PushError> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.cancelled(epoch) {
            return Err(self.err_kind());
        }
        let room = self.cap.saturating_sub(st.bytes);
        if room == 0 {
            return Ok(bytes.to_vec());
        }
        let take = room.min(bytes.len());
        st.bytes += take;
        st.queue.push_back(bytes[..take].to_vec());
        self.space.notify_all();
        Ok(bytes[take..].to_vec())
    }

    /// writer 线程取一块(阻塞;None = 关闭,或换代后旧数据清空完且
    /// 再无新数据——writer 按新代重新入循环由调用方处理)。
    fn pop_blocking(&self, want_epoch: u64) -> Option<Vec<u8>> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(front) = st.queue.pop_front() {
                st.bytes -= front.len();
                self.space.notify_all();
                if self.cur_gen.load(Ordering::Acquire) != want_epoch {
                    continue; // 换代:旧数据整队丢弃
                }
                return Some(front);
            }
            if self.closed.load(Ordering::Acquire)
                || self.cur_gen.load(Ordering::Acquire) != want_epoch
            {
                return None;
            }
            st = self.space.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// 换代:清队、代 +1、唤醒(writer 与 enqueue 等待者按新代续行;
    /// 旧 writer 在 pop 返回 None 后退场,由调用方挂新 writer)。
    pub fn bump_generation(&self) -> u64 {
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.queue.clear();
            st.bytes = 0;
        }
        let next = self.cur_gen.fetch_add(1, Ordering::AcqRel) + 1;
        self.space.notify_all();
        next
    }

    /// 关闭:唤醒全部等待者(enqueue 立即 Err;writer 收 None 退场)。
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.space.notify_all();
    }

    /// 当前代。
    pub fn generation(&self) -> u64 {
        self.cur_gen.load(Ordering::Acquire)
    }

    /// 积压字节(诊断/测试)。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn queued_bytes(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).bytes
    }
}

/// writer 线程主体:独占消费写队列写 pty(经 `sink`;生产 = 会话,
/// 测试 = 注入假件)。写失败(管道断)→ 关队列(后续 enqueue 报
/// Closed,UI 不再卡等)。
pub fn writer_loop(
    queue: Arc<WriteQueue>,
    sink: Arc<dyn BytesSink + Send + Sync>,
) -> std::io::Result<()> {
    let epoch = queue.generation();
    while let Some(item) = queue.pop_blocking(epoch) {
        if let Err(e) = sink.write_all(&item) {
            queue.close();
            return Err(e);
        }
    }
    Ok(())
}

/// 写目标抽象(生产 = Arc<Mutex<PtySession>>;测试假件可控阻塞)。
pub trait BytesSink: Send + Sync {
    fn write_all(&self, bytes: &[u8]) -> std::io::Result<()>;
}

/// 生产实现:PtySession 的写(&self,内部互斥——见 core pty.rs)。
pub struct SessionSink(pub Arc<Mutex<mica_core::pty::PtySession>>);

impl BytesSink for SessionSink {
    fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(bytes)
    }
}

#[cfg(test)]
mod tests;
