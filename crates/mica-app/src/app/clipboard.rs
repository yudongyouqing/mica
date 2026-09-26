//! Win32 剪贴板薄封装:只做 CF_UNICODETEXT 的读写。
//! 失败全部静默降级(返回 None / bool)——剪贴板被占用是常态(其他
//! 进程持有),终端不能为此弹窗或卡输入路径。

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
/// CF_UNICODETEXT(=13;Ole feature 只为这个常量不值得开)
const CF_UNICODETEXT: u32 = 13;

use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

/// 打开剪贴板(重试 3 次,其他进程常短暂持有)。
fn with_clipboard<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    unsafe {
        for _ in 0..3 {
            if OpenClipboard(None).is_ok() {
                let result = f();
                let _ = CloseClipboard();
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        None
    }
}

/// 读剪贴板文本(空剪贴板/非文本格式 → None)。
pub fn get_text() -> Option<String> {
    with_clipboard(|| unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
        let ptr = GlobalLock(HGLOBAL(handle.0)) as *const u16;
        if ptr.is_null() {
            return None;
        }
        // SAFETY:CF_UNICODETEXT 数据保证 NUL 结尾;长度即走到 NUL
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        let text = String::from_utf16_lossy(slice);
        let _ = GlobalUnlock(HGLOBAL(handle.0));
        Some(text)
    })
}

/// 写剪贴板文本。
pub fn set_text(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    with_clipboard(|| unsafe {
        let handle =
            GlobalAlloc(GMEM_MOVEABLE, bytes).expect("clipboard GlobalAlloc(几百 KB 内)不失败");
        let ptr = GlobalLock(handle) as *mut u16;
        if ptr.is_null() {
            return None;
        }
        // SAFETY:handle 按 bytes 分配,wide 恰好 bytes/2 个 u16
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(handle);
        // SetClipboardData 接管句柄所有权;EmptyClipboard 先清旧格式
        EmptyClipboard().ok()?;
        SetClipboardData(CF_UNICODETEXT, Some(HANDLE(handle.0))).ok()?;
        Some(())
    })
    .is_some()
}

/// 粘贴归一:孤立 `\n` → `\r\n`(ConPTY 语义),已有 `\r\n` 不动。
pub fn normalize_paste(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => out.push_str("\r\n"),
            '\r' => {
                out.push('\r');
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    out.push('\n');
                }
            }
            c => out.push(c),
        }
    }
    out
}
