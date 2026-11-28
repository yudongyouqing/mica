//! Transfer only session-owned state. DirectWrite objects never leave their UI thread.
use super::*;
use std::collections::HashMap;
use std::sync::{
    LazyLock,
    atomic::{AtomicUsize, Ordering},
};
use windows::Win32::UI::WindowsAndMessaging::{SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW};

impl<R> TabState<R> {
    fn map_routers<T>(self, mut make: impl FnMut(R) -> T) -> TabState<T> {
        let panes = self
            .panes
            .into_iter()
            .map(|p| {
                let Terminal {
                    term,
                    session,
                    session_permit,
                    pty_buf,
                    writeq,
                    input_stage,
                    session_gen,
                    router,
                    metrics,
                    palette,
                    cols,
                    rows,
                    shell,
                    exited,
                    strip_router: _,
                    row_insts: _,
                    last_atlas_ver: _,
                    force_full: _,
                } = p.terminal;
                PaneState {
                    id: p.id,
                    terminal: Terminal {
                        term,
                        session,
                        session_permit,
                        pty_buf,
                        writeq,
                        input_stage,
                        session_gen,
                        router: make(router),
                        metrics,
                        palette,
                        cols,
                        rows,
                        shell,
                        exited,
                        strip_router: None,
                        last_atlas_ver: 0,
                        row_insts: Vec::new(),
                        force_full: true,
                    },
                }
            })
            .collect();
        TabState {
            title: self.title,
            color: self.color,
            focused: self.focused,
            layout: self.layout,
            dirty: true,
            panes,
        }
    }
}

struct Offer {
    dest: isize,
    fonts: Vec<(f32, Vec<String>)>,
    tab: TabState<()>,
}

static OFFERS: LazyLock<Mutex<HashMap<usize, Offer>>> = LazyLock::new(Default::default);

pub(super) fn send(id: u64, dest: HWND, slot: usize) -> bool {
    let source = SHARED_HWND
        .with(|s| s.borrow().clone())
        .and_then(|s| *s.lock().unwrap());
    if source == Some(dest.0 as isize) || !window_registry::alive_windows().contains(&dest) {
        return false;
    }
    let old_active = ACTIVE.get();
    let Some((index, tab)) = TABS.with(|tabs| {
        let mut tabs = tabs.borrow_mut();
        let index = tabs
            .iter()
            .position(|t| t.panes.first().is_some_and(|p| p.id == id))?;
        Some((index, tabs.remove(index)))
    }) else {
        return false;
    };
    let mut previous = Vec::new();
    let mut fonts = Vec::new();
    let tab = tab.map_routers(|r| {
        fonts.push((
            r.font_size_pt() / DPI_SCALE.with(std::cell::Cell::get),
            r.families_in_use(),
        ));
        previous.push(r);
    });
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    OFFERS.lock().unwrap().insert(
        token,
        Offer {
            dest: dest.0 as isize,
            fonts,
            tab,
        },
    );
    // Block reentrant window messages while this tab is in transit. The receiver
    // claims the offer only after all fallible allocations have succeeded.
    unsafe {
        SendMessageTimeoutW(
            dest,
            WM_APP_TRANSFER,
            WPARAM(token),
            LPARAM(slot.min(isize::MAX as usize) as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            5000,
            None,
        )
    };
    let remaining = OFFERS.lock().unwrap().remove(&token);
    if let Some(offer) = remaining {
        let mut routers = previous.into_iter();
        let tab = offer.tab.map_routers(|()| routers.next().unwrap());
        TABS.with(|tabs| tabs.borrow_mut().insert(index, tab));
        draw_frame();
        return false;
    }
    let n = TABS.with(|t| t.borrow().len());
    TAB_COUNT.set(n);
    SEARCH_OPEN.set(false);
    RENAME_OPEN.set(false);
    if n == 0 {
        if let Some(raw) = source {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(raw as _)),
                    WM_APP_CLOSE_EMPTY,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    } else {
        switch_tab((old_active - usize::from(index < old_active)).min(n - 1));
    }
    unsafe {
        if windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(dest).as_bool() {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(dest);
        }
    }
    true
}

pub(super) fn receive(hwnd: HWND, token: usize, index: usize) {
    // Sent window messages can be delivered inside a Win32 call on this same thread.
    // Leave the offer with the sender if the destination is mid-frame or mid-update.
    if !TABS.with(|t| t.try_borrow_mut().is_ok()) || !GPU.with(|g| g.try_borrow_mut().is_ok()) {
        return;
    }
    let fonts = {
        let offers = OFFERS.lock().unwrap();
        let Some(offer) = offers.get(&token).filter(|o| o.dest == hwnd.0 as isize) else {
            return;
        };
        offer.fonts.clone()
    };
    let mut routers = Vec::new();
    for (pt, families) in fonts {
        let families: Vec<_> = families.iter().map(String::as_str).collect();
        match DwriteRouter::new(scaled_pt(pt), &families) {
            Ok(r) => routers.push(r),
            Err(e) => {
                eprintln!("tab transfer: {e}");
                return;
            }
        }
    }
    let Some(owner) = SHARED_HWND.with(|s| s.borrow().clone()) else {
        return;
    };
    let Some(offer) = OFFERS.lock().unwrap().remove(&token) else {
        return;
    };
    let mut routers = routers.into_iter();
    let mut tab = offer.tab.map_routers(|()| routers.next().unwrap());
    for p in &mut tab.panes {
        let t = &mut p.terminal;
        t.metrics = t.router.metrics();
        t.term.set_cell_metrics(
            t.metrics.cell_width.round() as u16,
            t.metrics.line_height.round() as u16,
        );
        t.palette = current_palette();
        t.term.set_palette(&t.palette);
        t.pty_buf.move_wake(owner.clone());
    }
    let idx = TABS.with(|tabs| {
        let mut tabs = tabs.borrow_mut();
        let idx = index.min(tabs.len());
        tabs.insert(idx, tab);
        TAB_COUNT.set(tabs.len());
        idx
    });
    SEARCH_OPEN.set(false);
    RENAME_OPEN.set(false);
    switch_tab(idx);
    flush_all_panes();
}
