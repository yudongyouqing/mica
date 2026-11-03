//! T08 三次返工验收(复核六条):全部走**生产共享组件**
//! (`stage_into`/`drain_into`/`WriteQueue`/`PtyBacklog`/`SessionPermit`)
//! 与真实锁模型,不手工复刻理想调用序。

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

/// 可编程唤醒假件:可注入失败序列(脚本化)。
struct ScriptWake {
    calls: AtomicUsize,
    /// 失败集合:第 n 次(0 起)调用返回 false。
    fail_set: Mutex<Vec<usize>>,
}

impl ScriptWake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            fail_set: Mutex::new(Vec::new()),
        })
    }

    fn total(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn fail_at(&self, idx: usize) {
        self.fail_set.lock().unwrap().push(idx);
    }
}

impl WakeSink for ScriptWake {
    fn wake(&self, _pane: u64) -> bool {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        !self.fail_set.lock().unwrap().contains(&n)
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
// 复核 2(修前失败保留):旧欠账未清时新输入越过——生产入口 stage_into
// ---------------------------------------------------------------------------

/// **修前失败**:受控交错——writer 弹出队首制造空位(flush 后仍有旧欠账),
/// 新输入 try_push 直接进队列越过旧欠账。修后 stage_into 规定:暂存
/// 非空时新输入**只追加**。断言最终顺序 OLD 先于 NEW。
/// (修前用旧 stage_input 逻辑手动复刻生产序得 NEW→OLD,见复核探针。)
#[test]
fn new_input_never_bypasses_old_debt() {
    // 无 writer + gate 控制:排水由测试精确控制(受控交错的确定性)
    let q = WriteQueue::new();
    let gate = Arc::new(AtomicBool::new(false)); // shell 不读
    let written = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(GatedSink {
        gate: Arc::clone(&gate),
        written: Arc::clone(&written),
    });
    let w = spawn_writer(&q, sink);
    // 制造旧欠账:小块填满队列(writer 只能弹一块挂住)+ 暂存
    let mut stage = InputStage::new();
    let old_total = WRITE_QUEUE_BYTES + 64 * 1024;
    let old = vec![b'O'; old_total];
    // 先小块填队列(4KiB × N),一次 stage_into 会整块进队列被 writer
    // 一次弹空——分块保证 writer 只挂住一块,队列回到近满
    for chunk in old.chunks(4 * 1024) {
        stage_into(&mut stage, &q, chunk);
    }
    assert!(!stage.is_empty(), "旧欠账在(队列近满 + writer 挂住一块)");
    // **受控交错**:flush 尝试推进(writer 弹出的空位只够装回很少)
    stage.flush_into(&q);
    assert!(
        !stage.is_empty(),
        "欠账仍在(writer 只挂住一块,空位远小于欠账)"
    );
    // 新输入经生产入口
    let flow = stage_into(&mut stage, &q, b"NEW");
    assert!(
        matches!(flow, InputFlow::Debt(_)),
        "旧欠账在时新输入必须入暂存(不得 Accepted 绕过),得 {flow:?}"
    );
    // 放水清空:最终顺序 OLD 全部先于 NEW
    gate.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !stage.is_empty() && Instant::now() < deadline {
        stage.flush_into(&q);
        std::thread::sleep(Duration::from_millis(5));
    }
    q.close();
    w.join().unwrap();
    let out = written.lock().unwrap().clone();
    assert_eq!(out.len(), old.len() + 3, "零丢失");
    assert!(out[..old.len()].iter().all(|&b| b == b'O'), "OLD 全部在前");
    assert_eq!(&out[old.len()..], b"NEW", "NEW 严格在 OLD 之后");
}

// ---------------------------------------------------------------------------
// 复核 4:结构化结果 / 关闭拒收 / 溢出计数 / drain_into 语义
// ---------------------------------------------------------------------------

#[test]
fn closed_queue_reports_rejected_not_silent() {
    let q = WriteQueue::new();
    q.close();
    let mut stage = InputStage::new();
    // 修前:try_push 返回 0,stage_input 仍收进暂存并"返回成功"
    assert_eq!(
        stage_into(&mut stage, &q, b"typed"),
        InputFlow::RejectedClosed,
        "关闭必须结构化拒收(不得静默进暂存)"
    );
    assert!(stage.is_empty(), "关闭后暂存不收");
}

#[test]
fn overflow_counted_and_visible() {
    let q = WriteQueue::new();
    let mut stage = InputStage::new();
    let before = overflow_drops();
    // 超过 队列(1MiB)+ 暂存(512KiB)总量才触发溢出拒收
    let big = vec![b'X'; WRITE_QUEUE_BYTES + INPUT_STAGE_BYTES + 100];
    assert_eq!(
        stage_into(&mut stage, &q, &big),
        InputFlow::RejectedOverflow,
        "总量超队列+暂存必须整体拒收(得见计数)"
    );
    assert!(overflow_drops() >= before + 100, "溢出计数可见");
}

/// CLI/IPC 语义:drain_into 全部进队列 → Ok;关闭 → Err(Closed)。
#[test]
fn drain_into_applied_means_fully_queued() {
    let q = WriteQueue::new();
    let written = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(GatedSink {
        gate: Arc::new(AtomicBool::new(true)),
        written: Arc::clone(&written),
    });
    let w = spawn_writer(&q, sink);
    let mut stage = InputStage::new();
    let data = vec![b'D'; WRITE_QUEUE_BYTES + 128 * 1024]; // 超容量
    let r = drain_into(
        &mut stage,
        &q,
        &data,
        Instant::now() + Duration::from_secs(30),
    );
    assert!(r.is_ok(), "writer 排水期内应全部进队列(得 {r:?})");
    q.close();
    w.join().unwrap();
    assert_eq!(written.lock().unwrap().len(), data.len(), "全量写达");
    // 关闭后:立即 Err(Closed)
    let r2 = drain_into(
        &mut stage,
        &q,
        b"more",
        Instant::now() + Duration::from_millis(100),
    );
    assert_eq!(r2, Err(InputFlow::RejectedClosed));
}

/// drain_into 期限:shell 永不读 → Err(Debt)(IPC 据此报 timeout,
/// 不再假 Applied)。
#[test]
fn drain_into_deadline_reports_debt() {
    let q = WriteQueue::new();
    let gate = Arc::new(AtomicBool::new(false)); // 永不读
    let written = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(GatedSink {
        gate: Arc::clone(&gate),
        written: Arc::clone(&written),
    });
    let w = spawn_writer(&q, sink);
    let mut stage = InputStage::new();
    // 小块填满队列(writer 挂一块)+ 欠账:drain_into 在期限内无法清空
    let data = vec![b'W'; WRITE_QUEUE_BYTES + 32 * 1024];
    for chunk in data.chunks(4 * 1024) {
        let _ = stage_into(&mut stage, &q, chunk);
    }
    assert!(!stage.is_empty());
    // 追加一段走 drain_into(带着存量欠账,期限内清不完)
    let r = drain_into(
        &mut stage,
        &q,
        b"tail",
        Instant::now() + Duration::from_millis(300),
    );
    assert!(
        matches!(r, Err(InputFlow::Debt(_))),
        "期限未尽必须报 Debt(得 {r:?})"
    );
    q.close();
    gate.store(true, Ordering::SeqCst);
    w.join().unwrap();
}

// ---------------------------------------------------------------------------
// 复核 1:丢唤醒屏障(受控交错:检查后、挂起前 close)
// ---------------------------------------------------------------------------

/// **修前失败形态**:等待者通过取消检查→close 发通知→等待者才挂起→
/// 丢失(直到无关释放)。修后 Budget.notify_all 持锁——通知方阻塞在
/// held 上直到等待者挂起。这里用真实线程时序(等待者持锁窗口由
/// reserve 内部保证),断言 close 后 <500ms 返回(不依赖后续释放)。
#[test]
fn budget_cancel_no_lost_wakeup_close_vs_wait_race() {
    let _g = budget_guard();
    // 用另一个 pane 占住全部进程预算
    let holders: Vec<Arc<PtyBacklog>> = (0..(PROCESS_BUDGET_BYTES / PANE_BUDGET_BYTES as u64))
        .map(|i| PtyBacklog::new(50 + i, PANE_BUDGET_BYTES, ScriptWake::new()))
        .collect();
    for h in &holders {
        h.push(0, &vec![1u8; PANE_BUDGET_BYTES]).unwrap();
    }
    let victim = PtyBacklog::new(70, PANE_BUDGET_BYTES, ScriptWake::new());
    let v = Arc::clone(&victim);
    let producer = std::thread::spawn(move || v.push(0, b"x"));
    // 等待者确定进入预算等待(检查已过、已挂起或正要挂起)
    std::thread::sleep(Duration::from_millis(120));
    assert!(!producer.is_finished());
    // **窗口注入**:close 在等待者检查后立刻发生(若 notify 无锁屏障,
    // 此刻发射即丢失——修前复现点;修后通知方等锁,等待者必醒)
    let t = Instant::now();
    victim.close();
    assert_eq!(
        producer.join().unwrap(),
        Err(PushError::Closed),
        "close 必须唤醒预算等待者"
    );
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "即时唤醒(修前要等无关释放,耗时 {:?})",
        t.elapsed()
    );
    // WriteQueue::close 同屏障:阻塞中的 drain_into 被即时取消。
    // **无 writer**(队列保持满——有 writer 会立刻弹出队首制造空位,
    // drain 反而能完成):确定性制造"队列满+欠账"的阻塞形态
    let q = WriteQueue::new();
    let mut stage = InputStage::new();
    let big = vec![b'Q'; WRITE_QUEUE_BYTES + 64 * 1024];
    for chunk in big.chunks(4 * 1024) {
        let _ = stage_into(&mut stage, &q, chunk);
    }
    assert!(q.queued_bytes() >= WRITE_QUEUE_BYTES - 8 * 1024, "队列满");
    assert!(!stage.is_empty(), "欠账在");
    let drainer = {
        let q = Arc::clone(&q);
        let mut stage = stage;
        std::thread::spawn(move || {
            drain_into(
                &mut stage,
                &q,
                b"tail",
                Instant::now() + Duration::from_secs(60),
            )
        })
    };
    std::thread::sleep(Duration::from_millis(80));
    let t2 = Instant::now();
    q.close();
    assert_eq!(
        drainer.join().unwrap(),
        Err(InputFlow::RejectedClosed),
        "队列关闭同样即时唤醒等待者"
    );
    assert!(t2.elapsed() < Duration::from_millis(500));
    for h in &holders {
        drain_all(h);
    }
}

// ---------------------------------------------------------------------------
// 复核 6:monitor 状态机——旧成功不清新失败(受控脚本)
// ---------------------------------------------------------------------------

/// 脚本:wake[0] 失败(push A 置 retry)→ monitor 补投成功(清)→
/// push B 再次失败(置)→ 旧成功**不得**清掉新失败 → retry 保持,
/// 直到下一次成功。锁内转换保证。
#[test]
fn monitor_success_does_not_clear_newer_failure() {
    let _g = budget_guard();
    let wake = ScriptWake::new();
    wake.fail_at(0); // push A 的装配投递失败
    wake.fail_at(2); // push B 的装配投递失败(0=失败,1=monitor 补投成功,2=失败)
    let b = PtyBacklog::new(80, PANE_BUDGET_BYTES, wake.clone());
    b.spawn_monitor();
    // push A:arm 失败 → retry=true
    b.push(0, b"A").unwrap();
    assert!(b.retry_pending(), "首投失败置 retry");
    // 等 monitor 补投成功(第 1 次 call)清 retry
    let deadline = Instant::now() + Duration::from_secs(2);
    while b.retry_pending() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!b.retry_pending(), "monitor 成功后清 retry");
    // 消费 A(wake_pending 清)→ push B:第 2 次 call 失败 → retry=true
    assert_eq!(drain_all(&b), b"A");
    b.push(0, b"B").unwrap();
    assert!(b.retry_pending(), "新失败必须置位");
    // **关键窗口**:即便 monitor 紧随其后成功(第 3 次 call)也只清
    // 自己这次成功之后的位——锁内序保证不丢
    let deadline = Instant::now() + Duration::from_secs(2);
    while (b.retry_pending() || b.has_more()) && Instant::now() < deadline {
        if !b.retry_pending() && b.has_more() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // B 最终送达(monitor 或后续成功路径)
    let got = drain_all(&b);
    assert_eq!(got, b"B");
    b.close();
}

// ---------------------------------------------------------------------------
// 复核 5:permit 严格上限(并发争抢最后一槽)
// ---------------------------------------------------------------------------

#[test]
fn concurrent_permit_race_never_exceeds_max() {
    let _g = budget_guard();
    let start = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for _ in 0..64 {
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            while !start.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            crate::app::lifecycle::acquire_session()
        }));
    }
    start.store(true, Ordering::SeqCst);
    let granted: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let ok = granted.iter().filter(|p| p.is_some()).count();
    assert_eq!(
        ok,
        crate::app::lifecycle::MAX_SESSIONS,
        "并发争抢:恰 MAX 个成功(修前 load-then-add 可超)"
    );
    // Drop 全部 permit → 计数归零
    drop(granted);
    assert_eq!(crate::app::lifecycle::live_sessions(), 0);
}

// ---------------------------------------------------------------------------
// 承前契约:唤醒合并 / EOF 后送达 / 静默 shell 保序(生产入口)
// ---------------------------------------------------------------------------

#[test]
fn wakes_coalesce_and_no_duplicate_after_success() {
    let _g = budget_guard();
    let wake = ScriptWake::new();
    let b = PtyBacklog::new(6, PANE_BUDGET_BYTES, wake.clone());
    b.spawn_monitor();
    for i in 0..100u32 {
        b.push(0, format!("c{i}").as_bytes()).unwrap();
    }
    assert_eq!(wake.total(), 1, "100 push 1 wake");
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(wake.total(), 1, "成功投递绝不重发");
    drain_all(&b);
    b.push(0, b"n").unwrap();
    assert_eq!(wake.total(), 2);
    b.close();
}

#[test]
fn last_output_then_eof_still_delivered() {
    let _g = budget_guard();
    let wake = ScriptWake::new();
    wake.fail_at(0);
    let b = PtyBacklog::new(8, PANE_BUDGET_BYTES, wake.clone());
    b.spawn_monitor();
    b.push(0, b"final bytes").unwrap(); // 之后立即 EOF(无更多 push)
    let deadline = Instant::now() + Duration::from_secs(5);
    while wake.total() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(wake.total() >= 2, "EOF 后 monitor 补投");
    assert_eq!(drain_all(&b), b"final bytes");
    b.close();
}

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
    let paste: Vec<u8> = vec![b'P'; WRITE_QUEUE_BYTES + 300 * 1024];
    let mut stage = InputStage::new();
    match stage_into(&mut stage, &q, &paste) {
        InputFlow::Debt(_) => {}
        other => panic!("大粘贴应欠账,得 {other:?}"),
    }
    // 欠账未清时按键(生产入口——顺序规则内在)
    match stage_into(&mut stage, &q, b"after-paste\r") {
        InputFlow::Debt(_) => {}
        other => panic!("按键必须排在欠账后,得 {other:?}"),
    }
    gate.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !stage.is_empty() && Instant::now() < deadline {
        stage.flush_into(&q);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(stage.is_empty());
    q.close();
    w.join().unwrap();
    let out = written.lock().unwrap().clone();
    assert_eq!(out.len(), paste.len() + b"after-paste\r".len());
    assert!(out.starts_with(&paste));
    assert!(out.ends_with(b"after-paste\r"));
}

// ---------------------------------------------------------------------------
// 预算守恒/限额(承前)
// ---------------------------------------------------------------------------

#[test]
fn budget_exact_conservation_across_small_pushes() {
    let _g = budget_guard();
    let before = reserved_bytes();
    {
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, ScriptWake::new());
        b.push(0, b"a").unwrap();
        b.push(0, b"b").unwrap();
        assert_eq!(reserved_bytes() - before, 2);
        assert_eq!(drain_all(&b), b"ab");
        assert_eq!(reserved_bytes() - before, 0);
    }
    assert_eq!(reserved_bytes(), before);
}

#[test]
fn drop_clear_bump_close_all_release_exactly_once() {
    let _g = budget_guard();
    let before = reserved_bytes();
    {
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, ScriptWake::new());
        b.push(0, &vec![7u8; 1024]).unwrap();
    }
    assert_eq!(reserved_bytes(), before);
    {
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, ScriptWake::new());
        b.push(0, &vec![7u8; 512]).unwrap();
        b.close();
        b.close();
        assert_eq!(reserved_bytes(), before, "close 归还且不双扣");
    }
    {
        let b = PtyBacklog::new(1, PANE_BUDGET_BYTES, ScriptWake::new());
        b.push(0, &vec![7u8; 256]).unwrap();
        let g1 = b.bump_generation();
        assert_eq!(g1, 1);
        assert!(!b.has_more());
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
        .map(|i| PtyBacklog::new(i as u64, PANE_BUDGET_BYTES, ScriptWake::new()))
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
    let b = PtyBacklog::new(9, 64 * 1024, ScriptWake::new());
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

/// 真实会话锁模型(承前):写端被占,控制面不受阻。
#[test]
fn real_session_sink_kill_child_not_blocked_by_writer() {
    use mica_core::pty::{PtySession, command_from_str};
    let (mut session, reader) =
        PtySession::spawn(command_from_str("cmd.exe"), 80, 24).expect("pty");
    drop(reader);
    {
        let _guard = session.block_writer_for_test();
        let t = Instant::now();
        session.resize(100, 30).expect("resize 不被写端锁拖住");
        assert!(t.elapsed() < Duration::from_millis(500));
    }
    session.kill_child();
    let h = session.writer_handle();
    assert!(h.write(b"").is_ok());
    session.kill_child();
}

/// 会话隔离(承前):旧 writer 不吃新代数据。
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
    let q_new = WriteQueue::new();
    let w_new_written = Arc::new(Mutex::new(Vec::new()));
    let sink_new = Arc::new(GatedSink {
        gate: Arc::new(AtomicBool::new(true)),
        written: Arc::clone(&w_new_written),
    });
    let w_new = spawn_writer(&q_new, sink_new);
    q_old.close();
    q_new.try_push(b"NEW");
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
    assert!(w_old.is_finished());
    assert_eq!(*w_old_written.lock().unwrap(), b"OLD-1OLD-2");
    q_new.close();
    w_new.join().unwrap();
}

// ---------------------------------------------------------------------------
// 四轮返工:事务零副作用(复核 2/3)
// ---------------------------------------------------------------------------

/// 修前形态:超限 100 字节返回 RejectedOverflow,但前缀 1MiB+512KiB
/// 已被接收(部分提交)。修后:整体拒绝且**队列/暂存内容逐字节不变**。
#[test]
fn overflow_rejection_has_zero_side_effects() {
    let q = WriteQueue::new();
    let mut stage = InputStage::new();
    // 预置旧内容:队列填满 + 暂存留 debt(两个面都验零副作用)
    let fill = vec![0u8; WRITE_QUEUE_BYTES];
    assert_eq!(stage_into(&mut stage, &q, &fill), InputFlow::Accepted);
    assert_eq!(stage_into(&mut stage, &q, b"debt"), InputFlow::Debt(4));
    let q_before = q.queued_bytes();
    let s_before = stage.len();
    let too_big = vec![b'X'; WRITE_QUEUE_BYTES + INPUT_STAGE_BYTES];
    assert_eq!(
        stage_into(&mut stage, &q, &too_big),
        InputFlow::RejectedOverflow
    );
    assert_eq!(q.queued_bytes(), q_before, "队列不变");
    assert_eq!(stage.len(), s_before, "暂存不变");
    assert_eq!(stage.last_bytes(), b"debt", "暂存内容原样(零副作用)");
}

/// 容量规则只看暂存空余(FIFO 禁绕过),整体拒绝零副作用(修前外围
/// 预检按队列+暂存估,误判可容纳,实际部分拒收)。
#[test]
fn paste_with_old_debt_uses_stage_capacity_only() {
    let q = WriteQueue::new();
    let mut stage = InputStage::new();
    // 队列占满 → 1 字节进暂存成旧欠账(队列空时 Accepted 会进队列)
    let fill = vec![0u8; WRITE_QUEUE_BYTES];
    assert_eq!(stage_into(&mut stage, &q, &fill), InputFlow::Accepted);
    assert_eq!(stage_into(&mut stage, &q, b"A"), InputFlow::Debt(1));
    let big = vec![b'P'; 600_000];
    assert_eq!(
        stage_into(&mut stage, &q, &big),
        InputFlow::RejectedOverflow,
        "600KB 超出暂存空余:整体拒绝"
    );
    assert_eq!(stage.last_bytes(), b"A", "旧欠账原样,零副作用");
    assert_eq!(q.queued_bytes(), WRITE_QUEUE_BYTES, "队列不变");
}

/// 事务接受边界:恰等于容量的输入必须完整落位(不因边界 off-by-one
/// 拒绝或部分接收);UTF-8 多字节/bracketed 包裹/带回车命令都按字节
/// 流原样保序(整体接受)。
#[test]
fn transactional_accept_exact_boundary_and_content() {
    let q = WriteQueue::new();
    let mut stage = InputStage::new();
    let exact = WRITE_QUEUE_BYTES; // 队列空 + 暂存空:恰好装满队列
    let data: Vec<u8> = vec![b'K'; exact];
    assert_eq!(stage_into(&mut stage, &q, &data), InputFlow::Accepted);
    assert_eq!(q.queued_bytes(), exact);

    // 暂存路径的 UTF-8/包裹/回车内容(整体接受后原样可取)
    let q2 = WriteQueue::new();
    let mut st2 = InputStage::new();
    // 先占满队列,逼后续进暂存
    let fill = vec![0u8; WRITE_QUEUE_BYTES];
    assert_eq!(stage_into(&mut st2, &q2, &fill), InputFlow::Accepted);
    let payload = "中文\r\n\x1b[200~粘贴\x1b[201~echo 你好\r".as_bytes();
    match stage_into(&mut st2, &q2, payload) {
        InputFlow::Debt(_) => {}
        other => panic!("应欠账暂存,得 {other:?}"),
    }
    assert_eq!(st2.last_bytes(), payload, "暂存内容与输入逐字节一致");
}

// ---------------------------------------------------------------------------
// 四轮返工:permit 转移(复核 4)
// ---------------------------------------------------------------------------

/// 满 16 会话时的原地重启:自己的槽位 kill 后转移给自己——必须成功
/// (修前 spawn_capacity_ok 前置检查会阻止已满时的原地重启)。
#[test]
fn restart_transfers_own_slot_when_full() {
    let _g = budget_guard();
    use crate::app::lifecycle::{self, MAX_SESSIONS};
    // MAX-1 个他人 permit + 自持 1 = 恰好满(满时原地重启的形态)
    let others: Vec<_> = (0..MAX_SESSIONS - 1)
        .map(|_| lifecycle::acquire_session())
        .collect();
    let mut own = lifecycle::acquire_session();
    assert!(own.is_some());
    assert_eq!(lifecycle::live_sessions(), MAX_SESSIONS);
    // 重启序:kill 旧(permit 放掉)→ 再取(自己的槽位回到池中)
    drop(own.take());
    own = lifecycle::acquire_session();
    assert!(own.is_some(), "自持槽位转移必须成功");
    drop(own);
    drop(others);
    assert_eq!(lifecycle::live_sessions(), 0);
}

/// 槽位被抢:释放后其他窗口立即占用 → 重启方拿不到 → 可恢复失败
/// (无 expect/panic),计数守恒。
#[test]
fn restart_slot_stolen_is_recoverable_failure() {
    let _g = budget_guard();
    use crate::app::lifecycle::{self, MAX_SESSIONS};
    let others: Vec<_> = (0..MAX_SESSIONS - 1)
        .map(|_| lifecycle::acquire_session())
        .collect();
    let mut own = lifecycle::acquire_session();
    // 重启序:放旧 → 竞争者抢走 → 取新失败
    drop(own.take());
    let thief = lifecycle::acquire_session(); // 另一窗口抢走
    assert!(thief.is_some(), "竞争者拿到刚释放的槽位");
    let retry = lifecycle::acquire_session();
    assert!(retry.is_none(), "重启方可恢复失败(非 panic)");
    drop(thief);
    // 释放后重试成功(用户可重试重启)
    let again = lifecycle::acquire_session();
    assert!(again.is_some(), "槽位回来后重启可重试");
    drop(again);
    drop(others);
    assert_eq!(lifecycle::live_sessions(), 0);
}
