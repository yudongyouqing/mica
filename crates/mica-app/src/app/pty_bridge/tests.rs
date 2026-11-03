//! T08 二次返工验收(复核七条):生产路径组合 + 真实接线和交错顺序。
//! 全部纯数据/线程/注入器 + 真实 PtySession(锁模型),不依赖前台焦点。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::*;

fn budget_guard() -> std::sync::MutexGuard<'static, ()> {
    static G: OnceLock<Mutex<()>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 可编程唤醒假件:计数 + 前 N 次失败注入。
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

    fn total(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl WakeSink for FakeWake {
    fn wake(&self, _pane: u64) -> bool {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        n >= self.fail_first.load(Ordering::SeqCst)
    }
}

/// 可挂起写假件(gate=false 阻塞 = shell 不读输入)。
struct GatedSink {
    gate: Arc<AtomicBool>,
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

fn drain_all(b: &PtyBacklog) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(c) = b.drain_batch(usize::MAX) {
        out.extend_from_slice(&c);
    }
    out
}

fn spawn_writer(
    q: &Arc<WriteQueue>,
    sink: Arc<dyn BytesSink + Send + Sync>,
) -> std::thread::JoinHandle<()> {
    let q = Arc::clone(q);
    std::thread::spawn(move || writer_loop(q, sink))
}

// ---------------------------------------------------------------------------
// 复核 1:enqueue 唤醒 writer(修前探针:writer 先睡,enqueue 后 200ms
// 仍 written=0、queued=1)
// ---------------------------------------------------------------------------

#[test]
fn idle_writer_wakes_on_single_input() {
    let q = WriteQueue::new();
    let written = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(GatedSink {
        gate: Arc::new(AtomicBool::new(true)),
        written: Arc::clone(&written),
    });
    let w = spawn_writer(&q, sink);
    std::thread::sleep(Duration::from_millis(80)); // writer 先进入空闲等待
    assert_eq!(q.queued_bytes(), 0);
    assert_eq!(q.try_push(b"x"), 1, "单次普通输入");
    let deadline = Instant::now() + Duration::from_millis(200);
    while *written.lock().unwrap() != b"x"[..] && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(*written.lock().unwrap(), b"x", "200ms 内必须写出(修前挂死)");
    q.close();
    w.join().unwrap();
}

// ---------------------------------------------------------------------------
// 复核 2:UI 全非阻塞 + 有界接收/拒绝/欠账(InputStage 语义)
// ---------------------------------------------------------------------------

#[test]
fn ui_input_paths_never_block_on_full_queue() {
    let q = WriteQueue::new();
    let gate = Arc::new(AtomicBool::new(false)); // shell 不读
    let written = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(GatedSink {
        gate: Arc::clone(&gate),
        written: Arc::clone(&written),
    });
    let w = spawn_writer(&q, sink);
    assert_eq!(
        q.try_push(&vec![b'p'; WRITE_QUEUE_BYTES]),
        WRITE_QUEUE_BYTES
    );
    // 生产 stage_input 序列(满队列下的按键/DSR/粘贴)
    let mut stage = InputStage::new();
    let t0 = Instant::now();
    let key = b"key\r";
    let acc = q.try_push(key);
    assert!(acc <= key.len(), "writer 弹出队首后可能空出少量位");
    assert_eq!(stage.stage(&key[acc..]), 0);

    // Z 串贴满暂存:若按键部分进了暂存,预算相应少收(有界)
    assert_eq!(stage.stage(&vec![b'Z'; INPUT_STAGE_BYTES]), key.len() - acc);
    let stage_len_at_full = stage.len(); // = INPUT_STAGE_BYTES(恰好贴满)
    let rejected = stage.stage(b"overflow-tail"); // 超暂存预算:显式拒绝
    assert_eq!(rejected, b"overflow-tail".len(), "有界拒绝语义");
    assert!(
        t0.elapsed() < Duration::from_millis(20),
        "全部非阻塞,耗时 {:?}",
        t0.elapsed()
    );
    let t1 = Instant::now();
    q.close(); // 关闭即时完成(不等 gate)
    assert!(t1.elapsed() < Duration::from_millis(10));
    gate.store(true, Ordering::SeqCst);
    w.join().unwrap();
    // 放水后 writer 把"已入队的全部"写达:填充 1MiB + 本次被接收的按键
    // (在途块在关闭后仍由 writer 正常写完——关闭只拒新,不弃旧)
    let total_written = written.lock().unwrap().len();
    assert_eq!(total_written, WRITE_QUEUE_BYTES + acc, "关闭不丢已入队字节");
    assert_eq!(q.queued_bytes(), 0, "writer 取空退场");
    // 欠账续灌(新队列 = 下一会话形态):FIFO 保序。期望内容在 flush
    // 前构造(按键部分若进暂存则在前,Z 填到暂存预算为止)——两种
    // 竞态形态(acc=0 或 4)都被同一断言覆盖
    let key_tail = key.len() - acc;
    let mut expected = key[acc..].to_vec();
    expected.extend_from_slice(&vec![b'Z'; INPUT_STAGE_BYTES - key_tail]);
    debug_assert_eq!(expected.len(), stage_len_at_full);
    let q2 = WriteQueue::new();
    let written2 = Arc::new(Mutex::new(Vec::new()));
    let sink2 = Arc::new(GatedSink {
        gate: Arc::new(AtomicBool::new(true)),
        written: Arc::clone(&written2),
    });
    let h2 = spawn_writer(&q2, sink2);
    let left = stage.flush_into(&q2);
    assert_eq!(stage.len(), left);
    q2.close();
    h2.join().unwrap();
    let out = written2.lock().unwrap().clone();
    assert_eq!(out, expected, "暂存内容按原序完整写达");
}

// ---------------------------------------------------------------------------
// 复核 3:真实会话锁模型——写端阻塞不拖 kill_child
// ---------------------------------------------------------------------------

#[test]
fn real_session_sink_kill_child_not_blocked_by_writer() {
    use mica_core::pty::{PtySession, command_from_str};
    let (mut session, reader) =
        PtySession::spawn(command_from_str("cmd.exe"), 80, 24).expect("pty");
    drop(reader);
    {
        // 锁住写端(等价 writer 阻塞在 write)——同锁下控制面调用不受阻:
        // resize(&self) 即时返回;写端互斥与会话控制字段完全分离。
        let _guard = session.block_writer_for_test();
        let t = Instant::now();
        session.resize(100, 30).expect("resize 不被写端锁拖住");
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "控制面不得被阻塞写拖住,耗时 {:?}",
            t.elapsed()
        );
    }
    // guard 已释放:kill_child(&mut)走正常路径
    session.kill_child();
    let h = session.writer_handle();
    assert!(h.write(b"").is_ok());
    session.kill_child();
}

// ---------------------------------------------------------------------------
// 复核 4:会话隔离——旧 writer 与新代数据并存不误删
// ---------------------------------------------------------------------------

#[test]
fn old_writer_never_consumes_new_session_data() {
    let q_old = WriteQueue::new();
    let gate = Arc::new(AtomicBool::new(false));
    let w_old_written = Arc::new(Mutex::new(Vec::new()));
    let sink_old = Arc::new(GatedSink {
        gate: Arc::clone(&gate),
        written: Arc::clone(&w_old_written),
    });
    let w_old = spawn_writer(&q_old, sink_old);
    q_old.try_push(b"OLD-1");
    q_old.try_push(b"OLD-2");
    // "重启":新队列 + 新 writer(真实交错:旧 writer 仍活、旧数据滞留)
    let q_new = WriteQueue::new();
    let w_new_written = Arc::new(Mutex::new(Vec::new()));
    let sink_new = Arc::new(GatedSink {
        gate: Arc::new(AtomicBool::new(true)),
        written: Arc::clone(&w_new_written),
    });
    let w_new = spawn_writer(&q_new, sink_new);
    q_old.close();
    q_new.try_push(b"NEW"); // 修前:旧 writer 弹后验代会把新数据丢掉
    let deadline = Instant::now() + Duration::from_secs(10);
    while *w_new_written.lock().unwrap() != b"NEW"[..] && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*w_new_written.lock().unwrap(), b"NEW");
    gate.store(true, Ordering::SeqCst);
    let done = Instant::now() + Duration::from_secs(10);
    while !w_old.is_finished() && Instant::now() < done {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(w_old.is_finished(), "旧 writer 取空退场");
    assert_eq!(*w_old_written.lock().unwrap(), b"OLD-1OLD-2");
    q_new.close();
    w_new.join().unwrap();
}

// ---------------------------------------------------------------------------
// 复核 6:唤醒重试独立于生产者
// ---------------------------------------------------------------------------

/// 首投失败 + 持续输出直到 pane 满(转发线程堵在 push)——monitor 补投,
/// 数据逐字节一致。
#[test]
fn wake_retry_survives_backpressured_forwarder() {
    let _g = budget_guard();
    let wake = FakeWake::new();
    wake.fail_first.store(1, Ordering::SeqCst);
    let b = PtyBacklog::new(7, 64 * 1024, wake.clone());
    b.spawn_monitor();
    let input: Vec<u8> = (0u8..=255).cycle().take(128 * 1024).collect();
    let producer = {
        let b = Arc::clone(&b);
        let d = input.clone();
        std::thread::spawn(move || {
            for c in d.chunks(8 * 1024) {
                if b.push(0, c).is_err() {
                    return;
                }
            }
        })
    };
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !producer.is_finished() && Instant::now() < deadline {
        if let Some(c) = b.drain_batch(16 * 1024) {
            got.extend_from_slice(&c);
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    producer.join().unwrap();
    got.extend_from_slice(&drain_all(&b));
    assert!(wake.total() >= 2, "首投失败后有补投");
    assert_eq!(got, input, "逐字节一致(零丢字节)");
    b.close();
}

/// 最后一块输出后立即 EOF:转发线程退场,首投失败的数据由 monitor 补投。
#[test]
fn last_output_then_eof_still_delivered() {
    let _g = budget_guard();
    let wake = FakeWake::new();
    wake.fail_first.store(1, Ordering::SeqCst);
    let b = PtyBacklog::new(8, PANE_BUDGET_BYTES, wake.clone());
    b.spawn_monitor();
    b.push(0, b"final bytes").unwrap(); // "最后一块后 EOF":无更多 push
    let deadline = Instant::now() + Duration::from_secs(5);
    while wake.total() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(wake.total() >= 2, "EOF 后 monitor 补投");
    assert_eq!(drain_all(&b), b"final bytes", "数据送达(修前滞留)");
    b.close();
}

/// 唤醒合并 + 失败后不重复投递成功消息。
#[test]
fn wakes_coalesce_and_no_duplicate_after_success() {
    let _g = budget_guard();
    let wake = FakeWake::new();
    let b = PtyBacklog::new(6, PANE_BUDGET_BYTES, wake.clone());
    b.spawn_monitor();
    for i in 0..100u32 {
        b.push(0, format!("c{i}").as_bytes()).unwrap();
    }
    assert_eq!(wake.total(), 1, "100 push 1 wake(全成功则不重发)");
    std::thread::sleep(Duration::from_millis(120)); // monitor 空转数轮
    assert_eq!(wake.total(), 1, "成功投递绝不重发");
    drain_all(&b);
    b.push(0, b"n").unwrap();
    assert_eq!(wake.total(), 2, "消费后重新装配");
    b.close();
}

// ---------------------------------------------------------------------------
// 复核 7 组合:静默 shell 大粘贴续灌 + 随后输入保序(真实交错)
// ---------------------------------------------------------------------------

#[test]
fn silent_shell_paste_then_keys_preserve_order() {
    let q = WriteQueue::new();
    let gate = Arc::new(AtomicBool::new(false));
    let written = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(GatedSink {
        gate: Arc::clone(&gate),
        written: Arc::clone(&written),
    });
    let w = spawn_writer(&q, sink);
    // 大粘贴(超队列容量 → 欠账)
    let paste: Vec<u8> = vec![b'P'; WRITE_QUEUE_BYTES + 300 * 1024];
    let mut stage = InputStage::new();
    let acc = q.try_push(&paste);
    assert_eq!(stage.stage(&paste[acc..]), 0, "暂存预算内全收");
    // 欠账未清时按键——按生产 stage_input 的顺序纪律:**先冲欠账再收
    // 新输入**(直接 try_push 会越过暂存里的粘贴尾,测试必须复刻生产序)
    let key = b"after-paste\r";
    let flushed = stage.flush_into(&q);
    let _ = flushed;
    let acc = q.try_push(key);
    assert!(acc <= key.len());
    assert_eq!(stage.stage(&key[acc..]), 0);
    // shell 恢复读取:writer 冲队列;心跳续灌(多次 flush = WM_TIMER 节奏)
    gate.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !stage.is_empty() && Instant::now() < deadline {
        stage.flush_into(&q);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(stage.len(), 0, "欠账全部灌入(静默期由心跳推进)");
    q.close();
    w.join().unwrap();
    let out = written.lock().unwrap().clone();
    assert_eq!(out.len(), paste.len() + b"after-paste\r".len(), "零丢失");
    assert!(out.starts_with(&paste), "粘贴先");
    assert!(out.ends_with(b"after-paste\r"), "按键严格在粘贴之后");
}

// ---------------------------------------------------------------------------
// 预算守恒/严格限额/取消无丢唤醒(承前次返工契约)
// ---------------------------------------------------------------------------

#[test]
fn budget_exact_conservation_across_small_pushes() {
    let _g = budget_guard();
    let before = reserved_bytes();
    {
        let wake = FakeWake::new();
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, wake);
        b.push(0, b"a").unwrap();
        b.push(0, b"b").unwrap();
        assert_eq!(reserved_bytes() - before, 2);
        assert_eq!(drain_all(&b), b"ab");
        assert_eq!(reserved_bytes() - before, 0);
    }
    assert_eq!(reserved_bytes(), before, "Drop 不留账");
}

#[test]
fn drop_clear_bump_close_all_release_exactly_once() {
    let _g = budget_guard();
    let before = reserved_bytes();
    {
        let wake = FakeWake::new();
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, wake);
        b.push(0, &vec![7u8; 1024]).unwrap();
    }
    assert_eq!(reserved_bytes(), before, "非空 Drop 归还");
    {
        let wake = FakeWake::new();
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, wake);
        b.push(0, &vec![7u8; 512]).unwrap();
        b.close();
        b.close();
        assert_eq!(reserved_bytes(), before, "close 归还且不双扣");
    }
    {
        let wake = FakeWake::new();
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, wake);
        b.push(0, &vec![7u8; 256]).unwrap();
        let g1 = b.bump_generation();
        assert_eq!(g1, 1);
        assert!(!b.has_more(), "换代清空旧数据");
        b.push(g1, b"new session").unwrap();
        assert_eq!(reserved_bytes() - before, 11);
        assert_eq!(b.push(0, b"stale"), Err(PushError::StaleGeneration));
    }
    assert_eq!(reserved_bytes(), before);
}

#[test]
fn multi_pane_concurrent_reservation_strictly_bounded() {
    let _g = budget_guard();
    let before = reserved_bytes();
    const PANES: usize = 8;
    let stop = Arc::new(AtomicBool::new(false));
    let panes: Vec<Arc<PtyBacklog>> = (0..PANES)
        .map(|i| PtyBacklog::new(i as u64, PANE_BUDGET_BYTES, FakeWake::new()))
        .collect();
    let consumers: Vec<_> = panes
        .iter()
        .cloned()
        .map(|b| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let _ = b.drain_batch(usize::MAX);
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        })
        .collect();
    let producers = Arc::new(Mutex::new(Vec::<std::thread::JoinHandle<()>>::new()));
    {
        let mut g = producers.lock().unwrap();
        for b in &panes {
            let b = Arc::clone(b);
            g.push(std::thread::spawn(move || {
                let chunk = vec![1u8; 128 * 1024];
                for _ in 0..200 {
                    b.push(0, &chunk).unwrap();
                }
            }));
        }
    }
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
    assert_eq!(reserved_bytes(), before);
    assert!(peak.saturating_sub(before) <= PROCESS_BUDGET_BYTES);
}

#[test]
fn over_budget_producer_blocks_then_output_is_byte_identical() {
    let _g = budget_guard();
    let before = reserved_bytes();
    let wake = FakeWake::new();
    let b = PtyBacklog::new(9, 64 * 1024, wake);
    let input: Vec<u8> = (0u8..=255).cycle().take(512 * 1024).collect();
    let producer = {
        let b = Arc::clone(&b);
        let d = input.clone();
        std::thread::spawn(move || {
            for c in d.chunks(8 * 1024) {
                b.push(0, c).unwrap();
            }
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    let mut got = Vec::new();
    while !producer.is_finished() {
        if let Some(c) = b.drain_batch(16 * 1024) {
            got.extend_from_slice(&c);
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    producer.join().unwrap();
    got.extend_from_slice(&drain_all(&b));
    assert_eq!(got, input, "零丢字节(逐字节一致)");
    drop(b);
    assert_eq!(reserved_bytes(), before);
}

/// 复核 8:取消通知无丢唤醒——阻塞在**进程预算**预留中的 push 被
/// close/换代即时唤醒(pane 满路径同验)。
#[test]
fn process_budget_wait_cancelled_by_close_without_lost_wakeup() {
    let _g = budget_guard();
    // 占满进程预算(不消费):pane 上限 2MiB,16 个恰好 32MiB(单 pane
    // 塞不下 30MiB——上一版 holder2 卡在 pane 空间等待而非进程预算)
    let holders: Vec<Arc<PtyBacklog>> = (0..(PROCESS_BUDGET_BYTES / PANE_BUDGET_BYTES as u64))
        .map(|i| PtyBacklog::new(20 + i, PANE_BUDGET_BYTES, FakeWake::new()))
        .collect();
    for h in &holders {
        h.push(0, &vec![1u8; PANE_BUDGET_BYTES]).unwrap();
    }
    let victim = PtyBacklog::new(22, PANE_BUDGET_BYTES, FakeWake::new());
    let v = Arc::clone(&victim);
    let producer = std::thread::spawn(move || v.push(0, b"blocked"));
    std::thread::sleep(Duration::from_millis(80));
    assert!(!producer.is_finished(), "应阻塞在进程预算");
    let t = Instant::now();
    victim.close();
    assert_eq!(producer.join().unwrap(), Err(PushError::Closed));
    assert!(t.elapsed() < Duration::from_secs(1), "即时唤醒(无丢唤醒)");
    // 先释放 holder 预算(full 段的 push 也要走进程预算)
    for h in &holders {
        drain_all(h);
    }
    let full = PtyBacklog::new(23, 1024, FakeWake::new());
    full.push(0, &vec![1u8; 1024]).unwrap();
    let f = Arc::clone(&full);
    let p2 = std::thread::spawn(move || f.push(0, b"overflow"));
    std::thread::sleep(Duration::from_millis(50));
    let t2 = Instant::now();
    full.bump_generation();
    assert_eq!(p2.join().unwrap(), Err(PushError::StaleGeneration));
    assert!(t2.elapsed() < Duration::from_secs(1));
    for h in &holders {
        drain_all(h);
    }
}
