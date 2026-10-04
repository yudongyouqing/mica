//! 孤儿 ConPTY 宿主清理(M5c/T1,2026-10-03 事故票):强杀终端必留
//! OpenConsole 孤儿(taskkill /F 不走 Drop),管道断裂后 busy-loop 实测
//! 每个烧满一核。启动时按"父进程已死"清杀——真开着的终端(Windows
//! Terminal 等)父进程活着,不受影响;调用时机在自身 spawn pty 之前,
//! 零误杀窗口。

use std::collections::HashSet;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

/// 快照单遍收集:(pid, 父 pid, 进程名小写)。
fn snapshot() -> Vec<(u32, u32, String)> {
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
                out.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// 清杀父进程已死的 OpenConsole(ConPTY 宿主)。返回清杀数。
/// 传 `execute = false` 只数不杀(测试/观察用)。
pub fn sweep_orphaned_conhost(execute: bool) -> usize {
    let procs = snapshot();
    let alive: HashSet<u32> = procs.iter().map(|(pid, _, _)| *pid).collect();
    let mut killed = 0;
    // SAFETY: TerminateProcess 目标句柄由本侧 OpenProcess 获得
    unsafe {
        // 先收集孤儿 OpenConsole,先杀它们的子进程(powershell 等)——
        // 只杀 OpenConsole 会把 powershell 打成无宿主,defterm 立刻弹
        // WT/142("第二次启动报错"的清杀分支,1.0.4 实证);先杀子后杀
        // 父,子进程被我们 TerminateProcess 就不会再被 defterm 接管
        let orphan_conhost: Vec<u32> = procs
            .iter()
            .filter(|(_, parent, name)| name == "openconsole.exe" && !alive.contains(parent))
            .map(|(pid, _, _)| *pid)
            .collect();
        for (pid, parent, name) in &procs {
            if orphan_conhost.contains(parent)
                && name.ends_with(".exe")
                && *pid != *parent
                && let Ok(h) = windows::Win32::System::Threading::OpenProcess(
                    windows::Win32::System::Threading::PROCESS_TERMINATE,
                    false,
                    *pid,
                )
                && h != HANDLE::default()
            {
                let _ = windows::Win32::System::Threading::TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
        }
        for (pid, parent, name) in &procs {
            if name != "openconsole.exe" || alive.contains(parent) {
                continue;
            }
            // PID 复用竞态:快照与击杀之间父进程表可能变化——被清杀者的
            // 父已死是快照事实,击杀的是"当下仍叫 OpenConsole 的该 PID",
            // 复用窗口纳秒级,风险接受(WT 同类清理同样口径)
            if execute
                && let Ok(h) = windows::Win32::System::Threading::OpenProcess(
                    windows::Win32::System::Threading::PROCESS_TERMINATE,
                    false,
                    *pid,
                )
                && h != HANDLE::default()
            {
                if windows::Win32::System::Threading::TerminateProcess(h, 1).is_ok() {
                    killed += 1;
                }
                let _ = CloseHandle(h);
            } else if !execute {
                killed += 1;
            }
        }
    }
    killed
}
