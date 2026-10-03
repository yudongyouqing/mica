//! 崩溃档案(M5b/T1,spec §8):panic hook + 本地 minidump,**永不上传**
//! (Ghostty 哲学)。dmp 落 `%LOCALAPPDATA%\mica\crashes\`,panic 详情落
//! 同基名 .txt,GUI 线程弹 MessageBoxW 指路径。dmp+exe+pdb 同存方可解,
//! README 发版说明写明(release 归档 pdb)。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE,
};
use windows::Win32::System::Diagnostics::Debug::{
    MiniDumpNormal, MiniDumpWithDataSegs, MiniDumpWriteDump,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};
use windows::core::HSTRING;

/// 递归护栏:hook 内再 panic(MessageBox/IO 失败等)直接落到 stderr,不再进 hook。
static IN_HOOK: AtomicBool = AtomicBool::new(false);

/// 崩溃档案目录。LOCALAPPDATA 缺失(服务上下文等)退化到 exe 旁 crashes/。
pub fn crash_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("mica").join("crashes"))
        .unwrap_or_else(|| PathBuf::from("crashes"))
}

/// 安装 panic hook。进程生命期一次(重复调用静默替换——set_hook 语义)。
pub fn install() {
    std::panic::set_hook(Box::new(|info| {
        if IN_HOOK.swap(true, Ordering::SeqCst) {
            eprintln!("[crash] nested panic: {info}");
            return;
        }
        let dir = crash_dir();
        let _ = std::fs::create_dir_all(&dir);
        let base = dir.join(format!(
            "mica-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        // 详情先行(dmp 失败至少有 panic 现场)
        let _ = std::fs::write(base.with_extension("txt"), format!("{info}\n"));
        let dumped = write_minidump(&base.with_extension("dmp"));
        let dump_note = if dumped {
            ",minidump .dmp"
        } else {
            "(minidump 失败)"
        };
        eprintln!(
            "[crash] panic: {info}\n[crash] 详情 {}.txt{}",
            base.display(),
            dump_note
        );
        let msg = format!(
            "Mica 崩溃了:\n{info}\n\n崩溃档案已存:\n{}.txt{}",
            base.display(),
            if dumped { " / .dmp" } else { "" }
        );
        // 弹窗抑制(CI/自动化:MICA_CRASH_NOPROMPT=1 时只留档案不弹模态框
        // ——集成测试 spawn 场景,无人点框会挂死)
        if std::env::var_os("MICA_CRASH_NOPROMPT").is_some_and(|v| v == "1") {
            return;
        }
        // SAFETY: OS 消息框;panic 线程无消息权时失败静默(stderr 已有档案路径)
        unsafe {
            let wide = HSTRING::from(msg);
            let _ = windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                None,
                windows::core::PCWSTR(wide.as_ptr()),
                windows::core::w!("Mica crash"),
                windows::Win32::UI::WindowsAndMessaging::MB_ICONWARNING,
            );
        }
        IN_HOOK.store(false, Ordering::SeqCst);
    }));
}

/// MiniDumpWriteDump 包装:Normal + WithDataSegs(局部变量值,排障刚需;
/// 体积 ~几 MB 级,不到 Full 的膨胀量)。
fn write_minidump(path: &std::path::Path) -> bool {
    // SAFETY: 进程/文件句柄由本侧创建持有;指针参数全 None(无异常上下文
    // ——panic 非 SEH,dump 由 hook 时点活栈也足以还原)
    unsafe {
        let name = HSTRING::from(path.as_os_str());
        let Ok(h) = CreateFileW(
            &name,
            GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) else {
            return false;
        };
        let ok = MiniDumpWriteDump(
            GetCurrentProcess(),
            GetCurrentProcessId(),
            h,
            MiniDumpNormal | MiniDumpWithDataSegs,
            None,
            None,
            None,
        )
        .is_ok();
        let _ = CloseHandle(h);
        ok
    }
}
