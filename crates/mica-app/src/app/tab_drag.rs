//! Hit-test a live tab drag in the destination window's own DPI and scroll space.
use super::*;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::*;
pub(super) const PREVIEW: u32 = 0x8042;
thread_local! {
    static SOURCE_ID: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static SLOT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static TARGET: std::cell::Cell<Option<HWND>> = const { std::cell::Cell::new(None) };
    static MOVING_SINGLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn drop_slot() -> Option<usize> {
    SLOT.get()
}

pub(super) fn preview(hwnd: HWND, active: bool, point: LPARAM) -> LRESULT {
    if !TABS.with(|t| t.try_borrow_mut().is_ok()) || !GPU.with(|g| g.try_borrow_mut().is_ok()) {
        return LRESULT(0);
    }
    let slot = active
        .then(|| {
            let (x, y) = mouse_xy(point);
            let mut rect = RECT::default();
            unsafe { GetClientRect(hwnd, &mut rect) }.ok()?;
            let sm = strip_metrics();
            let n = TAB_COUNT.get();
            let right = mica_render::frame::tabs_right(rect.right as f32, &sm);
            if n == 0
                || x < 0
                || x as f32 >= right
                || y < 0
                || (n == 1 && sm.hide_single && SLOT.get().is_none() && y as f32 >= sm.title_h)
                || y as f32 >= sm.h
                || ((y as f32) < sm.title_h && x as f32 >= rect.right as f32 - 3.0 * sm.caption_w)
            {
                return None;
            }
            let width = mica_render::frame::tab_w_now(n, rect.right as f32, &sm);
            let local = x as f32 + TAB_SCROLL.get() as f32 * sm.tab_min_w
                - mica_render::frame::tabs_x0(&sm);
            Some((((local + 0.5 * width) / (width + sm.tab_gap)).floor() as usize).min(n))
        })
        .flatten();
    if SLOT.replace(slot) != slot {
        draw_frame();
    }
    LRESULT(slot.map_or(0, |i| (i + 1) as isize))
}

fn clear_target() {
    if let Some(previous) = TARGET.take() {
        unsafe {
            SendMessageTimeoutW(
                previous,
                PREVIEW,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                150,
                None,
            );
        }
    }
}

fn hit(source: HWND, p: POINT, moving_window: bool) -> Option<(HWND, usize)> {
    unsafe {
        let mut candidate = GetAncestor(WindowFromPoint(p), GA_ROOT);
        if moving_window && candidate == source {
            while let Ok(next) = GetWindow(candidate, GW_HWNDNEXT) {
                candidate = next;
                let mut r = RECT::default();
                if IsWindowVisible(candidate).as_bool()
                    && !IsIconic(candidate).as_bool()
                    && GetWindowRect(candidate, &mut r).is_ok()
                    && (r.left..r.right).contains(&p.x)
                    && (r.top..r.bottom).contains(&p.y)
                {
                    break;
                }
            }
        }
        if TARGET.get() != Some(candidate) {
            clear_target();
        }
        if candidate == source || !window_registry::alive_windows().contains(&candidate) {
            return None;
        }
        let mut pt = p;
        if !windows::Win32::Graphics::Gdi::ScreenToClient(candidate, &mut pt).as_bool() {
            return None;
        }
        let packed = (pt.x as u16 as usize) | ((pt.y as u16 as usize) << 16);
        let mut result = 0;
        TARGET.set(Some(candidate));
        SendMessageTimeoutW(
            candidate,
            PREVIEW,
            WPARAM(1),
            LPARAM(packed as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            150,
            Some(&mut result),
        );
        result.checked_sub(1).map(|slot| (candidate, slot))
    }
}
fn screen(hwnd: HWND, x: i32, y: i32) -> POINT {
    let mut pt = POINT { x, y };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut pt);
    }
    pt
}
pub(super) fn track(hwnd: HWND, x: i32, y: i32) -> bool {
    hit(hwnd, screen(hwnd, x, y), false).is_some()
}
pub(super) fn begin(index: usize) {
    SOURCE_ID.set(id(index));
    DRAG_TAB.set(Some(index));
}
pub(super) fn owns(index: usize) -> bool {
    SOURCE_ID
        .get()
        .is_some_and(|expected| id(index) == Some(expected))
}
fn id(index: usize) -> Option<u64> {
    TABS.with(|tabs| tabs.borrow().get(index)?.panes.first().map(|p| p.id))
}
pub(super) fn drop_external(hwnd: HWND, index: usize, x: i32, y: i32) -> bool {
    let pt = screen(hwnd, x, y);
    let target = hit(hwnd, pt, false);
    clear_target();
    let Some(id) = id(index) else { return true };
    if let Some((dest, slot)) = target {
        transfer::send(id, dest, slot);
        return true;
    }
    // An invalid drop over another Mica window must never detach to a third one.
    let under = unsafe { GetAncestor(WindowFromPoint(pt), GA_ROOT) };
    if under != hwnd && window_registry::alive_windows().contains(&under) {
        return true;
    }
    let mut bounds = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut bounds) };
    if x < 0 || x >= bounds.right || y < 0 || y > strip_h() as i32 + 40 {
        window::detach_live_tab(id, pt);
        return true;
    }
    false
}
pub(super) fn cancel() {
    clear_target();
    SOURCE_ID.take();
    DRAG_TAB.take();
    TAB_DX.with(|d| d.borrow_mut().clear());
}
pub(super) fn native_move(hwnd: HWND, finished: bool) {
    if TAB_COUNT.get() != 1 {
        return;
    }
    if finished && !MOVING_SINGLE.replace(false) {
        return;
    }
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_err() {
        clear_target();
        return;
    }
    let target = hit(hwnd, pt, true);
    if finished {
        clear_target();
        if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x1b) } >= 0
            && let Some((dest, slot)) = target
            && let Some(tab) = id(0)
        {
            transfer::send(tab, dest, slot);
        }
    } else {
        MOVING_SINGLE.set(true);
    }
}
pub(super) fn menu(hwnd: HWND, index: usize, pt: POINT) {
    let Some(id) = id(index) else { return };
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, 1, w!("移到新窗口"));
        let windows: Vec<_> = window_registry::alive_windows()
            .into_iter()
            .filter(|w| *w != hwnd && IsWindowVisible(*w).as_bool())
            .collect();
        for (i, other) in windows.iter().enumerate() {
            let mut title = [0u16; 128];
            let len = GetWindowTextW(*other, &mut title) as usize;
            let title = String::from_utf16_lossy(&title[..len]).replace('&', "&&");
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                i + 2,
                &HSTRING::from(format!("移到窗口 {} · {title}", i + 1)),
            );
        }
        let choice = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        )
        .0 as usize;
        let _ = DestroyMenu(menu);
        if choice == 1 {
            window::detach_live_tab(id, pt);
        } else if let Some(target) = choice.checked_sub(2).and_then(|i| windows.get(i)) {
            transfer::send(id, *target, usize::MAX);
        }
    }
}
