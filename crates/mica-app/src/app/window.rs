//! Window creation, geometry/placement, DWM chrome, opacity, startup bootstrap,
//! deferred initialization, and the Win32 message loop.
#![allow(unsafe_op_in_unsafe_fn)]

use super::*;
use std::sync::mpsc;
use windows::Win32::Foundation::POINT;

// WM_NCCALCSIZE makes the entire window the client area. Account for our
// own strip exactly once; round up so a fractional cell never loses a column.
pub(super) fn initial_window_rect(settings: &Settings, metrics: &FontMetrics) -> RECT {
    if let Some((width, height)) = settings.window_size_pixels {
        return RECT {
            right: i32::from(width),
            bottom: i32::from(height),
            ..Default::default()
        };
    }
    let strip = mica_render::frame::StripMetrics::from_scale_cfg(
        DPI_SCALE.with(std::cell::Cell::get),
        &settings.strip,
    );
    let strip_h = if strip.hide_single {
        strip.title_h
    } else {
        strip.h
    };
    let scale = DPI_SCALE.with(std::cell::Cell::get);
    RECT {
        right: (f32::from(settings.window_width) * metrics.cell_width
            + 2.0 * settings.window_padding_x * scale)
            .ceil() as i32,
        bottom: (f32::from(settings.window_height) * metrics.line_height
            + 2.0 * settings.window_padding_y * scale)
            .ceil() as i32
            + strip_h.ceil() as i32,
        ..Default::default()
    }
}

/// 纯尺寸规则:请求外框约束到**已成功读取的正尺寸工作区**。
/// monitor 查询失败不进这里(fit 前值已由调用方保证为正),
/// 也不表达位置——位置约束见 [`constrained_window_placement`]。
pub(super) fn fit_outer_size(request: (i32, i32), work: (i32, i32)) -> (i32, i32) {
    (request.0.min(work.0).max(1), request.1.min(work.1).max(1))
}

/// 窗口所在显示器工作区(物理像素,窗口本身 PMv2):只认正尺寸矩形,
/// 查询失败/退化矩形返回 None——由调用方保留请求原值,不把失败当 1×1 窗口
fn window_work_area(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    // SAFETY: 只读查询
    unsafe {
        let mon = windows::Win32::Graphics::Gdi::MonitorFromWindow(
            hwnd,
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
        );
        let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
            cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
            ..Default::default()
        };
        if windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi) == false {
            return None;
        }
        let w = mi.rcWork;
        (w.right > w.left && w.bottom > w.top).then_some((w.left, w.top, w.right, w.bottom))
    }
}

/// 落位约束:初始外框请求按所在显示器工作区收敛,`initial_window_rect`
/// 仍只表达配置(不碰 Settings/用户文件)。
/// - 尺寸:自己按工作区裁,而不是交给 OS 默认 max track——小屏 runner
///   上后者把 2032×1041 压成屏幕+边距(1044×788 实证),行为不可述;
/// - 位置:自动落位(光标屏 +60 偏移)在缩窗后可能越出工作区,起点一并
///   收回;显式 window-position 信任用户坐标(多屏自由摆,负坐标合法)。
///
/// monitor 查询失败:eprintln 诊断并保留请求(尺寸不缩、位置取当前
/// 矩形),既有 OS 兜底仍在——失败不是 1×1 窗口。
pub(super) fn constrained_window_placement(hwnd: HWND, request: RECT, auto_position: bool) -> RECT {
    // SAFETY: 只读几何查询;SetWindowPos 由调用方执行
    unsafe {
        let mut current = RECT::default();
        if GetWindowRect(hwnd, &mut current).is_err() {
            eprintln!(
                "[window] GetWindowRect 失败,保留请求外框 {}x{}(原点 0,0)",
                request.right - request.left,
                request.bottom - request.top
            );
            return request;
        }
        let mut placed = RECT {
            left: current.left,
            top: current.top,
            right: current.left + request.right - request.left,
            bottom: current.top + request.bottom - request.top,
        };
        let Some((wl, wt, wr, wb)) = window_work_area(hwnd) else {
            eprintln!(
                "[window] 工作区查询失败,保留请求外框 {}x{}(OS max track 兜底)",
                placed.right - placed.left,
                placed.bottom - placed.top
            );
            return placed;
        };
        let (w, h) = fit_outer_size(
            (placed.right - placed.left, placed.bottom - placed.top),
            (wr - wl, wb - wt),
        );
        placed.right = placed.left + w;
        placed.bottom = placed.top + h;
        if auto_position {
            // 缩窗后起点收回工作区:上压右/下边界,下托左/上边界
            placed.left = placed.left.min(wr - w).max(wl);
            placed.top = placed.top.min(wb - h).max(wt);
            placed.right = placed.left + w;
            placed.bottom = placed.top + h;
        }
        placed
    }
}

/// resize 拖拽边宽(逻辑 6px 随 DPI)
pub(super) fn resize_border() -> i32 {
    (6.0 * DPI_SCALE.with(std::cell::Cell::get)) as i32
}

/// 光标所在显示器工作区的左上偏移(#41 落位锚):混合 DPI 多屏下
/// CW_USEDEFAULT 可能落在边界另一侧,大窗口 majority 跨屏被系统重派
/// → WM_DPICHANGED 按建议矩形缩水;锚定光标屏创建则全程同屏。
/// 失败(极小概率 API 挂)回落 CW_USEDEFAULT 语义(返回原值)
fn cursor_workarea_origin() -> (i32, i32) {
    // SAFETY: 只读查询;失败走 CW_USEDEFAULT 兜底
    unsafe {
        let mut pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
        if windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt).is_err() {
            return (CW_USEDEFAULT, CW_USEDEFAULT);
        }
        let mon = windows::Win32::Graphics::Gdi::MonitorFromPoint(
            pt,
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
        );
        let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
            cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
            ..Default::default()
        };
        if windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi) == false {
            return (CW_USEDEFAULT, CW_USEDEFAULT);
        }
        (mi.rcWork.left + 60, mi.rcWork.top + 60)
    }
}

/// #78 整窗透明(WS_EX_LAYERED + LWA_ALPHA):true = 透明档(DIM),
/// false = 不透明(摘 LAYERED,回到普通合成)。渲染管线零改动——
/// DWM 在合成层打折;文字同步半透明是 v1 接受的差异(per-pixel
/// composition swapchain 留后续增强)
pub(super) fn apply_window_alpha(hwnd: HWND, translucent: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, LWA_ALPHA, SetLayeredWindowAttributes, SetWindowLongPtrW,
        WS_EX_LAYERED,
    };
    WINDOW_TRANSLUCENT.with(|t| t.set(translucent));
    // SAFETY: 样式位与分层属性读写;失败静默(老系统降级不透明)
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as usize;
        if translucent {
            let alpha = WINDOW_ALPHA_DIM.with(std::cell::Cell::get);
            if style & WS_EX_LAYERED.0 as usize == 0 {
                let _ = SetWindowLongPtrW(
                    hwnd,
                    GWL_EXSTYLE,
                    (style | WS_EX_LAYERED.0 as usize) as isize,
                );
            }
            let _ = SetLayeredWindowAttributes(
                hwnd,
                windows::Win32::Foundation::COLORREF(0),
                alpha,
                LWA_ALPHA,
            );
        } else if style & WS_EX_LAYERED.0 as usize != 0 {
            // 摘位前先恢复全不透明,避免窗口僵在半透明
            let _ = SetLayeredWindowAttributes(
                hwnd,
                windows::Win32::Foundation::COLORREF(0),
                255,
                LWA_ALPHA,
            );
            let _ = SetWindowLongPtrW(
                hwnd,
                GWL_EXSTYLE,
                (style & !(WS_EX_LAYERED.0 as usize)) as isize,
            );
        }
    }
}

/// 将当前配置的背景图与混合强度同步到指定 GPU 渲染器(启动/副窗/设备重建共用)。
pub(super) fn sync_background_image(renderer: &mut Renderer) {
    let path = BG_IMG_PATH.with(|b| b.borrow().clone());
    let alpha = BG_IMG_ALPHA.with(std::cell::Cell::get);
    if let Err(e) = renderer.set_background_image(path.as_deref().map(std::path::Path::new), alpha)
    {
        eprintln!("[bg] {e}");
    }
}

/// 将 Settings 播种到当前窗口线程的所有 TLS 槽(主窗/副窗共享单源)。
/// 返回是否以半透明态启动(`background-opacity != 1.0`)。
fn seed_window_settings(settings: &Settings) -> bool {
    KEYMAP.with(|k| *k.borrow_mut() = settings.keymap.clone());
    TITLE_LOCK.with(|t| *t.borrow_mut() = settings.title.clone());
    MOUSE_HIDE_ON_TYPE.with(|m| m.set(settings.mouse_hide_while_typing));
    COPY_ON_SELECT.with(|c| c.set(settings.copy_on_select));
    RIGHT_CLICK_PASTE.with(|r| r.set(settings.mouse_right_click == MouseRightClick::Paste));
    KEY_REMAP.with(|k| *k.borrow_mut() = settings.key_remap.clone());
    let dim = if (settings.background_opacity - 1.0).abs() < f32::EPSILON {
        217 // 未配置:0.85 默认档
    } else {
        (settings.background_opacity.clamp(0.05, 1.0) * 255.0) as u8
    };
    let bg_alpha = if (settings.background_opacity - 1.0).abs() < f32::EPSILON {
        0.9
    } else {
        settings.background_opacity
    };
    BG_IMG_PATH.with(|b| *b.borrow_mut() = settings.background_image.clone());
    BG_IMG_ALPHA.with(|a| a.set(bg_alpha));
    WINDOW_ALPHA_DIM.with(|a| a.set(dim));
    let start_translucent = (settings.background_opacity - 1.0).abs() >= f32::EPSILON;
    WINDOW_TRANSLUCENT.with(|t| t.set(start_translucent));
    // 光标闪烁(D20):默认不闪;cursor-blink = true 时窗口创建后开定时器
    CURSOR_BLINK.with(|b| b.set(settings.cursor_blink));
    TAB_SEED.with(|s| *s.borrow_mut() = (settings.font_families.clone(), settings.font_size_pt));
    SESSION_COMMAND.with(|s| *s.borrow_mut() = settings.command.clone());
    CONTENT_PADDING
        .with(|p| *p.borrow_mut() = (settings.window_padding_x, settings.window_padding_y));
    UNFOCUSED_SPLIT_OPACITY.with(|opacity| opacity.set(settings.unfocused_split_opacity));
    QUICK_TERMINAL_HEIGHT.with(|h| h.set(settings.quick_terminal_height));
    QUICK_TERMINAL_KEY.with(|k| *k.borrow_mut() = settings.quick_terminal_key.clone());
    STRIP_CFG.with(|c| *c.borrow_mut() = settings.strip.clone());
    CURRENT_PALETTE.with(|p| *p.borrow_mut() = settings.palette);
    start_translucent
}

pub(super) fn apply_theme_chrome(hwnd: HWND, palette: &Palette) {
    let bg = palette.bg;
    let dark = i32::from(mica_render::color::relative_luminance([bg.r, bg.g, bg.b]) < 0.5);
    // SAFETY: DWM copies this window attribute value during the call.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            std::ptr::from_ref(&dark).cast(),
            std::mem::size_of_val(&dark) as u32,
        );
    }
}

/// Fold the non-client area and apply the DWM border and corner preferences.
unsafe fn configure_window_chrome(hwnd: HWND, palette: &Palette) {
    // 创建即折叠原生边框(Win11 实测):创建路径里 NCCALCSIZE 先按默认
    // 计算落框,折叠(返回 0)仅在 FRAMECHANGED 强制重算时应用——不补
    // 这一发,窗口顶着原生标题栏+边框运行,与 strip 自绘的 caption
    // 形成双 chrome(用户实测 offset X=8/Y=30,2026-10-06 UI 对标首日)
    let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
        hwnd,
        None,
        0,
        0,
        0,
        0,
        windows::Win32::UI::WindowsAndMessaging::SWP_FRAMECHANGED
            | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
            | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE
            | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
            | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
    );

    apply_theme_chrome(hwnd, palette);
    // 外框描边移除(2026-10-07 用户裁定:不要任何描边)——Win11 默认
    // 给窗口一圈 1px 边线,DWMWA_COLOR_NONE 关掉
    let no_border: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE
    let _ = DwmSetWindowAttribute(
        hwnd,
        windows::Win32::Graphics::Dwm::DWMWA_BORDER_COLOR,
        &no_border as *const u32 as *const std::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    );
    // 圆角(Win11 DWM):NCCALCSIZE 自绘帧下默认策略不保证圆角,显式
    // 声明 ROUND(最大化时 DWM 自动去圆,无需处理)
    let corner: i32 = windows::Win32::Graphics::Dwm::DWMWCP_ROUND.0;
    let _ = DwmSetWindowAttribute(
        hwnd,
        windows::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE,
        &corner as *const i32 as *const std::ffi::c_void,
        std::mem::size_of::<i32>() as u32,
    );
}

/// 创建窗口、折叠边框/DWM 视觉、播种首屏 DPI 并按工作区约束落位。
/// 字体度量初始化失败时弹窗并销毁窗口,返回 None。
unsafe fn create_and_place_window(
    hinstance: HINSTANCE,
    settings: &Settings,
    is_primary: bool,
    drop_at: Option<POINT>,
) -> Option<HWND> {
    // 窗口先建(哑尺寸):DPI 播种需要 hwnd,而字体 router 的 pt 又要
    // 乘 DPI_SCALE;精确网格尺寸在播种后 SetWindowPos 定。
    // 落位优先级(#89):主窗 window-position 配置 > 光标屏锚定(#41),副窗恒光标屏。
    let (wx, wy) = if let Some(pt) = drop_at {
        (pt.x - 40, pt.y - 16)
    } else if is_primary {
        settings
            .window_position
            .unwrap_or_else(cursor_workarea_origin)
    } else {
        cursor_workarea_origin()
    };
    let title = HSTRING::from("Mica");
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("mica_app_class"),
        &title,
        WS_OVERLAPPEDWINDOW,
        wx,
        wy,
        900,
        560,
        None,
        None,
        Some(hinstance),
        None,
    )
    .expect(if is_primary {
        "CreateWindowExW failed"
    } else {
        "CreateWindowExW (second window)"
    });

    configure_window_chrome(hwnd, &settings.palette);

    // 首屏 DPI 播种(M5b/T4):窗口落在高 DPI 屏时,首建即正确字号
    {
        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd);
        if dpi != 96 {
            if is_primary {
                eprintln!(
                    "[dpi] 启动于 {dpi} DPI 屏(初始即按 x{:.2} 缩放)",
                    dpi as f32 / 96.0
                );
            }
            DPI_SCALE.with(|d| d.set(dpi as f32 / 96.0));
        }
    }

    // 精确网格尺寸(此刻 DPI 已播、字体可按屏缩放):COLS×ROWS 反推
    // 外框并落位。度量 router 用后即弃——首标签在 start_tab_with_profile
    // 里按 TAB_SEED/scaled_pt 另建,度量同源一致
    {
        let families: Vec<&str> = if settings.font_families.is_empty() {
            DEFAULT_FAMILIES.to_vec()
        } else {
            settings.font_families.iter().map(String::as_str).collect()
        };
        let router = match DwriteRouter::new(scaled_pt(settings.font_size_pt), &families) {
            Ok(router) => router,
            Err(error) => {
                warn_box(&format!("Font/atlas resource unavailable: {error}"));
                // WM_DESTROY decrements the count, even before registration.
                WINDOW_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
                return None;
            }
        };
        let metrics = router.metrics();
        if is_primary {
            eprintln!("font families in use: {:?}", router.families_in_use());
            eprintln!(
                "[dpi] 初始网格 cw={} lh={}(pt={} ×{})",
                metrics.cell_width,
                metrics.line_height,
                settings.font_size_pt,
                DPI_SCALE.with(std::cell::Cell::get)
            );
        }
        let rect = initial_window_rect(settings, &metrics);
        // 请求外框按所在屏工作区收敛;主窗显式 window-position 信任用户坐标,
        // 自动落位与副窗在缩窗后收回工作区内
        let auto_position = !is_primary || settings.window_position.is_none();
        let rect = constrained_window_placement(hwnd, rect, auto_position);
        let mut swp_flags = windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER;
        if is_primary {
            swp_flags |= windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE;
        }
        let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
            hwnd,
            None,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            swp_flags,
        );
        if is_primary {
            eprintln!(
                "[dpi] 落位窗口 {}x{}",
                rect.right - rect.left,
                rect.bottom - rect.top
            );
        }
    }

    Some(hwnd)
}

/// 主窗/副窗共用的完整窗口初始化链:TLS 播种 → 创窗落位 → 定时器 →
/// 进程级注册 → GPU → (主窗 IPC) → 首标签 → 背景/透明度 → 首帧显窗。
unsafe fn bootstrap_window(
    hinstance: HINSTANCE,
    settings: &Settings,
    is_primary: bool,
    drop_at: Option<POINT>,
    t0: Option<std::time::Instant>,
) -> Option<(HWND, SharedHwnd)> {
    let start_translucent = seed_window_settings(settings);
    let hwnd = create_and_place_window(hinstance, settings, is_primary, drop_at)?;

    if CURSOR_BLINK.with(std::cell::Cell::get) {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 1, 500, None);
    }
    // 子进程退出轮询(M5b/T3):1s 粒度,独立于光标闪烁(默认不闪也要探退出)
    let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 3, 1000, None);

    // T5/T7:后台线程共用的窗口哨兵,窗口创建后建立(两个线程都要投递)
    let hwnd_slot: SharedHwnd = Arc::new(Mutex::new(Some(hwnd.0 as isize)));
    SHARED_HWND.with(|s| *s.borrow_mut() = Some(Arc::clone(&hwnd_slot)));
    window_registry::register(&hwnd_slot); // T04:进程级窗口表
    WINDOW_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let gpu = match init_window_gpu(hwnd) {
        Ok(gpu) => gpu,
        Err(error) => {
            warn_box(&format!("GPU atlas resource unavailable: {error}"));
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            return None;
        }
    };
    GPU.with(|g| *g.borrow_mut() = Some(gpu));

    if is_primary {
        // M3a:单实例 IPC 服务(管道被占说明已有实例——main 侧已分流,这里
        // 只可能首实例到达;失败降级单开不致命)。必须在首帧前:晚于此,
        // 启动窗口期内的 `mica new-tab` 会自启第二个实例
        match ipc::serve() {
            Ok(rx) => *IPC_RX.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx),
            Err(false) => {} // 建管道失败:已在 ipc::serve 记日志
            Err(true) => unreachable!("main 已分流,GUI 路径不会撞已占管道"),
        }
    }

    if drop_at.is_none() {
        let profile = std::env::var("MICA_START_PROFILE").ok();
        if profile.is_some() {
            std::env::remove_var("MICA_START_PROFILE");
        }
        let _ = start_tab_with_profile(hwnd, profile.as_deref(), None);
    }
    // GPU 建后补一次 clear_color 与背景图(create_tab 不碰窗口资源)
    GPU.with(|g| {
        if let Some(gpu) = g.borrow_mut().as_mut() {
            gpu.renderer.set_clear_color(&settings.palette);
            if settings.background_image.is_some() {
                sync_background_image(&mut gpu.renderer);
            }
        }
    });
    if start_translucent {
        apply_window_alpha(hwnd, true);
    }

    // 首帧显式化 + 显窗后置(避免未初始化 swapchain 白底闪屏)
    draw_frame();
    if let Some(t0) = t0
        && std::env::var_os("MICA_DEBUG_TIMING").is_some()
    {
        eprintln!(
            "[timing] run→首帧 {}ms(含窗口/GPU/首标签)",
            t0.elapsed().as_millis()
        );
    }
    if drop_at.is_none() {
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
    Some((hwnd, hwnd_slot))
}

pub fn run() {
    // #59 启动计时锚(exec 起点近似;MICA_DEBUG_TIMING 门控输出分段)
    let t0 = std::time::Instant::now();
    // ConPTY 宿主消毒(1.0.6):进程内剔除第三方 OpenConsole 目录,
    // 见 pty.rs 的 sanitize_conpty_host_path 文档——必须在一切 spawn
    // 之前(profile 扫描/QT/主窗 pty 全部受益)
    mica_core::pty::sanitize_conpty_host_path();
    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None).expect("GetModuleHandleW").0);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // CS_DBLCLKS:双击翻译成 WM_LBUTTONDBLCLK(选择词/行语义的地基)
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: HICON::default(),
            hCursor: LoadCursorW(None, windows::Win32::UI::WindowsAndMessaging::IDC_ARROW)
                .expect("no arrow cursor"),
            hbrBackground: HBRUSH::default(), // 客户区由 wgpu 每帧清屏
            lpszMenuName: PCWSTR::null(),
            lpszClassName: w!("mica_app_class"),
            hIconSm: HICON::default(),
        };
        assert_ne!(RegisterClassExW(&wc), 0, "RegisterClassExW failed");

        // T4:字号与字体链来自配置(%APPDATA%\mica\config;缺失 = 全默认)
        let settings = load_settings();
        let Some((hwnd, hwnd_slot)) = bootstrap_window(hinstance, &settings, true, None, Some(t0))
        else {
            return;
        };

        // #109:watcher 是进程级服务,必须在消息循环能够关闭主窗前建立。
        // 保留首帧优先,但不能留给随主窗 timer 7 一起消失的延迟初始化。
        if let Some(reload) = spawn_config_watcher() {
            *CONFIG_WATCHER.lock().unwrap_or_else(|e| e.into_inner()) = Some(reload);
        }

        // #59 非关键初始化推迟:首帧已上屏,余下(COM/进程枚举/wsl.exe/
        // 第二个 GPU surface)经一拍定时器在主线程补做——
        // TLS 安全(全主线程),用户感知 = 首帧即此前的全部等待
        // Debug-only controlled startup timing for the early-main-close regression.
        // Release builds always retain the normal 100ms deferred initialization.
        let deferred_ms = if cfg!(debug_assertions) {
            std::env::var("MICA_TEST_DEFER_INIT_MS")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|ms| (100..=60_000).contains(ms))
                .unwrap_or(100)
        } else {
            100
        };
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 7, deferred_ms, None);
        START_T0.with(|c| c.set(Some(t0)));
        message_loop(hwnd_slot);
    }

    // 多窗口寿命(2026-10-12):主窗退出序已毕(其标签已拆、watcher 已停),
    // 副窗各在各线程;计数归零才让 main 返回(进程退出)。期间主线程
    // 纯 park,不再拥有任何窗口资源
    while WINDOW_COUNT.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        std::thread::sleep(Duration::from_millis(120));
    }
    // #124 诊断:主线程离开 park 即将 process::exit——此后仍不退出 =
    // CRT/DLL 卸载(loader lock)卡死,不是窗口逻辑
    orphan::clear_sessions_for_exit();
    eprintln!("[exit] 主线程离开 park,run() 返回,即将 process::exit");
}

/// 起新窗口(2026-10-12 多窗口):新线程跑与主窗同构的窗口创建序——
/// 窗口级状态(GPU/TABS/strip TLS)全线程本地,天然隔离;类已注册、
/// IPC 服务/配置 watcher/孤儿清理/Quick Terminal 留在主窗(副窗不重复起)。
/// 主窗退出序后 park 等计数归零(见 run() 尾),副窗随最后一窗关闭退出。
/// profile:Some = IPC new-window 的 profile(暂走默认 shell,预留)。
pub(super) fn spawn_window(profile: Option<String>) {
    let _ = profile; // 预留
    spawn_window_impl(None);
}

// A hidden destination owns no PTY until the source accepts its offer.
struct EmptyWindow(SharedHwnd);

impl Drop for EmptyWindow {
    fn drop(&mut self) {
        if let Some(raw) = *self.0.lock().unwrap() {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(raw as _)),
                    WM_APP_CLOSE_EMPTY,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

thread_local! {
    static DETACHING: RefCell<Vec<(u64, mpsc::Receiver<EmptyWindow>)>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn detach_live_tab(id: u64, pos: POINT) {
    let Some(source) = SHARED_HWND.with(|s| s.borrow().clone()) else {
        return;
    };
    let (tx, rx) = mpsc::channel();
    if DETACHING.with(|d| {
        let mut pending = d.borrow_mut();
        if pending.iter().any(|(tab, _)| *tab == id) {
            return false;
        }
        pending.push((id, rx));
        true
    }) {
        spawn_window_impl(Some((pos, source, tx)));
    }
}

pub(super) fn finish_detach() {
    let mut ready = Vec::new();
    DETACHING.with(|pending| {
        pending.borrow_mut().retain(|(id, rx)| match rx.try_recv() {
            Ok(window) => {
                ready.push((*id, window));
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        })
    });
    for (id, window) in ready {
        let raw = *window.0.lock().unwrap();
        if let Some(raw) = raw {
            let hwnd = HWND(raw as _);
            if transfer::send(id, hwnd, 0) {
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindowAsync(
                        hwnd,
                        windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
                    );
                }
            }
        }
    }
}

fn spawn_window_impl(detach: Option<(POINT, SharedHwnd, mpsc::Sender<EmptyWindow>)>) {
    if let Err(err) = std::thread::Builder::new()
        .name("mica-window".into())
        .spawn(move || unsafe {
            let settings = load_settings();
            let hinstance = HINSTANCE(GetModuleHandleW(None).expect("GetModuleHandleW").0);
            let window = bootstrap_window(
                hinstance,
                &settings,
                false,
                detach.as_ref().map(|d| d.0),
                None,
            );
            if let Some((_, source, tx)) = detach {
                if let Some((_, slot)) = &window {
                    let _ = tx.send(EmptyWindow(slot.clone()));
                }
                drop(tx);
                if let Some(raw) = *source.lock().unwrap() {
                    let _ = PostMessageW(
                        Some(HWND(raw as _)),
                        WM_APP_DETACH_READY,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            if let Some((_, slot)) = window {
                message_loop(slot);
            }
        })
    {
        eprintln!("could not start window thread: {err}");
        finish_detach();
    }
}

/// #59 非关键初始化(首帧后一拍,主线程):孤儿清理 / jumplist COM /
/// profile 扫描 / Quick Terminal。全在主线程执行——
/// 这些件吃 TLS(GPU/SHARED_HWND),后台线程会撞双实例 TLS。
/// DEFER_DONE 只在主窗线程置位(timer 挂主窗 hwnd),副窗天然不进
pub(super) unsafe fn deferred_init(hwnd: HWND) {
    if DEFER_DONE.with(|c| c.replace(true)) {
        return;
    }
    let t = std::time::Instant::now();
    // 孤儿会话清理(T02,2026-11-01 重造):只清注册表里能证明归属本项目
    // 旧实例的会话(child 活着 + 创建时间一致 + owner 已死),不再按全机
    // 进程名+父 PID 泛杀。首标签 pty 属存活的本进程,天然跳过
    let orphans = orphan::sweep_owned(
        &orphan::Registry::new(orphan::Registry::default_path()),
        true,
    );
    if orphans > 0 {
        eprintln!("[orphan] 清杀 {orphans} 个上个实例遗留的孤儿会话");
    }
    // M3a:AUMID + jump list(profile 任务直达;COM 实例化是慢路径)
    jumplist::set_appuser_model_id();
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    jumplist::install_cached_profiles(&exe);
    // M3b:Quick Terminal(独立 GPU surface,重)
    let _ = quickterm::init(hwnd);
    if std::env::var_os("MICA_DEBUG_TIMING").is_some() {
        let total = START_T0
            .with(std::cell::Cell::get)
            .map(|t0| t0.elapsed().as_millis())
            .unwrap_or_default();
        eprintln!(
            "[timing] 延迟段 {}ms(孤儿/COM/profile/QT),累计 {}ms",
            t.elapsed().as_millis(),
            total
        );
    }
}

/// 警告框:不挂窗口(启动期 hwnd 尚未创建),自带消息泵,OK 后继续初始化。
pub(super) fn warn_box(text: &str) {
    unsafe {
        let _ = MessageBoxW(
            None,
            &HSTRING::from(text),
            w!("Mica"),
            MB_OK | MB_ICONWARNING,
        );
    }
}

unsafe fn message_loop(hwnd_slot: SharedHwnd) {
    let mut msg = MSG::default();
    loop {
        let ret = GetMessageW(&mut msg, None, 0, 0);
        if ret.0 == -1 {
            // MSDN 明言别拿返回值当 bool:-1 是错误,消息内容未定义,只记录
            eprintln!("GetMessageW failed, GetLastError={}", GetLastError().0);
            break;
        }
        if ret.0 == 0 {
            break; // WM_QUIT:走退出序
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    // 退出序 = T5 序的推广(哨兵 → 断源 → join):
    // #124 诊断:退出序分相计时(单调毫秒,自 WM_QUIT 起)。close_terminal
    // 内部还有 per-pane 细分,由 EXIT_TRACING 门控(正常运行不刷屏)
    let exit_t0 = std::time::Instant::now();
    let phase = |name: &str| {
        eprintln!("[exit] {name} (+{}ms)", exit_t0.elapsed().as_millis());
    };
    lifecycle::EXIT_TRACING.store(true, std::sync::atomic::Ordering::Relaxed);
    phase("step1: sentinel None");
    *hwnd_slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    phase("step2: watcher 留存(进程级,副窗仍在时热重载继续)");
    phase("step5: reclaim terminals, leak only GPU");
    let tabs = TABS.with(|tabs| std::mem::take(&mut *tabs.borrow_mut()));
    for tab in tabs {
        for pane in tab.panes {
            lifecycle::close_terminal(pane.terminal);
        }
    }
    phase("step5a: terminals reclaimed");
    let mut gpu = GPU.with(|g| g.borrow_mut().take());
    if let Some(gpu) = gpu.as_mut() {
        gpu.renderer.release_atlas_resources();
    }
    std::mem::forget(gpu);
    phase("step5b: atlas released");
    crate::app::quickterm::forget_state();
    phase("step5c: qt state forgotten");
    DETACHING.with(|pending| pending.borrow_mut().clear());
    phase("step5d: registry cleared");
    let (ok, fallback, inflight, degraded) = lifecycle::reaper_stats();
    eprintln!(
        "[exit] step6: done (reaper 已回收 {ok}/降级 forget {fallback}/在途 {inflight}/故障态 {degraded};pty 预留 {}B) (+{}ms)",
        pty_bridge::reserved_bytes(),
        exit_t0.elapsed().as_millis()
    );
}
