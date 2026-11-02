//! 孤儿 ConPTY 会话清理(M5c/T1 → 2026-11-01 T02 重造 → 同日补修加固)。
//!
//! 历史问题:强杀终端(taskkill /F 不走 Drop)后,ConPTY 宿主(conhost/
//! OpenConsole)成孤儿,管道断裂后 busy-loop 实测每个烧满一核。旧实现按
//! 全机"进程名 + 父 PID 已死"清杀——候选从未证明属于 Mica(可能误杀他
//! 人终端的宿主);`execute=false` 的观察模式仍会真实终止子进程。
//!
//! 模型(**归属可证明才动手**,2026-11-01 复核加固):
//! - spawn 前后进程快照差分,登记本会话的**确切身份三元组**:child
//!   (shell)pid+创建时间、host(ConPTY 宿主)pid+创建时间、owner(mica)
//!   pid+创建时间。宿主不再按父 PID/名称/时间窗推断——推断在"旧 mica
//!   PID 被复用又退出"等场景会把无关 conhost 拉进名单(复核实证)。
//! - 清杀只动**身份核验通过**的目标;执行器在同一个已打开句柄上复核
//!   创建时间再 TerminateProcess,覆盖快照→击杀之间的 PID 复用竞态。
//! - 记录有 `Running`/`Closing` 状态:关闭先标 Closing,**后台回收成功
//!   才移除**;回收降级 forget 时记录留存,下次启动按宿主身份清残留。
//!   击杀失败(执行返回 false)同样保留记录待重试,不静默丢弃。
//! - 注册表文件是跨进程共享资源(`--multi` 多实例):所有读改写在
//!   会话级命名互斥体事务内进行,写入走临时文件 + 原子替换;持锁进程
//!   崩溃由内核释放互斥体(abandoned 仍可获取),截断窗口不丢数据。
//!
//! 拓扑依据(portable-pty 0.9 源码核实):ConPTY 的 shell 与 conhost 都是
//! mica 的直接子进程(CreateProcessW 由本进程调用,二者是兄弟)。
//!
//! Job Object 方案评估(未采纳,留设计):`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//! 能让 taskkill /F 顺带收走全部子进程,从根上消灭孤儿;但它同时改变语义
//! ——mica 任何崩溃/退出都会立刻杀掉所有用户 shell(正在跑的命令一并损失),
//! 且嵌套 job(shell 自建 job、ConPTY 宿主行为)需单独实证。属行为变更,
//! 需要设计裁定,本轮不引入。

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::MoveFileExW;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, ReleaseMutex, WaitForSingleObject,
};

/// 本进程内的注册表操作互斥(线程级);跨进程由命名互斥体事务保证。
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// 跨进程命名互斥体(会话命名空间 Local\,按用户会话隔离)。
const REGISTRY_MUTEX_NAME: &str = "Local\\mica-pty-sessions";

// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

/// 记录状态:Running = 会话在用;Closing = 已发起关闭,等后台回收确认。
/// Closing 记录在下次启动时若 child 已死而宿主仍活,按登记身份清宿主。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordState {
    Running,
    Closing,
}

impl RecordState {
    fn as_char(self) -> char {
        match self {
            RecordState::Running => 'R',
            RecordState::Closing => 'C',
        }
    }
    fn from_char(c: &str) -> Option<Self> {
        match c {
            "R" => Some(RecordState::Running),
            "C" => Some(RecordState::Closing),
            _ => None,
        }
    }
}

/// 一条受跟踪的 PTY 会话:`*_start` 是进程创建时间(FILETIME 100ns ticks),
/// 用于防 PID 复用——PID 数字相同但创建时间不同就是另一个进程。
/// `host_*` 是 spawn 期快照差分出的 ConPTY 宿主确切身份;0 = 未知
/// (旧版 4 字段记录),未知宿主**永不击杀**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionRecord {
    pub mica_pid: u32,
    pub mica_start: u64,
    pub child_pid: u32,
    pub child_start: u64,
    pub host_pid: u32,
    pub host_start: u64,
    pub state: RecordState,
}

/// 快照里的一条进程信息。`created` 为 None 表示创建时间查询失败(保守
/// 处理:不杀、保留记录)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
    pub created: Option<u64>,
}

/// ConPTY 宿主进程名(小写):系统 conhost 与 WT 系 OpenConsole。
const HOST_NAMES: [&str; 2] = ["conhost.exe", "openconsole.exe"];

// ---------------------------------------------------------------------------
// 计划(纯计算)
// ---------------------------------------------------------------------------

/// 一次击杀:pid + **期望创建时间**。执行器必须在同一句柄上核验创建时间
/// 一致才允许 TerminateProcess——计划基于快照,快照后 PID 可能被复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KillStep {
    pub pid: u32,
    pub expect_start: u64,
    pub why: &'static str,
}

/// 单条记录的处置计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordAction {
    /// 保留:会话仍属存活 owner / 关闭在途 / 身份暂无法验证。
    Retain,
    /// 清理(命中身份的击杀步骤;child 与 host 都可为 None = 无可杀)。
    /// 执行全部成功 → 记录移除;任一失败 → 保留待下次。
    Cleanup {
        child: Option<KillStep>,
        host: Option<KillStep>,
    },
}

/// 单条记录的计划结果(附原因,诊断面)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordPlan {
    pub record: SessionRecord,
    pub action: RecordAction,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SweepPlan {
    pub records: Vec<RecordPlan>,
}

/// child/host 身份核验的三态(防 PID 复用:PID 相同创建时间不同 = 别人的)。
enum IdState {
    /// 活着且创建时间一致:确属登记的那个进程。
    Verified,
    /// 不在快照:已死。
    Dead,
    /// PID 被其他进程占用:绝不可动。
    Reused,
    /// 在快照但创建时间查询失败:无法证明,保守不动。
    Unknown,
}

fn verify_id(procs: &[ProcInfo], pid: u32, expect_start: u64) -> IdState {
    match procs.iter().find(|p| p.pid == pid) {
        None => IdState::Dead,
        Some(p) => match p.created {
            Some(t) if t == expect_start => IdState::Verified,
            Some(_) => IdState::Reused,
            None => IdState::Unknown,
        },
    }
}

fn kill_step(pid: u32, expect_start: u64, why: &'static str) -> KillStep {
    KillStep {
        pid,
        expect_start,
        why,
    }
}

/// 纯计算:由注册表记录 + 进程快照决定每条记录的处置。零副作用。
/// `self_id` = (本进程 pid, 创建时间),用于识别"本进程遗留的 Closing
/// 记录"(回收降级后的恢复路径)。
pub fn plan_sweep(
    records: &[SessionRecord],
    procs: &[ProcInfo],
    self_id: Option<(u32, u64)>,
) -> SweepPlan {
    let mut plan = SweepPlan::default();
    for r in records {
        let owner = procs.iter().find(|p| p.pid == r.mica_pid);
        let owner_alive_verified = owner.is_some_and(|p| p.created == Some(r.mica_start));
        let owner_unverifiable = matches!(owner, Some(p) if p.created.is_none());
        let owner_is_self = self_id == Some((r.mica_pid, r.mica_start));

        // ---- 本进程遗留的 Closing:恢复路径(降级 forget 后的宿主残留)----
        if r.state == RecordState::Closing && owner_is_self {
            match verify_id(procs, r.child_pid, r.child_start) {
                IdState::Dead => {
                    // child 已死(关闭时已杀),宿主若仍活按登记身份清
                    plan.records.push(host_only_plan(
                        r,
                        procs,
                        "本进程 Closing 遗留:child 已死,清宿主",
                    ));
                }
                _ => plan.records.push(RecordPlan {
                    record: *r,
                    action: RecordAction::Retain,
                    note: "本进程 Closing 仍在途(child 未死),保留".into(),
                }),
            }
            continue;
        }

        // ---- owner 存活或不可判:不动 ----
        if owner_alive_verified {
            plan.records.push(RecordPlan {
                record: *r,
                action: RecordAction::Retain,
                note: "owner 存活:会话合法".into(),
            });
            continue;
        }
        if owner_unverifiable {
            plan.records.push(RecordPlan {
                record: *r,
                action: RecordAction::Retain,
                note: format!("owner pid {} 创建时间未知,保守保留", r.mica_pid),
            });
            continue;
        }
        // 此后 owner 已死(不在快照)或 PID 已被复用:孤儿路径

        let mut child_step = None;
        let mut retain_child = false;
        match verify_id(procs, r.child_pid, r.child_start) {
            IdState::Verified => {
                child_step = Some(kill_step(
                    r.child_pid,
                    r.child_start,
                    "orphaned shell of dead mica",
                ));
            }
            IdState::Dead => {}                      // 已死:只剩宿主要清
            IdState::Reused => {}                    // 别人的进程:跳过(PID 不再是我们的)
            IdState::Unknown => retain_child = true, // 查不到创建时间:保留记录
        }

        let (host_step, retain_host) = host_step_of(r, procs);

        // 记录处置:凡有身份未验明(child/host 任一 Unknown)→ 保留待下次
        let action = if retain_child || retain_host {
            RecordAction::Retain
        } else {
            RecordAction::Cleanup {
                child: child_step,
                host: host_step,
            }
        };
        let note = match (&action, r.host_pid) {
            (RecordAction::Retain, _) => "身份待验明,保守保留".to_string(),
            (_, 0) => "旧版记录无宿主身份:只处置 child".to_string(),
            _ => "孤儿:按登记身份清 child/宿主".to_string(),
        };
        plan.records.push(RecordPlan {
            record: *r,
            action,
            note,
        });
    }
    plan
}

/// Closing 遗留的宿主清理计划(child 已死,只剩 host 身份核验)。
fn host_only_plan(r: &SessionRecord, procs: &[ProcInfo], note: &str) -> RecordPlan {
    let (host, retain_host) = host_step_of(r, procs);
    RecordPlan {
        record: *r,
        action: if retain_host {
            RecordAction::Retain
        } else {
            RecordAction::Cleanup { child: None, host }
        },
        note: note.to_string(),
    }
}

/// 宿主击杀步骤:只认登记身份(pid+创建时间)。未知宿主(host_pid=0)、
/// 宿主已死、PID 被复用、查询失败 → None(复用/查询失败给出保留或备注)。
fn host_step_of(r: &SessionRecord, procs: &[ProcInfo]) -> (Option<KillStep>, bool) {
    if r.host_pid == 0 {
        return (None, false); // 旧版记录:无宿主身份,永不推断
    }
    match verify_id(procs, r.host_pid, r.host_start) {
        IdState::Verified => (
            Some(kill_step(
                r.host_pid,
                r.host_start,
                "registered conpty host of dead session",
            )),
            false,
        ),
        IdState::Dead => (None, false),
        IdState::Reused => (None, false), // 宿主 PID 已易主:不动
        IdState::Unknown => (None, true), // 查询失败:保留记录待下次
    }
}

// ---------------------------------------------------------------------------
// 执行(唯一允许触碰进程的地方)
// ---------------------------------------------------------------------------

/// 终止执行器。
pub trait Terminator {
    /// 执行一步:打开 pid,**在同一句柄上核验创建时间 == expect_start**,
    /// 一致才 TerminateProcess。返回是否成功终止。
    fn terminate(&mut self, step: &KillStep) -> bool;
}

/// 生产执行器:OpenProcess(TERMINATE|QUERY_LIMITED) → 同句柄 GetProcessTimes
/// 核验 → TerminateProcess → CloseHandle。
pub struct WinTerminator;

impl Terminator for WinTerminator {
    fn terminate(&mut self, step: &KillStep) -> bool {
        // SAFETY: 目标句柄由本侧 OpenProcess 获得,用后即关;全程不放开
        // 句柄,核验与击杀针对同一个进程对象
        unsafe {
            let Ok(h) = OpenProcess(
                PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                step.pid,
            ) else {
                return false;
            };
            if h == HANDLE::default() {
                return false;
            }
            let mut create = FILETIME_ZERO;
            let mut exit = FILETIME_ZERO;
            let mut kernel = FILETIME_ZERO;
            let mut user = FILETIME_ZERO;
            let times_ok =
                GetProcessTimes(h, &mut create, &mut exit, &mut kernel, &mut user).is_ok();
            let created = ((create.dwHighDateTime as u64) << 32) | create.dwLowDateTime as u64;
            if !times_ok || created != step.expect_start {
                // 身份不符(快照后 PID 被复用/进程已退出):绝不击杀
                let _ = CloseHandle(h);
                return false;
            }
            let ok = windows::Win32::System::Threading::TerminateProcess(h, 1).is_ok();
            let _ = CloseHandle(h);
            ok
        }
    }
}

/// dry-run 执行器:只记录,零副作用。记录 (pid, expect_start) 供测试断言
/// 计划与执行调用一致。
#[derive(Default)]
pub struct NoopTerminator {
    pub calls: Vec<(u32, u64)>,
}

impl Terminator for NoopTerminator {
    fn terminate(&mut self, step: &KillStep) -> bool {
        self.calls.push((step.pid, step.expect_start));
        false // 永不"成功":dry-run 不产生任何真实处置
    }
}

/// 执行计划并决定记录去留。返回应保留的记录(全部击杀成功即移除,
/// 任一失败保留待下次重试——失败可安全重试的条件:击杀是幂等的
/// TerminateProcess,重试前会再次核验身份)。
pub fn apply_plan(plan: &SweepPlan, ex: &mut dyn Terminator) -> Vec<SessionRecord> {
    let mut retained = Vec::new();
    for rp in &plan.records {
        let steps = match &rp.action {
            RecordAction::Retain => {
                retained.push(rp.record);
                continue;
            }
            RecordAction::Cleanup { child, host } => child.iter().chain(host.iter()),
        };
        // 全部执行,不短路:child 击杀失败也要尝试宿主(宿主才是烧 CPU 的)
        let all_ok = steps.map(|s| ex.terminate(s)).fold(true, |a, b| a & b);
        if all_ok {
            eprintln!("[orphan] 清杀 pid({}):{}", rp.record.child_pid, rp.note);
        } else {
            eprintln!("[orphan] 击杀失败,保留记录待下次:{}", rp.note);
            retained.push(rp.record);
        }
    }
    retained
}

// ---------------------------------------------------------------------------
// 进程快照与身份查询
// ---------------------------------------------------------------------------

const FILETIME_ZERO: FILETIME = FILETIME {
    dwLowDateTime: 0,
    dwHighDateTime: 0,
};

/// 查询进程创建时间(FILETIME 100ns ticks)。进程不存在/查询失败 → None。
pub fn query_creation(pid: u32) -> Option<u64> {
    // SAFETY: 只读查询;句柄用后即关
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        if h == HANDLE::default() {
            return None;
        }
        let mut create = FILETIME_ZERO;
        let mut exit = FILETIME_ZERO;
        let mut kernel = FILETIME_ZERO;
        let mut user = FILETIME_ZERO;
        let ok = GetProcessTimes(h, &mut create, &mut exit, &mut kernel, &mut user).is_ok();
        let _ = CloseHandle(h);
        ok.then_some(((create.dwHighDateTime as u64) << 32) | create.dwLowDateTime as u64)
    }
}

/// 查询进程退出码:Some(259)=STILL_ACTIVE(仍活),Some(其他)=已终止,
/// None=进程不存在或查询失败。已终止但仍有存活句柄(泄漏句柄)的进程
/// 仍可 OpenProcess 成功——判定"死没死"必须看退出码,不能只看能否打开。
pub fn query_exit_code(pid: u32) -> Option<u32> {
    // SAFETY: 只读查询;句柄用后即关
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        if h == HANDLE::default() {
            return None;
        }
        let mut code: u32 = 0;
        let ok = windows::Win32::System::Threading::GetExitCodeProcess(h, &mut code).is_ok();
        let _ = CloseHandle(h);
        ok.then_some(code)
    }
}

/// 本进程身份(pid, 创建时间)。sweep 用于识别自己名下的 Closing 遗留。
pub fn self_identity() -> Option<(u32, u64)> {
    let pid = std::process::id();
    query_creation(pid).map(|start| (pid, start))
}

/// 轻量快照行(名字+父 PID,不查创建时间)。
struct ProcRow {
    pid: u32,
    parent: u32,
    name: String,
}

fn snapshot_rows() -> Vec<ProcRow> {
    let mut out = Vec::new();
    // SAFETY: 快照句柄本侧持有;遍历失败(竞态消失)按枚举结束处理
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let name_len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]).to_lowercase();
                out.push(ProcRow {
                    pid: entry.th32ProcessID,
                    parent: entry.th32ParentProcessID,
                    name,
                });
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// 进程快照(ProcInfo,创建时间由 enrich 按需补查)。
fn snapshot() -> Vec<ProcInfo> {
    snapshot_rows()
        .into_iter()
        .map(|r| ProcInfo {
            pid: r.pid,
            parent: r.parent,
            name: r.name,
            created: None,
        })
        .collect()
}

/// spawn 前快照探针:spawn 后差分出新宿主,得到确切身份(差分是精确的,
/// 不依赖时间窗启发;并发 spawn 也各自命中自己的新宿主)。
pub struct SpawnProbe {
    before: Vec<ProcRow>,
}

/// 在 PtySession::spawn **之前**调用。
pub fn before_spawn() -> SpawnProbe {
    SpawnProbe {
        before: snapshot_rows(),
    }
}

impl SpawnProbe {
    /// spawn 之后调用:after 快照 − before 快照 ∧ mica 直属 ∧ 宿主名 →
    /// 新宿主的 (pid, 创建时间)。找不到(宿主瞬死/枚举失败)→ None,
    /// 该会话的宿主身份记为未知(永不推断补杀)。
    pub fn host_identity(&self) -> Option<(u32, u64)> {
        let mica_pid = std::process::id();
        let before: HashSet<u32> = self.before.iter().map(|r| r.pid).collect();
        let candidates: Vec<u32> = snapshot_rows()
            .into_iter()
            .filter(|r| {
                r.pid != mica_pid
                    && r.parent == mica_pid
                    && !before.contains(&r.pid)
                    && HOST_NAMES.contains(&r.name.as_str())
            })
            .map(|r| r.pid)
            .collect();
        // 并发 spawn 理论上可能差分出多个新宿主:取创建时间最新的(本会话
        // 的宿主最后创建)。当前所有 spawn 都在 UI 线程串行,这里是双保险。
        candidates
            .iter()
            .filter_map(|&pid| query_creation(pid).map(|t| (pid, t)))
            .max_by_key(|&(_, t)| t)
    }
}

// ---------------------------------------------------------------------------
// 注册表(跨进程事务 + 原子写)
// ---------------------------------------------------------------------------

/// 跨进程注册表锁:命名互斥体(会话命名空间)。持锁进程死亡由内核释放
/// (WAIT_ABANDONED 仍视为获取成功)。
struct RegistryLock(HANDLE);

/// 获取跨进程锁;超时返回 None(调用方降级:跳过本轮或记日志,不死等)。
fn acquire_registry_lock(timeout_ms: u32) -> Option<RegistryLock> {
    // SAFETY: 命名常量;句柄由本侧持有,Drop 释放
    unsafe {
        let name = windows::core::HSTRING::from(REGISTRY_MUTEX_NAME);
        let Ok(h) = CreateMutexW(None, false, &name) else {
            return None;
        };
        if h.is_invalid() {
            let _ = CloseHandle(h);
            return None;
        }
        let wait = WaitForSingleObject(h, timeout_ms);
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            let _ = CloseHandle(h);
            return None;
        }
        Some(RegistryLock(h))
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        // SAFETY: 本侧持有且已获取;释放次序 Release → Close(Release 失败
        // 也不阻塞:进程退出内核兜底回收)
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// PTY 会话注册表:每行
/// `mica_pid mica_start child_pid child_start host_pid host_start state`。
/// 旧版 4 字段行解析为 (host=0/0, Running)——旧记录无宿主身份,只处置
/// child,永不推断宿主。
pub struct Registry {
    path: PathBuf,
}

impl Registry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 默认位置:`%LOCALAPPDATA%\mica\pty-sessions.txt`(机器本地状态,
    /// 不随漫游配置走)。
    pub fn default_path() -> PathBuf {
        let base = std::env::var("LOCALAPPDATA")
            .ok()
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".mica"));
        base.join("mica").join("pty-sessions.txt")
    }

    // ---- 无锁原语(调用方必须已持 RegistryLock 或 FILE_LOCK)----

    fn parse_line(line: &str) -> Option<SessionRecord> {
        let toks: Vec<&str> = line.split_whitespace().collect();
        match toks.len() {
            // 旧版 4 字段:无宿主身份、视为 Running(旧记录只处置 child)
            4 => Some(SessionRecord {
                mica_pid: toks[0].parse().ok()?,
                mica_start: toks[1].parse().ok()?,
                child_pid: toks[2].parse().ok()?,
                child_start: toks[3].parse().ok()?,
                host_pid: 0,
                host_start: 0,
                state: RecordState::Running,
            }),
            7 => Some(SessionRecord {
                mica_pid: toks[0].parse().ok()?,
                mica_start: toks[1].parse().ok()?,
                child_pid: toks[2].parse().ok()?,
                child_start: toks[3].parse().ok()?,
                host_pid: toks[4].parse().ok()?,
                host_start: toks[5].parse().ok()?,
                state: RecordState::from_char(toks[6])?,
            }),
            _ => None,
        }
    }

    fn read_unlocked(&self) -> Vec<SessionRecord> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines().filter_map(Self::parse_line).collect()
    }

    /// 原子写:临时文件 + MoveFileEx 替换。读者要么看到旧全文要么新全文,
    /// 不会读到截断半份;写中途崩溃只损失临时文件。
    fn write_unlocked(&self, records: &[SessionRecord]) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir); // 失败由后续写失败兜住
        }
        let tmp = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let body = {
            let mut s = String::new();
            for r in records {
                s.push_str(&format!(
                    "{} {} {} {} {} {} {}\n",
                    r.mica_pid,
                    r.mica_start,
                    r.child_pid,
                    r.child_start,
                    r.host_pid,
                    r.host_start,
                    r.state.as_char()
                ));
            }
            s
        };
        let Ok(mut f) = std::fs::File::create(&tmp) else {
            return; // 注册表是崩溃清理的辅助面:写失败不致命
        };
        if f.write_all(body.as_bytes()).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        let _ = f.sync_all();
        drop(f);
        // SAFETY: 路径来自本侧构造;REPLACE_EXISTING + WRITE_THROUGH 原子替换
        unsafe {
            let src = windows::core::HSTRING::from(tmp.as_os_str());
            let dst = windows::core::HSTRING::from(self.path.as_os_str());
            let flags = windows::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                | windows::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
            if MoveFileExW(&src, &dst, flags).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    fn modify<F: FnOnce(&mut Vec<SessionRecord>)>(&self, f: F) {
        // 线程锁 + 跨进程锁双层:线程锁防本进程并发,命名锁防多实例
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(_xlock) = acquire_registry_lock(5_000) else {
            eprintln!("[orphan] 注册表跨进程锁超时,本轮操作跳过");
            return;
        };
        let mut all = self.read_unlocked();
        f(&mut all);
        self.write_unlocked(&all);
    }

    // ---- 公开操作(自带事务)----

    /// 登记新会话(spawn 成功后)。
    pub fn track(&self, rec: SessionRecord) {
        self.modify(|all| {
            all.retain(|r| r.child_pid != rec.child_pid); // 同 pid 旧记录先清
            all.push(rec);
        });
    }

    /// 标记关闭在途:**不移除**——后台回收成功(reaper Drop 完成)才由
    /// remove 移除;降级 forget 时记录留存,下次启动清宿主残留。
    pub fn mark_closing(&self, child_pid: u32) {
        self.modify(|all| {
            if let Some(r) = all
                .iter_mut()
                .find(|r| r.child_pid == child_pid && r.state == RecordState::Running)
            {
                r.state = RecordState::Closing;
            }
        });
    }

    /// 移除记录(Closing → Closed:仅由回收确认/干净退出调用)。
    pub fn remove(&self, child_pid: u32) {
        self.modify(|all| {
            all.retain(|r| r.child_pid != child_pid);
        });
    }

    /// 进程退出序:移除本进程名下全部记录(子已杀;reaper 未及确认的
    /// Closing 一并清账,幂等)。
    pub fn clear_for(&self, mica_pid: u32) {
        self.modify(|all| {
            all.retain(|r| r.mica_pid != mica_pid);
        });
    }

    /// 当前登记的全部会话(诊断/测试面)。只读,自带短事务。
    pub fn records(&self) -> Vec<SessionRecord> {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(_xlock) = acquire_registry_lock(2_000) else {
            return Vec::new(); // 诊断面:拿不到锁就报空,不阻塞
        };
        self.read_unlocked()
    }

    /// sweep 事务专用:在**同一把跨进程锁**内执行 read → f(返回 retained)
    /// → 写回。f 拿到的是刚读出的记录与锁保护期,保证读改写原子。
    /// retained 与读出集相同(dry-run)时不落盘。
    fn sweep_transaction<T, F: FnOnce(Vec<SessionRecord>) -> (T, Vec<SessionRecord>)>(
        &self,
        f: F,
    ) -> Option<T> {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(_xlock) = acquire_registry_lock(10_000) else {
            eprintln!("[orphan] sweep 跨进程锁超时,本轮清杀跳过");
            return None;
        };
        let records = self.read_unlocked();
        let (out, retained) = f(records.clone());
        if retained != records {
            self.write_unlocked(&retained);
        }
        Some(out)
    }
}

/// 供测试检查注册表路径。
#[cfg(test)]
impl Registry {
    fn path_ref(&self) -> &std::path::Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// 顶层入口
// ---------------------------------------------------------------------------

/// 登记新会话(pty spawn 成功后调用;probe 来自 spawn 前的 before_spawn)。
/// 登记失败只静默——注册表是崩溃清理的辅助面,不影响运行。
pub fn track_session_since(probe: &SpawnProbe, child_pid: u32) {
    let Some(child_start) = query_creation(child_pid) else {
        return;
    };
    let mica_pid = std::process::id();
    let Some(mica_start) = query_creation(mica_pid) else {
        return;
    };
    let (host_pid, host_start) = probe.host_identity().unwrap_or((0, 0));
    Registry::new(Registry::default_path()).track(SessionRecord {
        mica_pid,
        mica_start,
        child_pid,
        child_start,
        host_pid,
        host_start,
        state: RecordState::Running,
    });
}

/// 发起关闭:标记 Closing(记录保留)。回收确认后由 remove_closed 移除。
pub fn mark_session_closing(child_pid: u32) {
    Registry::new(Registry::default_path()).mark_closing(child_pid);
}

/// 后台回收成功的确认:记录移除(reaper Drop 完成后调用)。
pub fn remove_closed_session(child_pid: u32) {
    Registry::new(Registry::default_path()).remove(child_pid);
}

/// 进程退出序:本进程记录全清。
pub fn clear_sessions_for_exit() {
    Registry::new(Registry::default_path()).clear_for(std::process::id());
}

/// 启动期清杀入口:跨进程锁事务内 读 → 快照 → 纯计划 →(execute 时)
/// 执行 → 按结果写回。dry-run(execute=false)不执行也不写回。
/// 返回成功终止数。
pub fn sweep_owned(registry: &Registry, execute: bool) -> usize {
    registry
        .sweep_transaction(|records| {
            if records.is_empty() {
                return (0usize, records);
            }
            let mut procs = snapshot();
            // 创建时间只查:记录的 child/owner/host 与本进程
            let interesting: HashSet<u32> = records
                .iter()
                .flat_map(|r| [r.child_pid, r.mica_pid, r.host_pid])
                .filter(|&p| p != 0)
                .chain(std::iter::once(std::process::id()))
                .collect();
            for p in procs.iter_mut() {
                if interesting.contains(&p.pid) {
                    p.created = query_creation(p.pid);
                }
            }
            // 快照为空 = 枚举失败(真实机器恒有进程):全部保留,不动手
            if procs.is_empty() {
                eprintln!(
                    "[orphan] 进程快照枚举失败,本轮不清杀(保留 {} 条记录)",
                    records.len()
                );
                return (0, records);
            }
            let plan = plan_sweep(&records, &procs, self_identity());
            if execute {
                let mut ex = CountingTerminator::new(WinTerminator);
                let retained = apply_plan(&plan, &mut ex);
                (ex.succeeded(), retained)
            } else {
                // dry-run:只把计划打到日志,不执行、不产生"击杀失败"
                // 噪音;零真实终止由 orphan_dry_run_never_terminates
                // (纯计划面)与集成测试(真进程面)两级锁死
                for rp in &plan.records {
                    eprintln!(
                        "[orphan] dry-run: child pid {} → {}",
                        rp.record.child_pid, rp.note
                    );
                }
                (0, records)
            }
        })
        .unwrap_or(0)
}

/// 计数包装:sweep 返回值需要"成功终止的进程数"。
struct CountingTerminator<T: Terminator> {
    inner: T,
    succeeded: usize,
}

impl<T: Terminator> CountingTerminator<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            succeeded: 0,
        }
    }
    fn succeeded(&self) -> usize {
        self.succeeded
    }
}

impl<T: Terminator> Terminator for CountingTerminator<T> {
    fn terminate(&mut self, step: &KillStep) -> bool {
        let ok = self.inner.terminate(step);
        self.succeeded += ok as usize;
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec_running(mica: (u32, u64), child: (u32, u64), host: (u32, u64)) -> SessionRecord {
        SessionRecord {
            mica_pid: mica.0,
            mica_start: mica.1,
            child_pid: child.0,
            child_start: child.1,
            host_pid: host.0,
            host_start: host.1,
            state: RecordState::Running,
        }
    }

    fn rec_closing(mica: (u32, u64), child: (u32, u64), host: (u32, u64)) -> SessionRecord {
        SessionRecord {
            state: RecordState::Closing,
            ..rec_running(mica, child, host)
        }
    }

    fn proc(pid: u32, parent: u32, name: &str, created: u64) -> ProcInfo {
        ProcInfo {
            pid,
            parent,
            name: name.into(),
            created: Some(created),
        }
    }

    /// 纯计划面的击杀步骤收集。
    fn kill_pids(plan: &SweepPlan) -> Vec<u32> {
        plan.records
            .iter()
            .flat_map(|rp| match &rp.action {
                RecordAction::Retain => vec![],
                RecordAction::Cleanup { child, host } => {
                    child.iter().chain(host.iter()).map(|s| s.pid).collect()
                }
            })
            .collect()
    }

    /// 保留的记录数。
    fn retain_count(plan: &SweepPlan) -> usize {
        plan.records
            .iter()
            .filter(|rp| rp.action == RecordAction::Retain)
            .count()
    }

    #[test]
    fn empty_registry_plans_nothing() {
        let plan = plan_sweep(&[], &[proc(1, 0, "init", 1)], None);
        assert!(plan.records.is_empty());
    }

    /// 核心:dry-run 在存在真实孤儿计划时零次真实终止;Noop 记录与计划
    /// 的 (pid, expect_start) 完全一致(执行面与计划面同源)。
    #[test]
    fn orphan_dry_run_never_terminates() {
        let records = [rec_running((100, 10), (200, 50), (300, 55))];
        let procs = [
            proc(1, 0, "init", 1),
            proc(200, 100, "powershell.exe", 50),
            proc(300, 100, "conhost.exe", 55),
        ];
        let plan = plan_sweep(&records, &procs, None);
        assert_eq!(kill_pids(&plan), vec![200, 300], "孤儿 child+宿主都要清");
        let mut noop = NoopTerminator::default();
        let retained = apply_plan(&plan, &mut noop);
        assert_eq!(retained.len(), 1, "noop 全失败 → 保留(dry-run 不写回)");
        let expect: Vec<(u32, u64)> = [(200u32, 50u64), (300, 55)].to_vec();
        assert_eq!(noop.calls, expect, "执行调用与计划同源且带身份");
    }

    /// owner 存活:保留,零步骤。
    #[test]
    fn orphan_unowned_process_is_ignored() {
        let records = [rec_running((100, 10), (200, 50), (300, 55))];
        let procs = [
            proc(100, 1, "mica.exe", 10),
            proc(200, 100, "powershell.exe", 50),
        ];
        let plan = plan_sweep(&records, &procs, None);
        assert_eq!(kill_pids(&plan), Vec::<u32>::new());
        assert_eq!(retain_count(&plan), 1);
    }

    /// 复核回归(2026-11-01):owner 不在快照(死后 PID 复用者又退出,
    /// 无复用屏障)时,与登记宿主**身份不同**的无关 conhost——即使它的
    /// 父 PID 恰好等于死掉的 mica_pid——绝不能进清杀名单。宿主只认登记
    /// 的 (pid, 创建时间),不做父 PID/名称/时间窗推断。
    #[test]
    fn unregistered_host_is_never_targeted_without_owner() {
        let records = [rec_running((100, 10), (200, 50), (300, 55))];
        let procs = [
            // 登记宿主已死;另一个无关 conhost 恰是 mica_pid 的孩子
            proc(999, 100, "conhost.exe", 900),
            proc(200, 100, "powershell.exe", 50),
        ];
        let plan = plan_sweep(&records, &procs, None);
        assert_eq!(
            kill_pids(&plan),
            vec![200],
            "只杀登记身份核验通过的 child;无关 conhost(999)不在名单"
        );
    }

    /// child PID 被复用:绝不杀 child;登记宿主仍按身份清。
    #[test]
    fn child_pid_reuse_is_never_terminated() {
        let records = [rec_running((100, 10), (200, 50), (300, 55))];
        let procs = [
            proc(200, 999, "notepad.exe", 900), // PID 200 已是别人的
            proc(300, 100, "conhost.exe", 55),
        ];
        let plan = plan_sweep(&records, &procs, None);
        assert_eq!(kill_pids(&plan), vec![300]);
    }

    /// 宿主 PID 被复用:绝不杀宿主。
    #[test]
    fn host_pid_reuse_is_never_terminated() {
        let records = [rec_running((100, 10), (200, 50), (300, 55))];
        let procs = [
            proc(200, 100, "powershell.exe", 50),
            proc(300, 111, "conhost.exe", 900), // 宿主 PID 已易主
        ];
        let plan = plan_sweep(&records, &procs, None);
        assert_eq!(kill_pids(&plan), vec![200], "child 清,易主宿主不动");
    }

    /// 旧版 4 字段记录(host_pid=0):只处置 child,永不推断宿主。
    #[test]
    fn legacy_record_without_host_identity_skips_host() {
        let records = [SessionRecord {
            mica_pid: 100,
            mica_start: 10,
            child_pid: 200,
            child_start: 50,
            host_pid: 0,
            host_start: 0,
            state: RecordState::Running,
        }];
        let procs = [
            proc(200, 100, "powershell.exe", 50),
            proc(300, 100, "conhost.exe", 55), // 看起来像宿主,但没有登记
        ];
        let plan = plan_sweep(&records, &procs, None);
        assert_eq!(kill_pids(&plan), vec![200], "未登记宿主绝不击杀");
    }

    /// 本进程 Closing 遗留(child 已死,宿主还活):恢复路径清宿主。
    #[test]
    fn closing_leftover_of_self_cleans_host() {
        let me = (4242u32, 7u64);
        let records = [rec_closing(me, (200, 50), (300, 55))];
        let procs = [proc(300, 4242, "conhost.exe", 55)]; // child 已死
        let plan = plan_sweep(&records, &procs, Some(me));
        assert_eq!(kill_pids(&plan), vec![300]);
    }

    /// 本进程 Closing 但 child 还活(关闭在途):保留,不动。
    #[test]
    fn closing_in_flight_is_retained() {
        let me = (4242u32, 7u64);
        let records = [rec_closing(me, (200, 50), (300, 55))];
        let procs = [
            proc(200, 4242, "powershell.exe", 50),
            proc(300, 4242, "conhost.exe", 55),
        ];
        let plan = plan_sweep(&records, &procs, Some(me));
        assert_eq!(kill_pids(&plan), Vec::<u32>::new());
        assert_eq!(retain_count(&plan), 1);
    }

    /// 他进程(owner 死)的 Closing 记录:child 已死 → 宿主清理。
    #[test]
    fn closing_of_dead_owner_cleans_host() {
        let records = [rec_closing((100, 10), (200, 50), (300, 55))];
        let procs = [proc(300, 100, "conhost.exe", 55)];
        let plan = plan_sweep(&records, &procs, None);
        assert_eq!(kill_pids(&plan), vec![300]);
    }

    /// child 创建时间查询失败:保留(保守),不杀不弃。
    #[test]
    fn child_creation_unknown_retains() {
        let records = [rec_running((100, 10), (200, 50), (0, 0))];
        let mut child = proc(200, 100, "powershell.exe", 50);
        child.created = None;
        let procs = [child];
        let plan = plan_sweep(&records, &procs, None);
        assert_eq!(kill_pids(&plan), Vec::<u32>::new());
        assert_eq!(retain_count(&plan), 1);
    }

    /// owner 创建时间查询失败:保守保留。
    #[test]
    fn owner_creation_unknown_retains() {
        let records = [rec_running((100, 10), (200, 50), (300, 55))];
        let mut owner = proc(100, 1, "mica.exe", 10);
        owner.created = None;
        let procs = [owner, proc(200, 100, "powershell.exe", 50)];
        let plan = plan_sweep(&records, &procs, None);
        assert_eq!(kill_pids(&plan), Vec::<u32>::new());
        assert_eq!(retain_count(&plan), 1);
    }

    /// 空快照(枚举失败):零击杀是安全底线。计划层的契约是"调用方
    /// 必须传完整快照"(sweep_owned 对空快照直接拒绝计划),这里锁住
    /// "即使误传空快照也不会产生击杀步骤"。
    #[test]
    fn enumeration_failure_is_conservative() {
        let records = [rec_running((100, 10), (200, 50), (300, 55))];
        let plan = plan_sweep(&records, &[], None);
        assert!(kill_pids(&plan).is_empty(), "看不见任何进程时绝不动手");
    }

    /// 执行失败保留:mock 执行器对宿主失败 → 该记录保留,child 成功的
    /// 另一条记录正常移除。
    #[test]
    fn execution_failure_retains_record() {
        let r_ok = rec_running((100, 10), (200, 50), (300, 55));
        let r_bad_host = rec_running((101, 11), (210, 60), (310, 65));
        let procs = [
            proc(200, 100, "powershell.exe", 50),
            proc(300, 100, "conhost.exe", 55),
            proc(210, 101, "powershell.exe", 60),
            proc(310, 101, "conhost.exe", 65),
        ];
        let plan = plan_sweep(&[r_ok, r_bad_host], &procs, None);
        struct FlakyHost;
        impl Terminator for FlakyHost {
            fn terminate(&mut self, s: &KillStep) -> bool {
                s.pid != 310 // 310(宿主)失败,其余成功
            }
        }
        let retained = apply_plan(&plan, &mut FlakyHost);
        assert_eq!(retained.len(), 1, "宿主击杀失败的记录保留");
        assert_eq!(retained[0].child_pid, 210);
    }

    /// 注册表往返:7 字段行 + 状态;mark_closing 状态迁移;旧 4 字段行
    /// 兼容;坏行跳过;原子替换后内容完整。
    #[test]
    fn registry_roundtrip_states_and_legacy() {
        let dir = std::env::temp_dir().join(format!(
            "mica-orphan-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = Registry::new(dir.join("sessions.txt"));
        let a = rec_running((1, 10), (2, 20), (3, 30));
        let b = rec_running((1, 10), (4, 40), (5, 50));
        reg.track(a);
        reg.track(b);
        assert_eq!(reg.records().len(), 2);
        // 状态迁移:Running → Closing(不移除)
        reg.mark_closing(2);
        let got = reg.records();
        assert_eq!(got.len(), 2, "mark_closing 不移除记录");
        assert_eq!(
            got.iter().find(|r| r.child_pid == 2).map(|r| r.state),
            Some(RecordState::Closing)
        );
        // 回收确认移除
        reg.remove(2);
        assert_eq!(reg.records(), vec![b]);
        // 旧版 4 字段行 + 坏行
        let path = reg.path_ref();
        std::fs::write(path, "garbage\n1 10 4 40 extra\n7 8 9 10\n").unwrap();
        let legacy = reg.records();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].child_pid, 9);
        assert_eq!(legacy[0].host_pid, 0, "旧版行无宿主身份");
        assert_eq!(legacy[0].state, RecordState::Running);
        std::fs::remove_dir_all(&dir).ok();
    }
}
