//! Titlebar / tab-strip geometry, animations, hit-testing, hover tracking,
//! overflow scrolling, and tab title presentation.
#![allow(unsafe_op_in_unsafe_fn)]

use super::*;

/// strip/顶栏运行时几何(DPI 缩放 + 用户配置单源,渲染与 hit-test 同取):
/// 150% 屏上物理常量曾致条高/× 与字体错位(2026-10-06 用户实测);
/// strip-* 配置键热重载时经 STRIP_CFG 生效(2026-10-12 全量可配)
pub(super) fn strip_metrics() -> mica_render::frame::StripMetrics {
    mica_render::frame::StripMetrics::from_scale_cfg(
        DPI_SCALE.with(std::cell::Cell::get),
        &STRIP_CFG.with(|c| c.borrow().clone()),
    )
}

/// 条总高(动态,2026-10-12):单标签隐藏第二行(上游 hasTabBar 同款;
/// strip-hide-single-tab = false 恒显)——终端区起点/命中上界都取此值;
/// 标签数跨 1↔2 时 draw_frame 的网格兜底重排自然跟进。读 TAB_COUNT
/// Cell(无借用):任意路径安全可调
pub(super) fn strip_h() -> f32 {
    let sm = strip_metrics();
    if TAB_COUNT.with(std::cell::Cell::get) <= 1 && sm.hide_single {
        sm.title_h
    } else {
        sm.h
    }
}

// 单调时钟(动画用):线程本地原点,CAPS_T0 存相对此刻的秒数,取差即
// 经过时间——Instant 不能进 const Cell,这是等价物
thread_local! {
    static CLOCK0: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

fn now_secs() -> f64 {
    CLOCK0.with(|c| {
        let t0 = c.get().unwrap_or_else(|| {
            let n = std::time::Instant::now();
            c.set(Some(n));
            n
        });
        t0.elapsed().as_secs_f64()
    })
}

/// 活跃标签胶囊当前横移量(动画值):ease-out cubic;时长可配
/// (strip-animation-duration,0 = 关);过期 = 0
pub(super) fn capsule_dx_now() -> f32 {
    let t0 = CAPS_T0.with(std::cell::Cell::get);
    if t0 < 0.0 {
        return 0.0;
    }
    let dur = STRIP_CFG.with(|c| c.borrow().anim_ms).max(1.0) / 1000.0;
    let k = (((now_secs() - t0) as f32) / dur).clamp(0.0, 1.0);
    let ease = 1.0 - (1.0 - k).powi(3);
    CAPS_START.with(std::cell::Cell::get) * (1.0 - ease)
}

/// 起一次胶囊滑动(切换/新建/关闭标签后调用):从旧活跃位滑到新活跃位
pub(super) fn prime_capsule_slide(old_idx: usize, new_idx: usize) {
    if old_idx == new_idx || STRIP_CFG.with(|c| c.borrow().anim_ms) <= 0.0 {
        return; // 动画关闭(strip-animation-duration = 0)
    }
    let sm = strip_metrics();
    let area_w = GPU
        .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.width as f32))
        .unwrap_or(0.0);
    let n = TAB_COUNT.with(std::cell::Cell::get).max(1);
    let w = mica_render::frame::tab_w_now(n, area_w, &sm);
    let dx = (old_idx as f32 - new_idx as f32) * (w + sm.tab_gap);
    CAPS_START.with(|c| c.set(dx));
    CAPS_T0.with(|t| t.set(now_secs()));
}

/// 拖拽排列:被拖标签占 slot,其余按原序填空(与渲染层同构)
pub(super) fn tab_order(n: usize, di: usize, slot: usize) -> Vec<usize> {
    if di >= n {
        return (0..n).collect();
    }
    let mut v: Vec<usize> = (0..n).filter(|i| *i != di).collect();
    v.insert(slot.min(v.len()), di);
    v
}

/// 武装 strip 动画定时器(timer 5,15ms):有动画在跑时反复调用无害
pub(super) fn ensure_anim_timer() {
    if let Some(slot) = SHARED_HWND.with(|s| s.borrow().clone())
        && let Some(raw) = *slot.lock().unwrap_or_else(|e| e.into_inner())
    {
        // SAFETY: 哨兵非空即窗口存活(run() 退出序先置 None)
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
                Some(windows::Win32::Foundation::HWND(raw as _)),
                5,
                15,
                None,
            );
        }
    }
}

/// strip 动推进一步(timer 5,15ms):focus 相位渐近 + 胶囊滑移衰减;
/// 返回 true = 还有动画在跑(timer 续命),false = 全部到位(可停表)
pub(super) fn strip_anim_step() -> bool {
    let mut busy = false;
    // focus 渐近(默认 150ms;strip-focus-animation-duration 可配,0 = 直达)
    let fp = FOCUS_PHASE.with(std::cell::Cell::get);
    let ft = FOCUS_TARGET.with(std::cell::Cell::get);
    let ms = STRIP_CFG.with(|c| c.borrow().focus_ms);
    if (fp - ft).abs() > 0.004 && ms > 0.0 {
        let step = 0.015 / (ms / 1000.0);
        let nf = if fp < ft {
            (fp + step).min(ft)
        } else {
            (fp - step).max(ft)
        };
        FOCUS_PHASE.with(|f| f.set(nf));
        busy = true;
    } else if fp != ft {
        FOCUS_PHASE.with(|f| f.set(ft));
    }
    // 胶囊:由时间驱动,只在未到期时算忙
    let t0 = CAPS_T0.with(std::cell::Cell::get);
    if t0 >= 0.0 && capsule_dx_now().abs() > 0.5 {
        busy = true;
    } else if t0 >= 0.0 {
        CAPS_T0.with(|t| t.set(-1.0));
    }
    // 悬停反色动画:120ms ease-out,从下划线长到整块
    if HOVER_PATH.with(|h| h.borrow().is_some()) {
        let a = HOVER_ANIM.with(std::cell::Cell::get);
        if a < 1.0 {
            HOVER_ANIM.with(|a| a.set((a.get() + 0.015 / 0.120).min(1.0)));
            busy = true;
        }
    } else {
        HOVER_ANIM.with(|a| a.set(1.0)); // 不在路径上:无动画态
    }
    // 邻位滑移:指数衰减(15ms 一拍 ×0.72 ≈ 105ms 收敛),全落定即停
    TAB_DX.with(|d| {
        let mut d = d.borrow_mut();
        if d.iter().any(|v| v.abs() > 0.5) {
            for v in d.iter_mut() {
                *v *= 0.72;
            }
            busy = true;
        } else {
            d.clear();
        }
    });
    busy
}

/// strip 命中(双行形制,#39 起含溢出箭头):Some(StripTabHit) = 第二行
/// 的标签/加号/箭头;None = 第一行(标题/caption = 拖拽或系统钮)或终端区
/// (单标签时第二行整行隐藏,上界取动态 strip_h)。
/// 布局常量单源在 frame(tabs_x0/plus_x/tab_w_now/arrow_w)。
pub(super) fn strip_hit(px: i32, py: i32) -> Option<mica_render::frame::StripTabHit> {
    use mica_render::frame::StripTabHit;
    let sm = strip_metrics();
    if py < 0 || py as f32 >= strip_h() {
        return None; // 不在 strip 区
    }
    if (py as f32) < sm.title_h {
        return None; // 第一行:标题/caption(点击 = 拖拽或系统钮,非客户区)
    }
    // 第二行:固定视口坐标的加号/溢出箭头优先,再在标签可视域内换算滚动后的标签槽
    let area_w = GPU
        .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.width as f32))
        .unwrap_or(0.0);
    let n = TABS.with(|tabs| tabs.borrow().len());
    let x = px.max(0) as f32;
    let p_x = mica_render::frame::plus_x(area_w, &sm);
    if x >= p_x && x < p_x + sm.plus_w {
        return Some(StripTabHit::Plus);
    }
    let w = mica_render::frame::tab_w_now(n, area_w, &sm);
    let span = w + sm.tab_gap;
    let right = mica_render::frame::tabs_right(area_w, &sm);
    // 溢出才显箭头(与渲染同判据);箭头绘制在固定屏幕坐标 [2k, 2k+arrow_w) /
    // [right-arrow_w, right),不随 TAB_SCROLL 偏移
    if mica_render::frame::tabs_x0(&sm) + n as f32 * span > right + 0.5 {
        if x >= 2.0 * sm.k && x < 2.0 * sm.k + sm.arrow_w {
            return Some(StripTabHit::ArrowLeft);
        }
        if x >= right - sm.arrow_w && x < right {
            return Some(StripTabHit::ArrowRight);
        }
    }
    let x0 = mica_render::frame::tabs_x0(&sm);
    if x < x0 || x >= right {
        return None;
    }
    let shift = TAB_SCROLL.with(std::cell::Cell::get) as f32 * sm.tab_min_w;
    let local = x + shift - x0;
    let idx = (local / span).floor();
    if idx >= 0.0 && (idx as usize) < n {
        Some(StripTabHit::Tab(idx as usize))
    } else {
        None
    }
}

/// strip 悬停命中(2026-10 UI 对标):第一行 = caption 三键(右端),
/// 第二行 = 标签/加号。与 strip_hit 同几何单源。
pub(super) fn strip_hover_at(px: i32, py: i32) -> mica_render::frame::StripHover {
    use mica_render::frame::{CaptionBtn, StripHover};
    let mut h = StripHover::default();
    let sm = strip_metrics();
    if py < 0 || py as f32 >= strip_h() {
        return h;
    }
    if (py as f32) < sm.title_h {
        let w = GPU
            .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.width as i32))
            .unwrap_or(0);
        let left = w - (sm.caption_w * 3.0) as i32;
        if px >= left {
            h.caption = Some(match ((px - left) as f32 / sm.caption_w) as i32 {
                0 => CaptionBtn::Min,
                1 => CaptionBtn::Max,
                _ => CaptionBtn::Close,
            });
        }
        return h;
    }
    match strip_hit(px, py) {
        Some(mica_render::frame::StripTabHit::Tab(i)) => h.tab = Some(i),
        Some(mica_render::frame::StripTabHit::Plus) => h.plus = true,
        Some(mica_render::frame::StripTabHit::ArrowLeft) => h.arrow_l = true,
        Some(mica_render::frame::StripTabHit::ArrowRight) => h.arrow_r = true,
        None => {}
    }
    h
}

/// NC 悬停离开武装(TME_NONCLIENT):caption 按钮区被 NCHITTEST 判为非客户
/// 区,离开事件是 WM_NCMOUSELEAVE 而非 WM_MOUSELEAVE
pub(super) fn arm_nc_leave(hwnd: HWND) {
    let mut tme = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_NONCLIENT | TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    // SAFETY: OS 追踪注册;失败只是收不到 NC leave,悬停态由下一帧覆盖
    unsafe {
        let _ = TrackMouseEvent(&mut tme);
    }
}

/// 标签溢出滚动调整(T8):步进 1 个标签宽,clamp 到 [0, max];返回是否变化。
/// 可视域 = tabs_x0 到 tabs_right(第二行,"+" 左缘)。
pub(super) fn tab_scroll_adjust(delta: i32) -> bool {
    let area_w = GPU
        .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.width as f32))
        .unwrap_or(0.0);
    let sm = strip_metrics();
    let n = TABS.with(|tabs| tabs.borrow().len());
    let w = mica_render::frame::tab_w_now(n, area_w, &sm);
    let need = mica_render::frame::tabs_x0(&sm) + n as f32 * (w + sm.tab_gap);
    let right = mica_render::frame::tabs_right(area_w, &sm);
    let max = if area_w <= 0.0 || need <= right {
        0
    } else {
        ((need - right) / sm.tab_min_w).ceil() as i32
    };
    let old = TAB_SCROLL.with(std::cell::Cell::get);
    let new = (old + delta).clamp(0, max);
    if new != old {
        TAB_SCROLL.with(|s| s.set(new));
        true
    } else {
        false
    }
}

/// 活跃标签滚入可视区(T8 自动跟随):切换/新建标签时调用。
pub(super) fn tab_follow_active() {
    let area_w = GPU
        .with(|g| g.borrow().as_ref().map(|gpu| gpu.config.width as f32))
        .unwrap_or(0.0);
    let sm = strip_metrics();
    let n = TABS.with(|tabs| tabs.borrow().len());
    let w = mica_render::frame::tab_w_now(n, area_w, &sm);
    let right = mica_render::frame::tabs_right(area_w, &sm);
    let visible = if area_w <= 0.0 {
        i32::MAX
    } else {
        (((right - mica_render::frame::tabs_x0(&sm)) / (w + sm.tab_gap)).floor() as i32).max(1)
    };
    let active = ACTIVE.get() as i32;
    let cur = TAB_SCROLL.with(std::cell::Cell::get);
    let want = if active < cur + 1 {
        (active - 1).max(0) // 左侧滚出:活跃前留一标签
    } else if active > cur + visible.saturating_sub(1) {
        active - visible + 1 // 右侧滚出
    } else {
        cur
    };
    if want != cur {
        TAB_SCROLL.with(|s| s.set(want));
        let _ = tab_scroll_adjust(0); // clamp
    }
}

/// 标签显示名(#81 导出文件名用):TITLE_LOCK 优先,回退标签自身标题
pub(super) fn tab_title_safe(tab: &TabState) -> String {
    TITLE_LOCK
        .with(|t| t.borrow().clone())
        .unwrap_or_else(|| tab.title.clone())
}

/// 标题呈现(Ghostty 语义的 Windows 适配):OSC 0/2 原样信任(Ghostty 的
/// shell 集成会发 user@host:cwd 这类好标题);Windows shell 常发裸 exe
/// 路径(PowerShell 默认),取 basename 去扩展——与 Ghostty direct
/// command 取 argv[0] 同一意图:标题=正在跑的程序名。
pub(super) fn pretty_title(raw: &str) -> String {
    let t = raw.trim();
    let lower = t.to_ascii_lowercase();
    if lower.ends_with(".exe") || lower.ends_with(".cmd") || lower.ends_with(".bat") {
        let base = t.rsplit(['\\', '/']).next().unwrap_or(t);
        return base
            .strip_suffix({
                let l = base.to_ascii_lowercase();
                if l.ends_with(".exe") {
                    ".exe"
                } else if l.ends_with(".cmd") {
                    ".cmd"
                } else {
                    ".bat"
                }
            })
            .unwrap_or(base)
            .to_string();
    }
    t.to_string()
}
