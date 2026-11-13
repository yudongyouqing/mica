//! Configuration loading, watching and application on the owning UI thread.
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use mica_core::config::settings::{self, ConfigError, Settings};
use notify::{Event, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// 热重载组件打包:退出时按 哨兵置 None(共用,见 SharedHwnd)→ drop watcher
/// → join 线程 拆除(序在 message_loop 的退出分支,先于既有 teardown)。
pub(super) struct ReloadHandle {
    pub(super) watcher: notify::RecommendedWatcher,
    pub(super) thread: std::thread::JoinHandle<()>,
}

/// 配置文件路径:`%APPDATA%\mica\config`;APPDATA 未设(理论上不存在,
/// 保留兜底)落当前目录 `.mica\config`。
fn config_path() -> PathBuf {
    mica_core::config::user_config_path()
}

/// 读配置并合成 Settings。错误哲学(T2 同款):终端绝不因配置拒绝启动——
/// 文件缺失 = 全默认静默;读失败或 resolve 失败 = MessageBox 警告后回落默认。
pub(super) fn load_settings() -> Settings {
    let path = config_path();
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
        Err(e) => {
            warn_box(&format!(
                "无法读取配置文件 {}:\n{e}\n\n继续使用默认设置。",
                path.display()
            ));
            return Settings::default();
        }
    };
    // PowerShell 5 `Out-File` 等工具写 UTF-8 会带 BOM:不剥则首键(常是
    // theme/font-family)被静默吞掉,配置"半生效"且无任何提示
    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
    match settings::resolve(source) {
        Ok(s) => s,
        Err(err) => {
            warn_box(&format_config_error(&path, &err, "已回退默认设置"));
            Settings::default()
        }
    }
}

/// 错误弹窗文案:解析行错误带行号在前,值语义错误随后(与 ConfigError 同序)。
/// 结尾措辞按调用点参数化:启动路径回退默认,热重载路径保留当前设置。
fn format_config_error(path: &Path, err: &ConfigError, outcome: &str) -> String {
    let mut msg = format!("配置文件 {} 有误,{outcome}:", path.display());
    for e in &err.parse {
        msg.push_str(&format!("\n  第 {} 行: {}", e.line, e.reason));
    }
    for v in &err.values {
        msg.push_str(&format!("\n  {v}"));
    }
    msg
}

/// 配置热重载 watcher(最佳努力,绝不致命——T4 错误哲学同款):
/// notify 推荐后端(Windows = ReadDirectoryChangesW;对文件路径它实际监视
/// 父目录并按路径过滤,原子替换/重建不失联)监听 config 文件,事件经 mpsc
/// 交给去抖线程。None = 未监听(文件缺失或 watch 失败,热重载降级关闭)。
/// 哨兵由 run() 统一建立后传入——与转发线程共用"窗口存活"这一个事实。
pub(super) fn spawn_config_watcher(hwnd_slot: SharedHwnd) -> Option<ReloadHandle> {
    let path = config_path();
    if !path.exists() {
        eprintln!("config watcher: {} 不存在,热重载未启用", path.display());
        return None;
    }
    // notify 的 EventHandler 直接支持 std mpsc Sender<Result<Event>>;
    // watcher 持有发送端,Drop 时后端停止、通道断开,线程随之退出
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    // 创建/监听/起线程三步都按"绝不致命"降级:失败只记日志、热重载关闭,
    // 绝不 expect 崩终端
    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("config watcher: 创建失败:{e};热重载未启用");
            return None;
        }
    };
    if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
        eprintln!(
            "config watcher: 无法监听 {}: {e};热重载未启用",
            path.display()
        );
        return None;
    }
    let thread_slot = Arc::clone(&hwnd_slot);
    let thread = match std::thread::Builder::new()
        .name("config-watcher".into())
        .spawn(move || debounce_loop(path, rx, thread_slot))
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("config watcher: 线程创建失败:{e};热重载未启用");
            return None; // watcher 随之 Drop,后端停止
        }
    };
    Some(ReloadHandle { watcher, thread })
}

/// 去抖线程:notify 后端已按被监视文件过滤事件,收到的必是 config 的动静。
/// 编辑器原子保存 = Delete+Create+Write 连发,不去抖会触发多轮重载并撞上
/// 瞬时缺失窗——故收首个事件后持续吞事件,直到 300ms 静默窗走完才投递一次;
/// 路径在静默结束时仍不存在(原子替换途中)则跳过本轮。通道断开(watcher
/// 已 Drop)即退出。
fn debounce_loop(path: PathBuf, rx: mpsc::Receiver<notify::Result<Event>>, hwnd_slot: SharedHwnd) {
    const QUIET: Duration = Duration::from_millis(300);
    loop {
        if rx.recv().is_err() {
            return; // watcher 已停,通道断开
        }
        loop {
            match rx.recv_timeout(QUIET) {
                Ok(_) => {} // 风暴未息,续等
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        if !path.exists() {
            continue;
        }
        // 复核 4:广播到**全部**存活窗口(单投会让多窗配置漂移);
        // 无存活窗口(进程退出中)静默丢弃本轮
        let _ = window_registry::post_to_all_alive(WM_APP_CONFIG, WPARAM(0), LPARAM(0));
        let _ = &hwnd_slot;
    }
}

/// 热重载主路径(WM_APP_CONFIG,主线程):重读+合成;失败弹窗保旧、现状态
/// 不动;成功则重建 DwriteRouter(旧 router Drop 释放图集,新字形随绘制自然
/// 重光栅化,无需显式清缓存)、按**不变的窗口像素尺寸**重算网格、resize 终端
/// 与会话、换 palette 与清屏色,最后全量重绘。
pub(super) unsafe fn reload_config(hwnd: HWND) {
    let path = config_path();
    let Ok(source) = std::fs::read_to_string(&path) else {
        // 去抖后仍读不到:文件被删而非瞬时缺失,提示后保旧
        warn_box(&format!(
            "无法读取配置文件 {}:\n热重载跳过,继续使用当前设置。",
            path.display()
        ));
        return;
    };
    // 与启动路径同一 BOM 剥离:热重载不该比首读更挑剔编码
    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
    let settings = match settings::resolve(source) {
        Ok(s) => s,
        Err(err) => {
            warn_box(&format_config_error(&path, &err, "已保留当前设置"));
            return; // 保旧
        }
    };
    let families: Vec<&str> = settings.font_families.iter().map(String::as_str).collect();
    let Ok(router) = DwriteRouter::new(scaled_pt(settings.font_size_pt), &families) else {
        warn_box("重建字体链失败,继续使用当前设置。");
        return; // 保旧
    };
    eprintln!("font families in use: {:?}", router.families_in_use()); // 冒烟期观察回退链
    let metrics = router.metrics();

    // 客户区像素尺寸不变(不碰窗口尺寸),终端区按新度量重算(减 strip)
    let mut rect = RECT::default();
    GetClientRect(hwnd, &mut rect).expect("GetClientRect");
    let mut reloaded = false;
    TABS.with(|tabs| {
        // 全部标签全部 pane 一起换(T7):palette/字体是全局语义
        for tab in tabs.borrow_mut().iter_mut() {
            for pane in tab.panes.iter_mut() {
                let t = &mut pane.terminal;
                let Ok(router) = DwriteRouter::new(scaled_pt(settings.font_size_pt), &families)
                else {
                    return;
                };
                t.router = router;
                // strip 字体/几何随配置(STRIP_CFG 已在上方更新):惰性建
                // 置空强制下一帧按新配置重建
                t.strip_router = None;
                t.metrics = metrics;
                // 应答值取整与布局换算 f32 的分工同 init(I3)
                t.term.set_cell_metrics(
                    metrics.cell_width.round() as u16,
                    metrics.line_height.round() as u16,
                );
                // 几何/字形全换:行缓存结构性失配,显式全量(Task 8)
                t.force_full = true;
                t.term.set_palette(&settings.palette);
                t.palette = settings.palette;
            }
        }
        reloaded = true;
    });
    // 窗口级同步:清屏色、键位表、建标签种子(T7:新标签跟随当前设置)
    GPU.with(|g| {
        if let Some(gpu) = g.borrow_mut().as_mut() {
            gpu.renderer.set_clear_color(&settings.palette);
        }
    });
    KEYMAP.with(|k| *k.borrow_mut() = settings.keymap.clone());
    TITLE_LOCK.with(|t| *t.borrow_mut() = settings.title.clone());
    MOUSE_HIDE_ON_TYPE.with(|m| m.set(settings.mouse_hide_while_typing));
    COPY_ON_SELECT.with(|c| c.set(settings.copy_on_select));
    RIGHT_CLICK_PASTE.with(|r| r.set(settings.mouse_right_click == MouseRightClick::Paste));
    KEY_REMAP.with(|k| *k.borrow_mut() = settings.key_remap.clone());
    let dim = if (settings.background_opacity - 1.0).abs() < f32::EPSILON {
        217
    } else {
        (settings.background_opacity.clamp(0.05, 1.0) * 255.0) as u8
    };
    WINDOW_ALPHA_DIM.with(|a| a.set(dim));
    // 热重载改透明度:立即生效
    if (settings.background_opacity - 1.0).abs() >= f32::EPSILON {
        WINDOW_TRANSLUCENT.with(|t| t.set(true));
        apply_window_alpha(hwnd, true);
    } else if WINDOW_TRANSLUCENT.with(std::cell::Cell::get) {
        // 改回 1.0:若处于透明档(配置进的,非 toggle),回不透明
        WINDOW_TRANSLUCENT.with(|t| t.set(false));
        apply_window_alpha(hwnd, false);
    }
    TAB_SEED.with(|s| *s.borrow_mut() = (settings.font_families.clone(), settings.font_size_pt));
    SESSION_COMMAND.with(|s| *s.borrow_mut() = settings.command.clone());
    CONTENT_PADDING
        .with(|p| *p.borrow_mut() = (settings.window_padding_x, settings.window_padding_y));
    QUICK_TERMINAL_HEIGHT.with(|h| h.set(settings.quick_terminal_height));
    QUICK_TERMINAL_KEY.with(|k| *k.borrow_mut() = settings.quick_terminal_key.clone());
    quickterm::apply_hotkey(settings.quick_terminal_key.as_deref());
    CURSOR_BLINK.with(|c| c.set(settings.cursor_blink));
    BLINK_PHASE.with(|p| p.set(true));
    if settings.cursor_blink {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 1, 500, None);
    } else {
        let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 1);
    }
    BG_IMG_PATH.with(|b| *b.borrow_mut() = settings.background_image.clone());
    GPU.with(|g| {
        if let Some(gpu) = g.borrow_mut().as_mut() {
            let path = settings
                .background_image
                .as_deref()
                .map(std::path::Path::new);
            let alpha = if (settings.background_opacity - 1.0).abs() < f32::EPSILON {
                0.9
            } else {
                settings.background_opacity
            };
            if let Err(e) = gpu.renderer.set_background_image(path, alpha) {
                eprintln!("[bg] {e}");
            }
        }
    });
    STRIP_CFG.with(|c| *c.borrow_mut() = settings.strip.clone());
    CURRENT_PALETTE.with(|p| *p.borrow_mut() = settings.palette);
    // title 锁定值即时应用(此前要等下一次 shell 标题事件才生效)——
    // 这也是"热重载确实执行"的外部观测面(测试读窗口文本)
    if let Some(t) = TITLE_LOCK.with(|t| t.borrow().clone()) {
        let _ = SetWindowTextW(hwnd, &HSTRING::from(t));
    }
    resize_all_grids(rect.right.max(1) as f32, rect.bottom.max(1) as f32);
    // draw_frame 自己也要借 TABS/GPU,必须在 with 之外调用
    if reloaded {
        draw_frame();
    }
}
