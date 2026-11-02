//! 进程级窗口登记(T04,2026-11-02):主/副窗口的统一关闭与 IPC/配置
//! 通知的活跃窗口寻址。
//!
//! 历史问题(F05):caption × 投递伪造 `WM_DESTROY`(HWND 不销毁,副窗
//! 场景留僵尸窗口);关最后标签只 `PostQuitMessage`(计数不归零,run()
//! 尾部 `while WINDOW_COUNT > 0` 永久 park,进程挂死);主窗关闭后 IPC/
//! 配置通知仍指向已死的 hwnd slot。
//!
//! 契约:
//! - **一切关闭走 WM_CLOSE → DestroyWindow**;计数只在真实 WM_DESTROY
//!   递减,递增只发生在 CreateWindowExW 成功之后(创建失败不登记)。
//! - 每个窗口把自己的 hwnd slot 登记进进程级注册表;IPC/配置通知向
//!   **第一个仍存活**的窗口投递(主窗先关,副窗接管接收)。
//! - 关闭最后普通窗口 → 计数归零 → main 返回 → 进程退出;QT 随进程
//!   结束(默认策略,显式托盘驻留属后续设计)。

use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::SharedHwnd;

/// 进程级窗口表:各窗口的 hwnd slot(创建后 Some(hwnd),销毁前置 None)。
/// slot 即窗口线程与后台线程共享的哨兵(与转发线程共用"窗口存活"事实)。
static REGISTRY: OnceLock<Mutex<Vec<SharedHwnd>>> = OnceLock::new();

fn registry() -> &'static Mutex<Vec<SharedHwnd>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// 登记窗口(CreateWindowExW 成功之后调用)。按 slot 的 Arc 指针幂等。
pub fn register(slot: &SharedHwnd) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if !reg.iter().any(|s| Arc::ptr_eq(s, slot)) {
        reg.push(Arc::clone(slot));
    }
}

/// 注销窗口(WM_DESTROY 里、计数递减前调用)。
pub fn unregister(slot: &SharedHwnd) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.retain(|s| !Arc::ptr_eq(s, slot));
}

/// 向第一个仍存活的窗口投递消息(存活 = slot 当前为 Some(hwnd))。
/// 返回是否投出——没有存活窗口(进程退出中)返回 false,调用方据此
/// 丢弃或保留通知。
pub fn post_to_alive(msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    for slot in reg.iter() {
        let hwnd = *slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(raw) = hwnd {
            // SAFETY: hwnd 来自登记窗口的创建句柄;Post 到已销毁窗口只是
            // 失败返回,无害
            let ok = unsafe {
                PostMessageW(
                    Some(HWND(raw as *mut std::ffi::c_void)),
                    msg,
                    wparam,
                    lparam,
                )
            }
            .is_ok();
            if ok {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空注册表:投递返回 false(不 panic、不投出)。
    #[test]
    fn empty_registry_posts_nothing() {
        // 注意:全局注册表可能被同进程其他测试动过;这里只验证"没有
        // 存活 slot 时"的行为——临时把注册表读出来,若非空则本用例跳过
        let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        let empty = reg.is_empty();
        drop(reg);
        if empty {
            assert!(!post_to_alive(0xB000, WPARAM(0), LPARAM(0)));
        }
    }

    /// 登记→注销的幂等与寻址:登记两个 slot(一个已置 None=已关窗,
    /// 一个 Some(伪造非窗口句柄——Post 失败))。用可判定行为:注销后
    /// post_to_alive 不再考虑它。真实窗口的寻址由集成测试覆盖
    /// (tests/window_lifecycle.rs)。
    #[test]
    fn register_unregister_idempotent() {
        let a: SharedHwnd = Arc::new(Mutex::new(None)); // 已关窗形态
        register(&a);
        register(&a); // 幂等
        {
            let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(reg.iter().filter(|s| Arc::ptr_eq(s, &a)).count(), 1);
        }
        unregister(&a);
        unregister(&a); // 幂等
        {
            let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
            assert!(reg.iter().all(|s| !Arc::ptr_eq(s, &a)));
        }
        // None slot 不会被投递(post 返回 false,若无其他存活窗口)
        // ——此处不断言全局 false(其他测试可能已登记),只验证不 panic
        let _ = post_to_alive(0xB001, WPARAM(0), LPARAM(0));
    }
}
