//! 最小 UIA(M5c/T2,D33):自定义 IRawElementProviderSimple 只答
//! Name(窗口语义摘要),矩形/默认属性经 Host provider(UiaHostProviderFromHwnd)
//! 透传白给——"Narrator 能读出窗口语义"即收,完整 provider 全家是 1.1 票。
//! Name 走进程级 Mutex 快照(draw_frame 更新,UIA 工作线程读;TLS 会看错线程)。

use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BSTR};
use windows::Win32::UI::Accessibility::{
    IRawElementProviderSimple, IRawElementProviderSimple_Impl, ProviderOptions,
    ProviderOptions_ServerSideProvider, UIA_NamePropertyId, UIA_PATTERN_ID, UIA_PROPERTY_ID,
    UiaHostProviderFromHwnd, UiaReturnRawElementProvider,
};
use windows::core::{IUnknown, Interface, Result, implement};

/// UIA Name 的进程级快照:draw_frame 每帧刷新(N 个标签,活跃标题)。
pub static UIA_NAME: Mutex<String> = Mutex::new(String::new());

/// BSTR 变体手工构造(windows 0.62 的 VARIANT 无 From 便捷)。
fn bstr_variant(s: &str) -> VARIANT {
    let b = windows::core::BSTR::from(s);
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: core::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    bstrVal: core::mem::ManuallyDrop::new(b),
                },
            }),
        },
    }
}

#[implement(IRawElementProviderSimple)]
pub struct MicaProvider {
    hwnd: HWND,
}

impl IRawElementProviderSimple_Impl for MicaProvider_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(&self, _pattern: UIA_PATTERN_ID) -> Result<IUnknown> {
        // null = 无 pattern 支持;最小范围即收(D33)
        Ok(unsafe { IUnknown::from_raw(std::ptr::null_mut()) })
    }

    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        #[cfg(test)]
        if std::env::var("UIA_PROBE").is_ok() {
            eprintln!("[uia-probe] GetPropertyValue({})", id.0);
        }
        if id == UIA_NamePropertyId {
            let name = UIA_NAME.lock().map(|s| s.clone()).unwrap_or_default();
            Ok(bstr_variant(&name))
        } else {
            Ok(VARIANT::default()) // VT_EMPTY:其余属性交 Host provider 兜
        }
    }

    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        // SAFETY: 只读 hwnd 字段;返回带引用计数的 host provider
        unsafe { UiaHostProviderFromHwnd(self.hwnd) }
    }
}

/// WM_GETOBJECT 处理(生产 wndproc 与集成测试共用同一入口):
/// UIA 根对象请求返回 MicaProvider,其余落调用方默认。
pub fn handle_getobject(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    // 根对象请求 lParam == UiaRootObjectId(= -25 / 0xffff'ffe7,
    // UIAutomationCore 头文件实证)。判据走错(OBJID_CLIENT/0 都试过)
    // provider 挂不上——UIA 只读 host 域的窗口标题,集成测试两轮打回
    const UIA_ROOT_OBJECT_ID: i32 = -25;
    if lparam.0 as i32 != UIA_ROOT_OBJECT_ID {
        return None;
    }
    let provider: IRawElementProviderSimple = MicaProvider { hwnd }.into();
    // SAFETY: UiaReturnRawElementProvider 内部 AddRef,provider 本地副本
    // 出作用域释放安全
    Some(unsafe { UiaReturnRawElementProvider(hwnd, wparam, lparam, &provider) })
}
