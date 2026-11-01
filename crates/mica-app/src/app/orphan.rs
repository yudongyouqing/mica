//! 孤儿 ConPTY 宿主清理(M5c/T1 → 2026-11-01 T02 重造)。
//!
//! 历史问题:强杀终端(taskkill /F 不走 Drop)后,ConPTY 宿主(conhost/
//! OpenConsole)成孤儿,管道断裂后 busy-loop 实测每个烧满一核。旧实现按
//! 全机"进程名 + 父 PID 已死"清杀——两个缺陷:候选从未证明属于 Mica
//! (可能误杀他人终端的宿主);`execute=false` 的观察模式仍会真实终止
//! 子进程(终止路径没有走 execute 决策)。
//!
//! 新模型:**归属可证明才动手**。
//! - 每次 spawn 会话时把 `{mica_pid, mica_start, child_pid, child_start}`
//!   记入本地注册表(`%LOCALAPPDATA%\mica\pty-sessions.txt`);干净关闭时
//!   移除,进程死亡(强杀/崩溃)时条目留存。
//! - 下次启动对每条记录做验证:child_pid 存活且**创建时间一致**(防 PID
//!   复用)才算我们的;owner(mica_pid)已死才构成孤儿。拓扑依据(portable-
//!   pty 0.9 源码核实):ConPTY 的 shell 与 conhost 都是 mica 的**直接子
//!   进程**(CreateProcessW 由本进程调用,二者是兄弟)。
//! - 计划与执行分离:`plan_sweep` 是纯计算;一切终止都经 `execute_plan`
//!   走同一个 execute 决策——dry-run 由 NoopTerminator 证明零次终止。
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

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

/// 注册表文件操作的全局锁(多窗口线程都会 track/untrack)。
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// 一条受跟踪的 PTY 会话。`*_start` 是进程创建时间(FILETIME 100ns ticks),
/// 用于防 PID 复用——PID 数字相同但创建时间不同就是另一个进程。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionRecord {
    pub mica_pid: u32,
    pub mica_start: u64,
    pub child_pid: u32,
    pub child_start: u64,
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

/// 清杀计划:纯计算产物,不触碰任何进程。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepPlan {
    /// 按序终止:每条孤儿记录先杀 shell(防 defterm 弹窗接管,1.0.4 实证)
    /// 再杀宿主。
    pub kill: Vec<KillStep>,
    /// 保留进注册表的记录(会话仍属存活实例,或暂无法验证)。
    pub retained: Vec<SessionRecord>,
    /// 丢弃的陈旧记录(child 已死且宿主无候选 / PID 已被复用)。
    pub discarded: usize,
    /// 每个跳过/丢弃决定的原因(诊断面,eprintln 级)。
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KillStep {
    pub pid: u32,
    pub why: &'static str,
}

/// child 验证的三态(防 PID 复用是核心:PID 相同但创建时间不同就是
/// 别人的进程,绝不动)。
enum ChildState {
    /// 活着且创建时间一致:确属我们的 shell。
    Verified,
    /// 不在快照:已死(宿主可能仍烧 CPU——2026-10-03 事故形态)。
    Dead,
    /// PID 已被其他进程占用:绝不可终止。
    Reused,
    /// 在快照但创建时间查询失败:无法证明,保守不动。
    Unknown,
}

/// 纯计算:由注册表记录 + 进程快照决定清杀计划。零副作用——单测的
/// 全部输入都是普通数据。
pub fn plan_sweep(records: &[SessionRecord], procs: &[ProcInfo]) -> SweepPlan {
    let mut plan = SweepPlan::default();
    let mut host_victims: Vec<(u32, &'static str)> = Vec::new();
    let mut host_seen: HashSet<u32> = HashSet::new();

    for r in records {
        // ---- 1) child 验证(防 PID 复用)----
        let child_state = match procs.iter().find(|p| p.pid == r.child_pid) {
            None => ChildState::Dead,
            Some(p) => match p.created {
                Some(t) if t == r.child_start => ChildState::Verified,
                Some(_) => ChildState::Reused,
                None => ChildState::Unknown,
            },
        };
        if let ChildState::Reused = child_state {
            plan.notes.push(format!(
                "child pid {} 已被其他进程复用,不动该进程",
                r.child_pid
            ));
        }

        // ---- 2) owner(mica)验证:活着且创建时间一致 → 不是孤儿 ----
        let owner = procs.iter().find(|p| p.pid == r.mica_pid);
        match owner {
            Some(p) if p.created == Some(r.mica_start) => {
                // 归属进程仍存活:会话合法,保留记录
                plan.retained.push(*r);
                if let ChildState::Unknown = child_state {
                    plan.notes
                        .push(format!("child pid {} 创建时间未知,保留观察", r.child_pid));
                }
                continue;
            }
            Some(p) if p.created.is_none() => {
                // owner PID 活着但无法验证创建时间:无法排除是本尊,保守保留
                plan.retained.push(*r);
                plan.notes
                    .push(format!("owner pid {} 创建时间未知,保守保留", r.mica_pid));
                continue;
            }
            _ => {} // owner 不在快照(已死)或 PID 已被新进程复用:孤儿路径
        }

        // ---- 3) 孤儿路径:先记 child 终止,再找宿主候选 ----
        if let ChildState::Verified = child_state {
            plan.kill.push(KillStep {
                pid: r.child_pid,
                why: "orphaned shell of dead mica",
            });
        }

        // 宿主候选:mica 的直接子进程 + 宿主名 + 创建时间在 owner 出生之后
        // (owner 死后该 PID 若被复用,复用者的宿主创建时间必然更晚;若
        // 复用者仍活着,再加"早于复用者出生"上界,双保险排除其宿主)
        let reuse_barrier = match owner {
            Some(p) => p.created, // PID 被复用且活着:上界 = 复用者创建时间
            None => None,         // owner 彻底不在:无上界
        };
        for p in procs {
            if p.parent != r.mica_pid
                || !HOST_NAMES.contains(&p.name.as_str())
                || host_seen.contains(&p.pid)
            {
                continue;
            }
            let Some(created) = p.created else {
                plan.notes
                    .push(format!("宿主 pid {} 创建时间未知,跳过", p.pid));
                continue;
            };
            if created < r.mica_start {
                continue; // 早于 owner 出生:不是 owner 的孩子
            }
            if let Some(barrier) = reuse_barrier
                && created >= barrier
            {
                continue; // 晚于复用者出生:是复用者的宿主,不动
            }
            host_seen.insert(p.pid);
            host_victims.push((p.pid, "conpty host of dead mica"));
        }

        // 记录处置:child 已验证(已安排终止)/已死/被复用 → 丢弃;
        // 创建时间未知 → 保留待下次
        match child_state {
            ChildState::Unknown => plan.retained.push(*r),
            _ => plan.discarded += 1,
        }
    }

    plan.kill.extend(
        host_victims
            .into_iter()
            .map(|(pid, why)| KillStep { pid, why }),
    );
    plan
}

/// 终止执行器:唯一允许触碰进程的地方。
pub trait Terminator {
    /// 打开并终止 pid。返回是否成功(测试用记录型实现观测调用)。
    fn terminate(&mut self, pid: u32) -> bool;
}

/// 生产执行器:OpenProcess(PROCESS_TERMINATE) → TerminateProcess → 关句柄。
pub struct WinTerminator;

impl Terminator for WinTerminator {
    fn terminate(&mut self, pid: u32) -> bool {
        // SAFETY: 目标句柄由本侧 OpenProcess 获得,用后即关
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) else {
                return false;
            };
            if h == HANDLE::default() {
                return false;
            }
            let ok = windows::Win32::System::Threading::TerminateProcess(h, 1).is_ok();
            let _ = CloseHandle(h);
            ok
        }
    }
}

/// dry-run 执行器:只记录,零副作用。`sweep_owned(execute=false)` 用它,
/// "观察模式不终止任何进程"由测试锁死。
#[derive(Default)]
pub struct NoopTerminator {
    pub calls: Vec<u32>,
}

impl Terminator for NoopTerminator {
    fn terminate(&mut self, pid: u32) -> bool {
        self.calls.push(pid);
        false
    }
}

/// 执行计划:一切终止的唯一入口。返回成功终止数。
pub fn execute_plan(plan: &SweepPlan, ex: &mut dyn Terminator) -> usize {
    let mut killed = 0;
    for step in &plan.kill {
        if ex.terminate(step.pid) {
            killed += 1;
        }
    }
    killed
}

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

/// 进程快照(名字+父 PID);创建时间仅对需要的 pid 补查(全量 OpenProcess
/// 每进程一次太重,按记录/宿主候选收集)。
fn snapshot() -> Vec<ProcInfo> {
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
                out.push(ProcInfo {
                    pid: entry.th32ProcessID,
                    parent: entry.th32ParentProcessID,
                    name,
                    created: None,
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

/// 对 plan 需要验证的 pid 批量补查创建时间(child/owner/宿主候选)。
fn enrich_creations(procs: &mut [ProcInfo], interesting: &dyn Fn(&ProcInfo) -> bool) {
    for p in procs {
        if interesting(p) {
            p.created = query_creation(p.pid);
        }
    }
}

/// PTY 会话注册表:每行 `mica_pid mica_start child_pid child_start`。
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

    fn read(&self) -> Vec<SessionRecord> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| {
                let mut it = line.split_whitespace();
                let vals: [u64; 4] = [
                    it.next()?.parse().ok()?,
                    it.next()?.parse().ok()?,
                    it.next()?.parse().ok()?,
                    it.next()?.parse().ok()?,
                ];
                if it.next().is_some() {
                    return None; // 多余字段:格式不认识,跳过
                }
                Some(SessionRecord {
                    mica_pid: vals[0] as u32,
                    mica_start: vals[1],
                    child_pid: vals[2] as u32,
                    child_start: vals[3],
                })
            })
            .collect()
    }

    fn write(&self, records: &[SessionRecord]) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir); // 失败由后续写失败兜住
        }
        let Ok(mut f) = std::fs::File::create(&self.path) else {
            return; // 注册表是崩溃清理的辅助面:写失败不致命
        };
        for r in records {
            let _ = writeln!(
                f,
                "{} {} {} {}",
                r.mica_pid, r.mica_start, r.child_pid, r.child_start
            );
        }
    }

    /// 记录新会话(在 pty spawn 成功后调用)。
    pub fn track(&self, rec: SessionRecord) {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.read();
        all.retain(|r| r.child_pid != rec.child_pid); // 同 pid 旧记录先清
        all.push(rec);
        self.write(&all);
    }

    /// 当前登记的全部会话(诊断/测试面)。
    pub fn records(&self) -> Vec<SessionRecord> {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.read()
    }

    /// 干净关闭时移除该 child 的记录。
    pub fn untrack(&self, child_pid: u32) {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.read();
        let before = all.len();
        all.retain(|r| r.child_pid != child_pid);
        if all.len() != before {
            self.write(&all);
        }
    }

    /// 进程退出序:清掉本进程名下的全部记录(整体退出,无残留会话)。
    pub fn clear_for(&self, mica_pid: u32) {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.read();
        let before = all.len();
        all.retain(|r| r.mica_pid != mica_pid);
        if all.len() != before {
            self.write(&all);
        }
    }
}

/// 启动期清杀入口:读注册表 → 快照 → 纯计划 →(execute 时)执行 →
/// 把保留记录写回。返回成功终止数。dry-run(execute=false)走
/// NoopTerminator——除读文件/读快照外零副作用。
pub fn sweep_owned(registry: &Registry, execute: bool) -> usize {
    let records = registry.read();
    let mut procs = snapshot();
    // 空快照 = 枚举失败(真实机器恒有 System 等进程)。"什么都没看见"
    // 不等于"全都死了"——按看不见处置会丢弃全部记录;保守起见本轮
    // 什么都不做,记录原样留给下次
    if procs.is_empty() {
        if !records.is_empty() {
            eprintln!(
                "[orphan] 进程快照枚举失败,本轮不清杀(保留 {} 条记录)",
                records.len()
            );
        }
        return 0;
    }
    // 创建时间只查三类:记录的 child/owner,以及可能是宿主的候选
    let child_pids: HashSet<u32> = records.iter().map(|r| r.child_pid).collect();
    let owner_pids: HashSet<u32> = records.iter().map(|r| r.mica_pid).collect();
    enrich_creations(&mut procs, &|p| {
        child_pids.contains(&p.pid)
            || owner_pids.contains(&p.pid)
            || (HOST_NAMES.contains(&p.name.as_str()) && owner_pids.contains(&p.parent))
    });

    let plan = plan_sweep(&records, &procs);
    for note in &plan.notes {
        eprintln!("[orphan] {note}");
    }
    let killed = if execute {
        execute_plan(&plan, &mut WinTerminator)
    } else {
        // NoopTerminator 只记录不触碰:观察模式的"零真实终止"由
        // orphan_dry_run_never_terminates 与 dry_run_leaves_real_processes_
        // and_registry_untouched 两级测试锁死(纯计划面 + 真进程面)
        let mut noop = NoopTerminator::default();
        let _ = execute_plan(&plan, &mut noop); // Noop 恒返 false → killed=0
        0
    };
    // 写回保留集;丢弃的记录随重写自然消失。dry-run 不写——观察模式
    // 连注册表都不改,保证随后可做真实清杀
    if execute && (!plan.retained.is_empty() || records.len() != plan.retained.len()) {
        registry.write(&plan.retained);
    }
    killed
}

/// 记录新会话(pty spawn 成功后调用)。注册表是崩溃清理的辅助面:
/// 任何失败只静默(最多吞一次未来清杀机会,不影响运行)。
pub fn track_session(child_pid: u32) {
    let Some(child_start) = query_creation(child_pid) else {
        return;
    };
    let mica_pid = std::process::id();
    let Some(mica_start) = query_creation(mica_pid) else {
        return;
    };
    Registry::new(Registry::default_path()).track(SessionRecord {
        mica_pid,
        mica_start,
        child_pid,
        child_start,
    });
}

/// 干净关闭单条会话时移除记录(kill_child 之后调用)。
pub fn untrack_session(child_pid: u32) {
    Registry::new(Registry::default_path()).untrack(child_pid);
}

/// 进程退出序:清掉本进程名下的全部记录。
pub fn clear_sessions_for_exit() {
    Registry::new(Registry::default_path()).clear_for(std::process::id());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(mica_pid: u32, mica_start: u64, child_pid: u32, child_start: u64) -> SessionRecord {
        SessionRecord {
            mica_pid,
            mica_start,
            child_pid,
            child_start,
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

    /// 干净世界:无记录无候选。
    #[test]
    fn empty_registry_kills_nothing() {
        let plan = plan_sweep(&[], &[proc(1, 0, "init", 100)]);
        assert!(plan.kill.is_empty());
        assert_eq!(plan.discarded, 0);
    }

    /// 核心:dry-run 执行器(NoopTerminator)只记录不执行——不得计成
    /// 任何成功终止;真进程未受影响由集成测试
    /// dry_run_leaves_real_processes_and_registry_untouched 在真机上证明。
    #[test]
    fn orphan_dry_run_never_terminates() {
        let records = [rec(100, 10, 200, 50)];
        let procs = [
            proc(1, 0, "init", 1),
            proc(200, 100, "powershell.exe", 50), // 我们的 shell,owner 死
            proc(300, 100, "conhost.exe", 55),    // 我们的宿主
        ];
        let plan = plan_sweep(&records, &procs);
        assert_eq!(plan.kill.len(), 2, "真实孤儿应有终止计划");
        let mut noop = NoopTerminator::default();
        let killed = execute_plan(&plan, &mut noop);
        assert_eq!(killed, 0, "dry-run 不得计为成功终止");
        assert_eq!(noop.calls.len(), plan.kill.len(), "记录面与计划一致");
    }

    /// owner 存活:不是孤儿,记录保留,零终止。
    #[test]
    fn orphan_unowned_process_is_ignored() {
        let records = [rec(100, 10, 200, 50)];
        let procs = [
            proc(100, 1, "mica.exe", 10), // owner 活着,创建时间一致
            proc(200, 100, "powershell.exe", 50),
        ];
        let plan = plan_sweep(&records, &procs);
        assert!(plan.kill.is_empty());
        assert_eq!(plan.retained, vec![records[0]]);
    }

    /// owner PID 存活但创建时间不同(PID 复用):老 owner 已死,按孤儿
    /// 处理,但复用者之后出生的宿主不动。
    #[test]
    fn owner_pid_reuse_treats_old_owner_dead_but_bounds_hosts() {
        let records = [rec(100, 10, 200, 50)];
        let procs = [
            proc(100, 1, "unrelated.exe", 500), // 复用者,晚于一切
            proc(200, 100, "powershell.exe", 50),
            proc(300, 100, "conhost.exe", 55), // 早于复用者:我们的宿主
            proc(400, 100, "conhost.exe", 600), // 晚于复用者:复用者的,不动
        ];
        let plan = plan_sweep(&records, &procs);
        let pids: Vec<u32> = plan.kill.iter().map(|k| k.pid).collect();
        assert!(pids.contains(&200), "shell 仍要终止");
        assert!(pids.contains(&300), "早于复用者的宿主要终止");
        assert!(!pids.contains(&400), "复用者的宿主绝不动");
    }

    /// child PID 被复用(创建时间不一致):那是别人的进程,绝不杀;记录
    /// 丢弃;孤儿宿主仍按 owner 路径清。
    #[test]
    fn child_pid_reuse_is_never_terminated() {
        let records = [rec(100, 10, 200, 50)];
        let procs = [
            proc(200, 999, "notepad.exe", 900), // PID 200 已是别人的
            proc(300, 100, "conhost.exe", 55),
        ];
        let plan = plan_sweep(&records, &procs);
        let pids: Vec<u32> = plan.kill.iter().map(|k| k.pid).collect();
        assert!(!pids.contains(&200), "被复用的 child 绝不终止");
        assert!(pids.contains(&300), "孤儿宿主仍要清");
        assert_eq!(plan.discarded, 1);
    }

    /// 同名无关进程:别人的 conhost(父不是死掉的 mica)不在候选内。
    #[test]
    fn unrelated_conhost_is_untouched() {
        let records = [rec(100, 10, 200, 50)];
        let procs = [
            proc(200, 100, "powershell.exe", 50),
            proc(310, 90, "conhost.exe", 42), // 别人的宿主,父 PID 不同
        ];
        let plan = plan_sweep(&records, &procs);
        let pids: Vec<u32> = plan.kill.iter().map(|k| k.pid).collect();
        assert!(!pids.contains(&310));
    }

    /// 宿主早于 owner 出生(父 PID 碰巧相同的历史进程):排除。
    #[test]
    fn host_older_than_owner_is_excluded() {
        let records = [rec(100, 500, 200, 550)];
        let procs = [
            proc(200, 100, "powershell.exe", 550),
            proc(320, 100, "conhost.exe", 100), // 早于 owner(500):不是它的
        ];
        let plan = plan_sweep(&records, &procs);
        let pids: Vec<u32> = plan.kill.iter().map(|k| k.pid).collect();
        assert!(!pids.contains(&320));
    }

    /// 快照为空(枚举失败):零终止是安全底线。sweep_owned 对空快照直接
    /// 拒绝计划(见其守卫);plan_sweep 按契约要求完整快照,这里锁住
    /// "即使误传空快照也不会产生终止"这一属性。
    #[test]
    fn enumeration_failure_is_conservative() {
        let records = [rec(100, 10, 200, 50)];
        let plan = plan_sweep(&records, &[]);
        assert!(plan.kill.is_empty(), "看不见任何进程时绝不动手");
    }

    /// owner 创建时间查询失败(None):无法证明已死,保守保留。
    #[test]
    fn owner_creation_unknown_retains() {
        let records = [rec(100, 10, 200, 50)];
        let mut owner = proc(100, 1, "mica.exe", 10);
        owner.created = None;
        let procs = [owner, proc(200, 100, "powershell.exe", 50)];
        let plan = plan_sweep(&records, &procs);
        assert!(plan.kill.is_empty());
        assert_eq!(plan.retained.len(), 1);
    }

    /// child 已死但宿主残留(2026-10-03 事故形态):宿主仍要清,记录丢弃。
    #[test]
    fn dead_child_with_live_host_still_sweeps_host() {
        let records = [rec(100, 10, 200, 50)];
        let procs = [proc(300, 100, "openconsole.exe", 55)];
        let plan = plan_sweep(&records, &procs);
        let pids: Vec<u32> = plan.kill.iter().map(|k| k.pid).collect();
        assert_eq!(pids, vec![300]);
        assert_eq!(plan.discarded, 1);
    }

    /// 多记录宿主去重:两条会话共用不了同一宿主,但防御性去重不死循环。
    #[test]
    fn host_candidates_dedup_across_records() {
        let records = [rec(100, 10, 200, 50), rec(100, 10, 201, 51)];
        let procs = [
            proc(200, 100, "powershell.exe", 50),
            proc(201, 100, "powershell.exe", 51),
            proc(300, 100, "conhost.exe", 55),
        ];
        let plan = plan_sweep(&records, &procs);
        let host_kills = plan.kill.iter().filter(|k| k.pid == 300).count();
        assert_eq!(host_kills, 1);
        assert_eq!(plan.kill.len(), 3);
    }

    /// 注册表往返:track → 读回;untrack → 消失;坏行跳过。
    #[test]
    fn registry_roundtrip_and_untrack() {
        let dir = std::env::temp_dir().join(format!(
            "mica-orphan-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = Registry::new(dir.join("sessions.txt"));
        let a = rec(1, 10, 2, 20);
        let b = rec(1, 10, 3, 30);
        reg.track(a);
        reg.track(b);
        let mut got = reg.records();
        got.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(got, want);
        reg.untrack(2);
        assert_eq!(reg.records(), vec![b]);
        // 坏行(字段不足/非数字/多余字段)跳过,好行保留
        let path = dir.join("sessions.txt");
        std::fs::write(&path, "garbage line\n1 10 3 30 extra\n4 5 6 7\n").unwrap();
        assert_eq!(reg.records(), vec![rec(4, 5, 6, 7)]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
