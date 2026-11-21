//! M5c/T2:最小 UIA 自证——不依赖 Narrator 人工。真窗口 + 生产同款
//! WM_GETOBJECT 入口,IUIAutomation 读回 Name 断言(COM 全链路)。
#![cfg(windows)]

use mica_app::app::uia::{UIA_NAME, handle_getobject};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, TreeScope_Element, TreeScope_Subtree,
    UIA_TextPatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW,
    WINDOW_EX_STYLE, WM_GETOBJECT, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{PCWSTR, w};

unsafe extern "system" fn test_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_GETOBJECT
        && let Some(r) = handle_getobject(hwnd, wparam, lparam)
    {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[test]
fn uia_reads_window_name_end_to_end() {
    unsafe {
        let cls: PCWSTR = w!("mica_uia_test");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(test_wndproc),
            lpszClassName: cls,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            cls,
            w!("mica uia test"),
            WS_OVERLAPPEDWINDOW, // 不加 WS_VISIBLE:无需上屏
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            400,
            300,
            None,
            None,
            None,
            None,
        )
        .expect("test window");

        *UIA_NAME.lock().unwrap() = "Mica,3 个标签,PowerShell".into();

        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        assert!(hr.is_ok());
        let uia: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).expect("CUIAutomation");
        let element: IUIAutomationElement = uia.ElementFromHandle(hwnd).expect("ElementFromHandle");
        let name = element.CurrentName().expect("CurrentName");
        assert_eq!(
            name.to_string(),
            "Mica,3 个标签,PowerShell",
            "UIA Name 经真 COM 查询读回(等价 Narrator 读到的)"
        );
        // 窗口原生标题属性(Host provider 透传域)也不为空——透传活着的旁证
        let _ = element.CurrentProcessId().expect("host provider 透传属性");
        // 深度查询回归(1.0.8 崩溃复现路径):FindAll + GetCurrentPattern
        // 曾因 GetPatternProvider 返回 IUnknown::from_raw(null) 触发原生崩溃
        let true_cond = uia.CreateTrueCondition().expect("CreateTrueCondition");
        let found_el = element
            .FindAll(TreeScope_Element, &true_cond)
            .expect("FindAll(TreeScope_Element)");
        assert!(
            found_el.Length().expect("Length") >= 1,
            "FindAll(TreeScope_Element) 应至少返回窗口元素自身"
        );
        let found_sub = element
            .FindAll(TreeScope_Subtree, &true_cond)
            .expect("FindAll(TreeScope_Subtree)");
        assert!(
            found_sub.Length().expect("Length") >= 1,
            "FindAll(TreeScope_Subtree) 应正常完成且不崩溃"
        );
        // 未实现的 pattern 应优雅返回 Err/空对象,而非 UB 崩溃
        let _ = element.GetCurrentPattern(UIA_TextPatternId);
        let _ = element.GetCurrentPattern(UIA_ValuePatternId);
        CoUninitialize();
        let _ = DestroyWindow(hwnd);
    }
}
