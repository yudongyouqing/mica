//! Jump list(M3a,spec §5):任务栏右键直达 profile。
//!
//! 最佳努力:任何 COM 失败只记日志,绝不影响主流程(jump list 是锦上添花)。
//! windows-rs 无 CLSID 常量,三个 GUID 按 SDK 文档值手写(注释留源)。

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::{ICustomDestinationList, IShellLinkW};
use windows::core::{GUID, Interface, PCWSTR};

/// CLSID_DestinationList(SDK shobjidl_core.h:86F021DE-D0DD-4FAE-8A99-762F7139C29C)
const CLSID_DESTINATION_LIST: GUID = GUID::from_values(
    0x86f0_21de,
    0xd0dd,
    0x4fae,
    [0x8a, 0x99, 0x76, 0x2f, 0x71, 0x39, 0xc2, 0x9c],
);
/// CLSID_ShellLink(SDK shobjidl_core.h:00021401-0000-0000-C000-000000000046)
const CLSID_SHELL_LINK_INPROC: GUID = GUID::from_values(
    0x0002_1401,
    0x0000,
    0x0000,
    [0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);
/// CLSID_EnumerableObjectCreation(SDK shobjidl_core.h:2D3468C1-36A7-43B6-AC24-D3F02FD9607A)
const CLSID_ENUMERABLE_OBJECT_CREATION: GUID = GUID::from_values(
    0x2d34_68c1,
    0x36a7,
    0x43b6,
    [0xac, 0x24, 0xd3, 0xf0, 0x2f, 0xd9, 0x60, 0x7a],
);

/// AUMID(与 SetCurrentProcessExplicitAppUserModelID 同源;jump list 按 ID 挂)。
pub const AUMID_W: PCWSTR = windows::core::w!("Mica.Terminal");

/// 进程级 AUMID(任务栏分组/jump list 挂靠点;GUI 启动最早调用)。
pub fn set_appuser_model_id() {
    // SAFETY: 常量串,进程级一次性
    unsafe {
        if let Err(e) = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(AUMID_W)
        {
            eprintln!("jumplist: AppUserModelID 设置失败 {e}");
        }
    }
}

/// 注册任务栏 jump list:每 profile 一项(参数 = `new-tab <name>`)。
pub fn install(exe_path: &str, profiles: &[(String, String)]) {
    // SAFETY: 全块 COM;结构非 Copy 按引用借
    unsafe {
        if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
            eprintln!("jumplist: CoInitializeEx 失败,跳过");
            return;
        }
        let Ok(list) = CoCreateInstance::<_, ICustomDestinationList>(
            &CLSID_DESTINATION_LIST,
            None,
            CLSCTX_INPROC_SERVER,
        ) else {
            eprintln!("jumplist: DestinationList 实例化失败");
            return;
        };
        let mut slots = 0u32;
        let Ok(_usage) = list.BeginList::<windows::core::IUnknown>(&mut slots) else {
            eprintln!("jumplist: BeginList 失败");
            return;
        };
        // IObjectCollection(IObjectArray 的可写视图)承接任务列表
        let Ok(collection) = CoCreateInstance::<_, IObjectCollection>(
            &CLSID_ENUMERABLE_OBJECT_CREATION,
            None,
            CLSCTX_INPROC_SERVER,
        ) else {
            eprintln!("jumplist: ObjectCollection 实例化失败");
            return;
        };
        for (name, _command) in profiles {
            if let Some(link) = make_link(exe_path, name)
                && let Err(e) = collection.AddObject(&link)
            {
                eprintln!("jumplist: AddObject({name}) 失败 {e}");
            }
        }
        // collection → array 同 IID 树(cast 失败说明系统接口异变,记日志跳过)
        let Ok(array) = collection.cast::<IObjectArray>() else {
            eprintln!("jumplist: IObjectCollection → IObjectArray cast 失败");
            return;
        };
        if let Err(e) = list.AddUserTasks(&array) {
            eprintln!("jumplist: AddUserTasks 失败 {e}");
        }
        if let Err(e) = list.CommitList() {
            eprintln!("jumplist: CommitList 失败 {e}");
        }
    }
}

/// 单条任务:IShellLinkW(exe + `new-tab <profile>` + 显示名)。
unsafe fn make_link(exe_path: &str, profile: &str) -> Option<IShellLinkW> {
    let link: IShellLinkW =
        CoCreateInstance(&CLSID_SHELL_LINK_INPROC, None, CLSCTX_INPROC_SERVER).ok()?;
    let path: windows::core::HSTRING = exe_path.into();
    link.SetPath(PCWSTR(path.as_ptr())).ok()?;
    let args: windows::core::HSTRING = format!("new-tab {profile}").into();
    link.SetArguments(PCWSTR(args.as_ptr())).ok()?;
    let desc: windows::core::HSTRING = format!("Open a {profile} tab").into();
    link.SetDescription(PCWSTR(desc.as_ptr())).ok()?;
    Some(link)
}
