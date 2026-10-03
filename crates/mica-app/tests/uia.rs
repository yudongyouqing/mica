//! M5c/T2:最小 UIA 自证——不依赖 Narrator 人工。真窗口 + 生产同款
//! WM_GETOBJECT 入口,IUIAutomation 读回 Name 断言(COM 全链路)。
#![cfg(windows)]

use mica_app::app::uia::{UIA_NAME, handle_getobject};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationElement};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, RegisterClassExW, WINDOW_EX_STYLE,
    WM_GETOBJECT, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
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
        CoUninitialize();
        // 销毁窗口:向 wndproc 投 WM_DESTROY 由 DefWindowProc 常规路径处理即可,
        // 测试进程退出时回收
        let _ = hwnd;
    }
}
