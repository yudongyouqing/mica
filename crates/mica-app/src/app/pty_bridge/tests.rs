//! T08 返工验收测试(复核六条):全部纯数据/线程同步/注入器,不依赖
//! 前台焦点。契约:零丢弃、严格守恒预算、可取消等待、唤醒合并与
//! 失败恢复、单 writer 保序、满队列时关闭/重启及时完成。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 断言精确预留值的用例串行锁:BUDGET 是进程级共享,并行用例互相
/// 持有预留会污染彼此的 reserved_bytes 断言(测试器竞争,非实现缺陷)。
fn budget_guard() -> std::sync::MutexGuard<'static, ()> {
    static G: OnceLock<Mutex<()>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

use super::*;

// ---------------------------------------------------------------------------
// 注入假件
// ---------------------------------------------------------------------------

/// 可编程唤醒假件:计数投递,可注入前 N 次失败。
struct FakeWake {
    calls: AtomicUsize,
    fail_first: AtomicUsize,
}

impl FakeWake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            fail_first: AtomicUsize::new(0),
        })
    }

    fn total_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl WakeSink for FakeWake {
    fn wake(&self, _pane_id: u64) -> bool {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_first.load(Ordering::SeqCst) {
            return false; // 注入失败
        }
        true
    }
}

/// 可阻塞写假件:gate 关闭时 write 挂起(shell 不读输入形态)。
struct GatedSink {
    gate: Arc<AtomicBool>, // false = 阻塞
    written: Arc<Mutex<Vec<u8>>>,
}

impl BytesSink for GatedSink {
    fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        while !self.gate.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.written.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
}

/// 基线:测试前后全进程预留必须归零(守恒的全局面)。
fn drain_all(b: &PtyBacklog) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = b.drain_batch(usize::MAX) {
        out.extend_from_slice(&chunk);
    }
    out
}

// ---------------------------------------------------------------------------
// 1/2/3:预算守恒与严格限额
// ---------------------------------------------------------------------------

/// 复核 2 的探针反例修后形态:多次小块 push 精确守恒
/// (旧实现 push("a")+push("b") 计 3、drain 后仍余 1)。
#[test]
fn budget_exact_conservation_across_small_pushes() {
    let _g = budget_guard();
    let before = reserved_bytes();
    {
        let wake = FakeWake::new();
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, wake.clone());
        assert_eq!(b.push(0, b"a").unwrap(), ());
        assert_eq!(b.push(0, b"b").unwrap(), ());
        assert_eq!(
            reserved_bytes() - before,
            2,
            "两次 1 字节 push 预留恰为 2(旧实现计 3)"
        );
        let drained = drain_all(&b);
        assert_eq!(drained, b"ab");
        assert_eq!(
            reserved_bytes() - before,
            0,
            "drain 后预留归零(旧实现仍余 1)"
        );
    }
    assert_eq!(reserved_bytes(), before, "Drop 不留账");
}

/// 复核 3:非空 Drop 归还;clear/换代/关闭归还且不双扣。
#[test]
fn drop_clear_bump_close_all_release_exactly_once() {
    let _g = budget_guard();
    let before = reserved_bytes();
    // Drop(非空)
    {
        let wake = FakeWake::new();
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, wake);
        b.push(0, &vec![7u8; 1024]).unwrap();
        assert_eq!(reserved_bytes() - before, 1024);
        // 不 drain:直接 Drop
    }
    assert_eq!(reserved_bytes(), before, "非空 Drop 归还");

    // close 归还 + close 后 Drop 不双扣
    {
        let wake = FakeWake::new();
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, wake);
        b.push(0, &vec![7u8; 512]).unwrap();
        b.close();
        assert_eq!(reserved_bytes(), before, "close 归还");
        b.close(); // 重复 close:数据已空,释放 0
        assert_eq!(reserved_bytes(), before, "重复 close 不双扣");
    }
    assert_eq!(reserved_bytes(), before);

    // bump 归还旧代 + 新代可用
    {
        let wake = FakeWake::new();
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, wake);
        b.push(0, &vec![7u8; 256]).unwrap();
        let g1 = b.bump_generation();
        assert_eq!(g1, 1);
        assert_eq!(reserved_bytes() - before, 0, "换代归还存量");
        b.push(g1, b"new session").unwrap(); // 11 字节
        assert_eq!(reserved_bytes() - before, 11);
    }
    assert_eq!(reserved_bytes(), before);
}

/// 复核 2/3:多 pane 并发预留严格 ≤ 上限(load-then-add 不严格;
/// 持锁判限必须被并发打满验证)。
#[test]
fn multi_pane_concurrent_reservation_strictly_bounded() {
    let _g = budget_guard();
    let before = reserved_bytes();
    const PANES: usize = 8;
    const PER_PUSH: usize = 128 * 1024;
    const ROUNDS: usize = 200;
    // 消费线程持续放水(制造并发释放)
    let stop = Arc::new(AtomicBool::new(false));
    let panes: Vec<Arc<PtyBacklog>> = (0..PANES)
        .map(|i| PtyBacklog::new(i as u64, PANE_BUDGET_BYTES, FakeWake::new()))
        .collect();
    let consumers: Vec<_> = panes
        .iter()
        .map(|b| {
            let b = Arc::clone(b);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let _ = b.drain_batch(usize::MAX);
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        })
        .collect();
    let producers: Vec<_> = panes
        .iter()
        .map(|b| {
            let b = Arc::clone(b);
            std::thread::spawn(move || {
                let chunk = vec![1u8; PER_PUSH];
                for _ in 0..ROUNDS {
                    b.push(0, &chunk).unwrap();
                }
            })
        })
        .collect();
    // 观察者:全程峰值必须 ≤ 上限(与 before 基线的差)
    let producers = Arc::new(Mutex::new(producers));
    let observer = {
        let producers = Arc::clone(&producers);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            let mut peak = 0u64;
            loop {
                let done = {
                    let mut g = producers.lock().unwrap();
                    if g.is_empty() {
                        break;
                    }
                    g.retain(|h| !h.is_finished());
                    !g.is_empty()
                };
                if !done || Instant::now() > deadline {
                    break;
                }
                peak = peak.max(reserved_bytes());
                std::thread::sleep(Duration::from_micros(300));
            }
            peak
        })
    };
    // join 全部(逐个取走)
    {
        let mut g = producers.lock().unwrap();
        for h in g.drain(..) {
            h.join().unwrap();
        }
    }
    let peak = observer.join().unwrap();
    stop.store(true, Ordering::SeqCst);
    for h in consumers {
        h.join().unwrap();
    }
    for b in &panes {
        drain_all(b);
    }
    drop(panes);
    assert_eq!(reserved_bytes(), before, "结束后归零");
    assert!(
        peak.saturating_sub(before) <= PROCESS_BUDGET_BYTES,
        "并发峰值 {peak} 超严格上限(基线 {before})"
    );
}

// ---------------------------------------------------------------------------
// 4:零丢弃——超预算阻塞等待,恢复后逐字节一致
// ---------------------------------------------------------------------------

/// 复核 1/6:超 pane 预算的生产者**阻塞**(不丢弃);消费者放水后
/// 输出与输入**逐字节完全一致**(零丢字节证据)。
#[test]
fn over_budget_producer_blocks_then_output_is_byte_identical() {
    let _g = budget_guard();
    let before = reserved_bytes();
    let wake = FakeWake::new();
    let b = PtyBacklog::new(9, 64 * 1024, wake); // 小 pane 上限便于测试
    let input: Vec<u8> = (0u8..=255).cycle().take(512 * 1024).collect();
    let producer = {
        let b = Arc::clone(&b);
        let data = input.clone();
        std::thread::spawn(move || {
            for chunk in data.chunks(8 * 1024) {
                b.push(0, chunk).unwrap(); // 满 → 阻塞,绝不 Err(未取消)
            }
        })
    };
    // 慢消费:等生产者先堵住(缓冲到上限),再持续放水
    std::thread::sleep(Duration::from_millis(100));
    let mut got = Vec::new();
    while !producer.is_finished() {
        if let Some(chunk) = b.drain_batch(16 * 1024) {
            got.extend_from_slice(&chunk);
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    producer.join().unwrap();
    got.extend_from_slice(&drain_all(&b));
    assert_eq!(got.len(), input.len(), "零丢字节(长度)");
    assert_eq!(got, input, "零丢字节(逐字节一致,含顺序)");
    drop(b);
    assert_eq!(reserved_bytes(), before);
}

/// 复核 5:满队列时关闭及时完成——push 阻塞中被 close 唤醒并返回 Err。
#[test]
fn close_wakes_blocked_push_immediately() {
    let _g = budget_guard();
    let wake = FakeWake::new();
    let b = PtyBacklog::new(2, 1024, wake);
    b.push(0, &vec![1u8; 1024]).unwrap(); // 填满 pane
    let producer = {
        let b = Arc::clone(&b);
        std::thread::spawn(move || b.push(0, b"overflow"))
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(!producer.is_finished(), "push 应阻塞在 pane 满");
    b.close();
    let started = Instant::now();
    let outcome = producer.join().unwrap();
    assert_eq!(outcome, Err(PushError::Closed));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "关闭必须即时唤醒"
    );
}

/// 换代同理:阻塞中的旧代 push 被 bump 唤醒,返回 StaleGeneration。
#[test]
fn generation_bump_wakes_blocked_push_with_stale() {
    let _g = budget_guard();
    let wake = FakeWake::new();
    let b = PtyBacklog::new(3, 1024, wake);
    b.push(0, &vec![1u8; 1024]).unwrap();
    let producer = {
        let b = Arc::clone(&b);
        std::thread::spawn(move || b.push(0, b"old session tail"))
    };
    std::thread::sleep(Duration::from_millis(50));
    b.bump_generation();
    assert_eq!(
        producer.join().unwrap(),
        Err(PushError::StaleGeneration),
        "旧代 push 以换代取消"
    );
}

// ---------------------------------------------------------------------------
// 4:唤醒合并与失败恢复
// ---------------------------------------------------------------------------

/// 复核 4:多次 push 只投一条唤醒(pending 合并)。
#[test]
fn wakes_coalesce_to_one_pending() {
    let _g = budget_guard();
    let wake = FakeWake::new();
    let b = PtyBacklog::new(4, PANE_BUDGET_BYTES, wake.clone());
    for i in 0..100u32 {
        b.push(0, format!("chunk-{i}").as_bytes()).unwrap();
    }
    let n = wake.total_calls();
    assert_eq!(n, 1, "100 次 push 只装配 1 条唤醒(得 {n})");
    drain_all(&b);
    // 消费后再 push:pending 已清,重新装配
    b.push(0, b"next").unwrap();
    assert_eq!(wake.total_calls(), 2);
}

/// 复核 4:最后一条 Post 失败且**再无输出**——retry_wake 在空闲期补投
/// 成功(失败注入 1 次)。
#[test]
fn last_post_failure_recovers_via_retry() {
    let _g = budget_guard();
    let wake = FakeWake::new();
    wake.fail_first.store(1, Ordering::SeqCst); // 首投失败
    let b = PtyBacklog::new(5, PANE_BUDGET_BYTES, wake.clone());
    b.push(0, b"only block").unwrap();
    assert_eq!(wake.total_calls(), 1, "首投(失败)");
    // 再无 push:转发线程空闲期轮询 retry_wake
    b.retry_wake();
    assert!(b.has_more(), "数据未被消费");
    assert_eq!(wake.total_calls(), 2, "重试补投成功");
    // 消费后 pending 清零;后续正常
    drain_all(&b);
    b.push(0, b"after").unwrap();
    assert_eq!(wake.total_calls(), 3);
}

/// 消费侧"清标记→复查→补通知":drain 一批后仍有余量 → 自动补投
/// (push 在取走与清标记之间到达的合并在补投中兑现)。
#[test]
fn drain_recheck_arms_followup_wake() {
    let _g = budget_guard();
    let wake = FakeWake::new();
    let b = PtyBacklog::new(6, PANE_BUDGET_BYTES, wake.clone());
    b.push(0, &vec![1u8; 64 * 1024]).unwrap();
    assert_eq!(wake.total_calls(), 1);
    let batch = b.drain_batch(16 * 1024).unwrap();
    assert_eq!(batch.len(), 16 * 1024);
    assert!(b.has_more());
    assert_eq!(wake.total_calls(), 2, "批后复查补投续批唤醒");
    drain_all(&b);
}

// ---------------------------------------------------------------------------
// 5:单 writer 写队列
// ---------------------------------------------------------------------------

/// 保序:writer 串行消费,写入序 = 入队序。
#[test]
fn write_queue_preserves_order_single_writer() {
    let q = WriteQueue::new();
    let written = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(GatedSink {
        gate: Arc::new(AtomicBool::new(true)),
        written: Arc::clone(&written),
    });
    let writer = {
        let q = Arc::clone(&q);
        let sink = Arc::clone(&sink) as Arc<dyn BytesSink + Send + Sync>;
        std::thread::spawn(move || writer_loop(q, sink))
    };
    for i in 0..500u32 {
        q.enqueue(0, format!("item-{i:04}\n").as_bytes()).unwrap();
    }
    q.close();
    writer.join().unwrap().unwrap();
    let w = written.lock().unwrap().clone();
    let text = String::from_utf8(w).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 500);
    for (i, l) in lines.iter().enumerate() {
        assert_eq!(*l, format!("item-{i:04}"));
    }
}

/// 复核 6:shell 不读输入(writer 挂在 gate 上)时——
/// (a) 小写入 enqueue 不死等 UI?不:队列满前 enqueue 秒回;
/// (b) 大粘贴 enqueue_or_defer **立即**返回欠账(UI 不卡);
/// (c) 关闭及时完成(writer 阻塞在写上也无需先醒:close 让 enqueue
///     全 Err、writer 在下一次写返回后收到 None 退场)。
#[test]
fn gated_writer_keeps_ui_responsive_and_close_completes() {
    let q = WriteQueue::new();
    let written = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(AtomicBool::new(false)); // shell 不读
    let sink = Arc::new(GatedSink {
        gate: Arc::clone(&gate),
        written: Arc::clone(&written),
    });
    let writer = {
        let q = Arc::clone(&q);
        let sink = Arc::clone(&sink) as Arc<dyn BytesSink + Send + Sync>;
        std::thread::spawn(move || writer_loop(q, sink))
    };
    // 填满队列(writer 挂在第一块上不消费)
    let big: Vec<u8> = vec![b'x'; WRITE_QUEUE_BYTES + 256 * 1024];
    let started = Instant::now();
    let leftover = q.enqueue_or_defer(0, &big).unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "大粘贴入队必须立即返回(UI 不卡),耗时 {:?}",
        started.elapsed()
    );
    assert!(leftover.len() > 256 * 1024 - 1024, "超出容量的部分成欠账");
    assert_eq!(q.queued_bytes(), WRITE_QUEUE_BYTES);
    // 小写入:队列满 → enqueue_or_defer 全欠,依旧秒回
    let t0 = Instant::now();
    let l2 = q.enqueue_or_defer(0, b"typed\r").unwrap();
    assert_eq!(l2, b"typed\r");
    assert!(t0.elapsed() < Duration::from_millis(50));
    // 关闭:及时完成(不等 gate)
    let t1 = Instant::now();
    q.close();
    assert!(t1.elapsed() < Duration::from_millis(10), "close 零等待");
    assert_eq!(q.enqueue(0, b"more"), Err(PushError::Closed));
    // 放水:writer 冲完存量后收 None 退场
    gate.store(true, Ordering::SeqCst);
    let done = Instant::now() + Duration::from_secs(30);
    while !writer.is_finished() && Instant::now() < done {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(writer.is_finished(), "放水后 writer 必须退场");
    let n = written.lock().unwrap().len();
    assert_eq!(n, WRITE_QUEUE_BYTES, "存量全部写达,欠账归调用方");
}

/// 换代:旧 writer 退场、旧数据作废、新 writer 新代续写。
#[test]
fn generation_bump_discards_old_and_starts_fresh_writer() {
    let q = WriteQueue::new();
    let gate = Arc::new(AtomicBool::new(true));
    let written = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(GatedSink {
        gate,
        written: Arc::clone(&written),
    });
    let w1 = {
        let q = Arc::clone(&q);
        let sink = Arc::clone(&sink) as Arc<dyn BytesSink + Send + Sync>;
        std::thread::spawn(move || writer_loop(q, sink))
    };
    q.enqueue(0, b"old-A\n").unwrap();
    q.enqueue(0, b"old-B\n").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let g1 = q.bump_generation();
    assert_eq!(g1, 1);
    let t = Instant::now();
    while !w1.is_finished() && Instant::now() - t < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(w1.is_finished(), "旧 writer 按代退场");
    // 旧代入队立即 Err
    assert_eq!(q.enqueue(0, b"stale"), Err(PushError::StaleGeneration));
    // 新 writer + 新数据
    let w2 = {
        let q = Arc::clone(&q);
        let sink = Arc::clone(&sink) as Arc<dyn BytesSink + Send + Sync>;
        std::thread::spawn(move || writer_loop(q, sink))
    };
    q.enqueue(g1, b"new-session\n").unwrap();
    q.close();
    w2.join().unwrap().unwrap();
    let text = String::from_utf8(written.lock().unwrap().clone()).unwrap();
    assert!(text.contains("new-session"), "新代数据写达");
    // 旧数据:已消费部分写达属于旧 writer 正常行为;未消费部分作废
    // (bump 清队保证不串会话——这里验证不出现新代数据缺失/乱插)
    assert!(
        text.ends_with("new-session\n"),
        "新代数据不被旧数据污染队尾"
    );
}

/// enqueue 满队列阻塞被 close 唤醒(小写入路径的及时取消)。
#[test]
fn enqueue_blocks_then_close_cancels() {
    let q = WriteQueue::new();
    q.enqueue(0, &vec![1u8; WRITE_QUEUE_BYTES]).unwrap(); // 填满
    let producer = {
        let q = Arc::clone(&q);
        std::thread::spawn(move || q.enqueue(0, b"blocked write"))
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(!producer.is_finished(), "enqueue 应阻塞");
    q.close();
    assert_eq!(
        producer.join().unwrap(),
        Err(PushError::Closed),
        "关闭唤醒等待者"
    );
}
