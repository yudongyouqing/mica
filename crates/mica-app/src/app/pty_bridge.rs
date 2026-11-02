//! 有界 PTY 输出桥(T08/F10,2026-11-02):合并两级积压为**按字节计量**
//! 的预算体系。
//!
//! 修前形态:pty 读线程 → **无界** mpsc → 转发线程往 **无界** Vec 累加 →
//! UI 一次 WM_APP_RENDER 全量 feed——内存峰值、消息风暴、交互长任务
//! 三者叠加(读线程块块 Post,UI 每块全量消化)。
//!
//! 预算链(自源头到 UI):
//! 1. **源头背压(零丢字节)**:mica-core 读通道改 `sync_channel`(按块
//!    有界,见 pty.rs)。通道满 → 读线程阻塞在 send → ConPTY 管道写端
//!    顶住 → shell 自然暂停产出。恢复后自动续流——正常终端字节永不丢。
//! 2. **pane 预算**:`PtyBuf` 2 MiB。UI 卡死病态场景下截**尾**(丢弃
//!    最新字节,绝不动头部——头是协议序列中间态,丢头=状态机错乱),
//!    计入诊断计数。
//! 3. **全进程预算**:所有 pane 积压合计 32 MiB(原子计),超限的 pane
//!    push 直接按截尾处理——多 pane 均摊同一上限。
//! 4. **UI 批预算**:单次 WM_APP_RENDER 消化 ≤256 KiB,有余量即补投
//!    自唤醒消息(让出消息循环续批)——DSR 应答与输入不被长批饿死。
//!
//! 唤醒防丢:转发线程每块都 Post(幂等);UI 批后 `has_more` 复查补投。
//! Post 失败(队列满)由下一块的 Post 自然补上。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// 单 pane 待解析预算(字节)。终端输出 80×24 全屏 SGR 密集 ≈ 100KB,
/// 2 MiB ≈ 20 屏余量:正常交互永不触及,触及即 UI 卡死的病态场景。
pub const PANE_BUDGET_BYTES: usize = 2 * 1024 * 1024;
/// 全进程待解析预算(字节):16 pane × 满预算的兜底。
pub const PROCESS_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
/// UI 单次调度批上限(字节):超此让出消息循环续批。
pub const UI_BATCH_BYTES: usize = 256 * 1024;

/// 全进程已积压字节(所有 pane 合计;push 加、drain 减)。
static PROCESS_BACKLOG: AtomicU64 = AtomicU64::new(0);
/// 因预算截尾而丢弃的字节累计(诊断面,与 reaper_stats 同款出口)。
static DROPPED_TAIL_BYTES: AtomicU64 = AtomicU64::new(0);

/// pty 输出落点(原 app.rs 的 PtyBuf 迁入,T08 加预算):
/// `epoch` 是会话代号(重启换代丢弃旧转发线程尾字节);`data` 有界。
pub struct PtyBuf {
    epoch: AtomicU64,
    data: Mutex<Vec<u8>>,
}

impl PtyBuf {
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch: AtomicU64::new(epoch),
            data: Mutex::new(Vec::new()),
        }
    }

    /// 当前会话代(转发线程比对用)。
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.data.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 转发线程追加一块(先核会话代,见 forwarder)。预算:pane 2 MiB
    /// 截尾 + 全进程 32 MiB 超限截尾;截掉的计入诊断。返回保留长度。
    pub fn push(&self, chunk: &[u8]) -> usize {
        let mut buf = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let proc_over =
            PROCESS_BACKLOG.load(Ordering::Relaxed) + chunk.len() as u64 > PROCESS_BUDGET_BYTES;
        buf.extend_from_slice(chunk);
        if buf.len() > PANE_BUDGET_BYTES || proc_over {
            let cap = if proc_over {
                // 进程超限:截到当前存量的一半以下,给其他 pane 让路
                buf.len() / 2
            } else {
                PANE_BUDGET_BYTES
            };
            let keep = buf.len().saturating_sub(buf.len() - cap);
            let dropped = buf.len() - keep;
            buf.truncate(keep);
            DROPPED_TAIL_BYTES.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        PROCESS_BACKLOG.fetch_add(buf.len() as u64, Ordering::Relaxed);
        buf.len()
    }

    /// UI 取一批(≤ UI_BATCH_BYTES)。空批 None(调用方免重绘)。
    pub fn drain_budget(&self) -> Option<Vec<u8>> {
        let mut buf = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if buf.is_empty() {
            return None;
        }
        let take = buf.len().min(UI_BATCH_BYTES);
        let out: Vec<u8> = buf.drain(..take).collect();
        drop(buf);
        PROCESS_BACKLOG.fetch_sub(out.len() as u64, Ordering::Relaxed);
        Some(out)
    }

    /// 批后仍有剩余(UI 复查补投续批的依据)。
    pub fn has_more(&self) -> bool {
        !self
            .data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// 会话换代(T03 契约):旧转发线程尾字节按代丢弃;数据清空并归还
    /// 进程预算。
    pub fn bump_epoch(&self) -> u64 {
        {
            let mut buf = self.data.lock().unwrap_or_else(|e| e.into_inner());
            PROCESS_BACKLOG.fetch_sub(buf.len() as u64, Ordering::Relaxed);
            buf.clear();
        }
        // Release 与转发线程的 Acquire 配对:换代的可见性先于数据清空
        self.epoch.fetch_add(1, Ordering::Release) + 1
    }

    /// 会话终结:残余字节属旧会话,清空并归还进程预算。
    /// (T09 调度器接入隐藏窗口暂停态时复用)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn clear(&self) {
        let mut buf = self.data.lock().unwrap_or_else(|e| e.into_inner());
        PROCESS_BACKLOG.fetch_sub(buf.len() as u64, Ordering::Relaxed);
        buf.clear();
    }
}

/// 诊断面:(全进程积压字节, 累计截尾字节)。
pub fn backlog_stats() -> (u64, u64) {
    (
        PROCESS_BACKLOG.load(Ordering::Relaxed),
        DROPPED_TAIL_BYTES.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_truncates_tail_never_head() {
        let b = PtyBuf::new(0);
        let head = vec![0x1bu8, b'[', b'3', b'1', b'm'];
        b.push(&head);
        let big = vec![b'x'; PANE_BUDGET_BYTES];
        let len = b.push(&big);
        assert!(len <= PANE_BUDGET_BYTES, "截到预算(得 {len})");
        let drained = b.drain_budget().unwrap();
        // 头部必须原样(协议序列中间态)
        assert_eq!(&drained[..head.len()], &head[..]);
    }

    #[test]
    fn drain_batch_respects_ui_budget() {
        let b = PtyBuf::new(0);
        b.push(&vec![b'a'; UI_BATCH_BYTES * 3]);
        let first = b.drain_budget().unwrap();
        assert_eq!(first.len(), UI_BATCH_BYTES);
        assert!(b.has_more());
        let _ = b.drain_budget().unwrap();
        let _ = b.drain_budget().unwrap();
        assert!(!b.has_more());
        assert!(b.drain_budget().is_none(), "空批 None(免重绘)");
    }

    #[test]
    fn order_preserved_across_batches() {
        let b = PtyBuf::new(0);
        let data: Vec<u8> = (0u8..=255).cycle().take(UI_BATCH_BYTES + 1000).collect();
        b.push(&data);
        let mut got = Vec::new();
        while let Some(chunk) = b.drain_budget() {
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, data, "分批取空后序不变");
    }

    #[test]
    fn clear_returns_process_budget() {
        let b = PtyBuf::new(0);
        b.push(b"junk");
        b.clear();
        assert!(b.is_empty());
        assert!(b.drain_budget().is_none());
    }
}
