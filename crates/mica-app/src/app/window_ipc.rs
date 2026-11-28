//! Bounded worker-to-window mailboxes. UI threads never wait on one another.
use super::*;
use mica_core::ipc::{BadgeState, PaneBadge, PaneInfo, READ_TEXT_MAX_BYTES, ReadTextResult};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    LazyLock,
    atomic::{AtomicU8, AtomicU64, Ordering},
    mpsc,
};
use std::time::Instant;

static NEXT_PANE: AtomicU64 = AtomicU64::new(1);
static OWNERS: LazyLock<Mutex<HashMap<u64, isize>>> = LazyLock::new(Default::default);
static MAILBOXES: LazyLock<Mutex<HashMap<isize, VecDeque<Pending>>>> =
    LazyLock::new(Default::default);
static READY_WINDOWS: LazyLock<Mutex<HashSet<isize>>> = LazyLock::new(Default::default);
const MAILBOX_CAP: usize = 128;

pub(super) struct Pending {
    pub req: IpcRequest,
    reply: mpsc::SyncSender<IpcResponse>,
    state: Arc<AtomicU8>,
    deadline: Instant,
}
impl Pending {
    pub fn begin(&self) -> bool {
        Instant::now() < self.deadline
            && self
                .state
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }
    pub fn finish(self, response: IpcResponse) {
        let _ = self.reply.try_send(response);
    }
}
pub(super) fn next_pane_id() -> u64 {
    NEXT_PANE.fetch_add(1, Ordering::Relaxed)
}
pub(super) fn register(hwnd: HWND) {
    MAILBOXES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(hwnd.0 as isize)
        .or_default();
}
pub(super) fn mark_ready(hwnd: HWND) {
    READY_WINDOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(hwnd.0 as isize);
}
fn ready_windows() -> Vec<HWND> {
    let ready = READY_WINDOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    window_registry::alive_windows()
        .into_iter()
        .filter(|h| ready.contains(&(h.0 as isize)))
        .collect()
}
pub(super) fn unregister(hwnd: HWND) {
    READY_WINDOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(hwnd.0 as isize));
    MAILBOXES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(hwnd.0 as isize));
    OWNERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, owner| *owner != hwnd.0 as isize);
}
pub(super) fn set_owner(pane: u64, hwnd: HWND) {
    OWNERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pane, hwnd.0 as isize);
}
pub(super) fn pane_closed(pane_id: u64) {
    OWNERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&pane_id);
    events::publish(events::WatchEvent::PaneClosed { pane_id });
}
pub(super) fn pane_created(hwnd: HWND, tab_index: usize, pane: &PaneState) {
    set_owner(pane.id, hwnd);
    events::publish(events::WatchEvent::PaneCreated {
        window_id: hwnd.0 as u64,
        tab_index,
        pane_id: pane.id,
        pid: pane.terminal.session.child_pid(),
        cwd: pane.terminal.initial_cwd.clone(),
    });
}
fn call(hwnd: HWND, req: IpcRequest, deadline: Instant) -> IpcResponse {
    // Initialization is part of the global request budget. The per-window UI
    // deadline starts only once GPU/first-pane setup can process the mailbox.
    while !READY_WINDOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&(hwnd.0 as isize))
    {
        if !MAILBOXES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&(hwnd.0 as isize))
        {
            return IpcResponse::error(req.id, IpcError::NoWindow);
        }
        if Instant::now() >= deadline {
            return IpcResponse::error(req.id, IpcError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let deadline = deadline.min(Instant::now() + Duration::from_millis(1500));
    let id = req.id;
    let (reply, rx) = mpsc::sync_channel(1);
    let state = Arc::new(AtomicU8::new(0));
    {
        let mut boxes = MAILBOXES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(queue) = boxes.get_mut(&(hwnd.0 as isize)) else {
            return IpcResponse::error(id, IpcError::NoWindow);
        };
        queue.retain(|p| p.state.load(Ordering::Acquire) == 0 && Instant::now() < p.deadline);
        if queue.len() >= MAILBOX_CAP {
            return IpcResponse::error(id, IpcError::Busy);
        }
        queue.push_back(Pending {
            req,
            reply,
            state: state.clone(),
            deadline,
        });
    }
    if unsafe { PostMessageW(Some(hwnd), WM_APP_WINDOW_IPC, WPARAM(0), LPARAM(0)) }.is_err() {
        state.store(2, Ordering::Release);
        return IpcResponse::error(id, IpcError::NoWindow);
    }
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(response) => response,
        Err(mpsc::RecvTimeoutError::Disconnected)
            if !MAILBOXES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&(hwnd.0 as isize)) =>
        {
            IpcResponse::error(id, IpcError::NoWindow)
        }
        Err(_) => {
            let _ = state.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire);
            IpcResponse::error(id, IpcError::Timeout)
        }
    }
}
pub(super) fn dispatch(req: IpcRequest, deadline: Instant) -> IpcResponse {
    if req.pane_id.is_none() && req.tab != Some(-1) {
        while ready_windows().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if Instant::now() >= deadline {
            return IpcResponse::error(req.id, IpcError::Timeout);
        }
    }
    if req.op == "new-window" {
        return window::spawn_controlled_window(req, deadline);
    }
    if req.op == "list" {
        let mut panes = Vec::<serde_json::Value>::new();
        let mut windows = ready_windows();
        if let Some(qt) = quickterm::global_hwnd() {
            windows.push(qt);
        }
        for hwnd in windows {
            let response = call(hwnd, req.clone(), deadline);
            if response.status == mica_core::ipc::IpcStatus::Error {
                // A window may close after the registry snapshot. Its removed
                // mailbox means it contributes no panes to this enumeration.
                if response.detail.as_deref() == Some(IpcError::NoWindow.code()) {
                    continue;
                }
                return response;
            }
            if let Some(serde_json::Value::Array(values)) = response.data {
                panes.extend(values);
            }
        }
        return IpcResponse::applied_with_data(req.id, serde_json::Value::Array(panes));
    }
    let target = if let Some(pane) = req.pane_id {
        OWNERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pane)
            .copied()
            .map(|raw| HWND(raw as _))
    } else if req.tab == Some(-1) {
        quickterm::global_hwnd()
    } else {
        let windows = ready_windows();
        let foreground = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
        if req.tab.is_none()
            && matches!(
                req.op.as_str(),
                "focus" | "send-text" | "read-text" | "set-badge"
            )
            && quickterm::global_hwnd() == Some(foreground)
        {
            Some(foreground)
        } else {
            windows
                .iter()
                .copied()
                .find(|hwnd| *hwnd == foreground)
                .or_else(|| windows.first().copied())
        }
    };
    match target {
        Some(hwnd) => call(hwnd, req, deadline),
        None => IpcResponse::error(req.id, IpcError::BadTarget),
    }
}
pub(super) fn drain(hwnd: HWND) {
    loop {
        let pending = MAILBOXES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&(hwnd.0 as isize))
            .and_then(VecDeque::pop_front);
        let Some(pending) = pending else { break };
        if pending.begin() {
            let response = if quickterm::is_hwnd(hwnd) {
                quickterm::apply_ipc(hwnd, &pending.req)
            } else {
                apply(hwnd, &pending.req)
            };
            pending.finish(response);
        }
    }
}
pub(super) fn pane_info(
    hwnd: HWND,
    tab_index: usize,
    tab: &TabState,
    pane: &PaneState,
    qt: bool,
) -> PaneInfo {
    terminal_info(
        hwnd,
        tab_index,
        pane.id,
        &pane.terminal,
        &pane.terminal.observed_title,
        tab_index == ACTIVE.get(),
        tab.panes.get(tab.focused).is_some_and(|p| p.id == pane.id),
        qt,
    )
}
#[allow(clippy::too_many_arguments)]
pub(super) fn terminal_info(
    hwnd: HWND,
    tab_index: usize,
    pane_id: u64,
    t: &Terminal,
    title: &str,
    active: bool,
    focused: bool,
    qt: bool,
) -> PaneInfo {
    PaneInfo {
        window_id: hwnd.0 as u64,
        is_quick_terminal: qt,
        tab_index,
        pane_id,
        pid: t.session.child_pid(),
        title: title.into(),
        cwd: t
            .term
            .cwd()
            .map(str::to_owned)
            .or_else(|| t.initial_cwd.clone()),
        prompt_state: t.term.prompt_state(),
        is_active_tab: active,
        is_focused_pane: focused,
        exited: t.exited,
        badge: t.badge.clone(),
    }
}
fn target_position(tabs: &[TabState], req: &IpcRequest) -> Option<(usize, usize)> {
    if let Some(id) = req.pane_id {
        return tabs
            .iter()
            .enumerate()
            .find_map(|(i, tab)| tab.panes.iter().position(|p| p.id == id).map(|p| (i, p)));
    }
    let index = match req.tab {
        Some(n) if n >= 0 => n as usize,
        Some(_) => return None,
        None => ACTIVE.get(),
    };
    let tab = tabs.get(index)?;
    (tab.focused < tab.panes.len()).then_some((index, tab.focused))
}
pub(super) fn clear_attention(t: &mut Terminal) -> bool {
    if t.badge
        .as_ref()
        .is_some_and(|badge| badge.state == BadgeState::Attention && !badge.sticky)
    {
        t.badge = None;
        events::publish(events::WatchEvent::BadgeChanged {
            pane_id: t.pane_id,
            badge: None,
        });
        true
    } else {
        false
    }
}
pub(super) fn clear_active_attention() {
    TABS.with(|tabs| {
        if let Some(tab) = tabs.borrow_mut().get_mut(ACTIVE.get())
            && let Some(t) = tab.active_pane()
            && clear_attention(t)
        {
            tab.dirty = true;
        }
    });
}
pub(super) fn observe_output(t: &mut Terminal) -> Option<String> {
    if t.term.take_shell_mark_overflow() {
        events::disconnect_lagged();
    }
    let cwd = t
        .term
        .cwd()
        .map(str::to_owned)
        .or_else(|| t.initial_cwd.clone());
    if cwd != t.observed_cwd {
        if let Some(cwd) = &cwd {
            events::publish(events::WatchEvent::CwdChanged {
                pane_id: t.pane_id,
                cwd: cwd.clone(),
            });
        }
        t.observed_cwd = cwd;
    }
    use mica_core::ipc::PromptState;
    use mica_core::protocol::sidecar::ShellMark;
    for mark in t.term.take_shell_mark_events() {
        let prompt = match mark {
            ShellMark::OutputStart => (PromptState::Running, None),
            ShellMark::CommandEnd { exit } => (PromptState::Idle, exit),
            ShellMark::PromptStart | ShellMark::CommandStart => (PromptState::Idle, None),
        };
        if prompt != t.observed_prompt || matches!(mark, ShellMark::CommandEnd { .. }) {
            events::publish(events::WatchEvent::PromptStateChanged {
                pane_id: t.pane_id,
                state: prompt.0,
                exit_code: prompt.1,
            });
            t.observed_prompt = prompt;
        }
    }
    let state = t.term.prompt_state();
    if state != t.observed_prompt.0 {
        events::publish(events::WatchEvent::PromptStateChanged {
            pane_id: t.pane_id,
            state,
            exit_code: None,
        });
        t.observed_prompt = (state, None);
    }
    let title = t.term.take_title();
    if let Some(title) = &title
        && *title != t.observed_title
    {
        t.observed_title = title.clone();
        events::publish(events::WatchEvent::TitleChanged {
            pane_id: t.pane_id,
            title: title.clone(),
        });
    }
    title
}
pub(super) fn read_terminal(req: &IpcRequest, t: &mut Terminal) -> IpcResponse {
    let snapshot = t.term.dump_text(
        req.lines
            .unwrap_or(if req.include_scrollback == Some(true) {
                50
            } else {
                t.rows as usize
            }),
        req.include_scrollback.unwrap_or(false),
        READ_TEXT_MAX_BYTES,
    );
    ReadTextResult {
        pane_id: t.pane_id,
        cols: snapshot.cols,
        rows: snapshot.rows,
        text: snapshot.text,
        truncated: snapshot.truncated,
    }
    .into_response(req.id)
}
pub(super) fn badge_terminal(req: &IpcRequest, t: &mut Terminal) -> IpcResponse {
    let Some(state) = req.badge_state else {
        return IpcResponse::error(req.id, IpcError::BadTarget);
    };
    t.badge = if state == BadgeState::None {
        None
    } else {
        Some(PaneBadge::new(
            state,
            req.badge_text.clone(),
            req.badge_sticky.unwrap_or(false),
        ))
    };
    events::publish(events::WatchEvent::BadgeChanged {
        pane_id: t.pane_id,
        badge: t.badge.clone(),
    });
    IpcResponse::applied(req.id)
}
pub(super) fn flow_response(id: u64, flow: pty_bridge::InputFlow) -> IpcResponse {
    match flow {
        pty_bridge::InputFlow::Accepted => IpcResponse::applied(id),
        pty_bridge::InputFlow::Debt(_) => IpcResponse::queued(id),
        pty_bridge::InputFlow::RejectedOverflow => IpcResponse::error(id, IpcError::InputOverflow),
        pty_bridge::InputFlow::RejectedClosed => IpcResponse::error(id, IpcError::PtyWrite),
    }
}
pub(super) fn apply(hwnd: HWND, req: &IpcRequest) -> IpcResponse {
    if req.op == "new-tab" {
        return match unsafe { start_tab_with_options(hwnd, req) } {
            Ok(info) => {
                draw_frame();
                IpcResponse::applied_with_data(req.id, serde_json::to_value(info).unwrap())
            }
            Err(e) => IpcResponse::error(req.id, e),
        };
    }
    if req.op == "activate" {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::SW_RESTORE,
            );
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
        }
        return IpcResponse::applied(req.id);
    }
    if req.op == "list" {
        let panes: Vec<_> = TABS.with(|tabs| {
            tabs.borrow()
                .iter()
                .enumerate()
                .flat_map(|(i, tab)| {
                    tab.panes
                        .iter()
                        .map(move |p| pane_info(hwnd, i, tab, p, false))
                })
                .collect()
        });
        return IpcResponse::applied_with_data(req.id, serde_json::to_value(panes).unwrap());
    }
    let target = TABS.with(|tabs| target_position(&tabs.borrow(), req));
    let Some((index, pos)) = target else {
        return IpcResponse::error(req.id, IpcError::BadTarget);
    };
    if req.op == "focus" {
        TABS.with(|tabs| tabs.borrow_mut()[index].focused = pos);
        switch_tab(index);
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::SW_RESTORE,
            );
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
        }
        return IpcResponse::applied(req.id);
    }
    let response = TABS.with(|tabs| {
        let mut tabs = tabs.borrow_mut();
        let tab = &mut tabs[index];
        let t = &mut tab.panes[pos].terminal;
        match req.op.as_str() {
            "send-text" => {
                let bytes = req.text.as_deref().unwrap_or_default().as_bytes();
                let payload = if t.term.bracketed_paste_active() {
                    [b"\x1b[200~".as_slice(), bytes, b"\x1b[201~"].concat()
                } else {
                    bytes.to_vec()
                };
                flow_response(req.id, stage_input(t, &payload))
            }
            "read-text" => {
                drain_pending(t);
                if let Some(title) = observe_output(t) {
                    tab.title = pretty_title(&title);
                }
                read_terminal(req, t)
            }
            "set-badge" => {
                let response = badge_terminal(req, t);
                tab.dirty = true;
                response
            }
            _ => IpcResponse::error(req.id, IpcError::UnknownOp),
        }
    });
    if req.op == "set-badge" {
        draw_frame();
    }
    response
}
pub(super) fn drain_pending(t: &mut Terminal) {
    // Atomically capture only bytes already received. The backlog is bounded;
    // feeding chunks cannot chase a continuously producing child indefinitely.
    if let Some(bytes) = t.pty_buf.drain_batch(pty_bridge::PANE_BUDGET_BYTES) {
        t.output_pending = true;
        if let Some(raw) = OWNERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&t.pane_id)
            .copied()
        {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(raw as _)),
                    WM_APP_RENDER,
                    WPARAM(t.pane_id as usize),
                    LPARAM(0),
                );
            }
        }
        for chunk in bytes.chunks(pty_bridge::UI_BATCH_BYTES) {
            t.term.feed(chunk);
            for reply in t.term.take_pty_writes() {
                let _ = stage_input(t, reply.as_bytes());
            }
        }
        if t.term.display_offset() > 0 {
            t.force_full = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending(deadline: Instant) -> (Pending, Arc<AtomicU8>) {
        let (reply, _) = mpsc::sync_channel(1);
        let state = Arc::new(AtomicU8::new(0));
        (
            Pending {
                req: IpcRequest::new(1, "send-text"),
                reply,
                state: state.clone(),
                deadline,
            },
            state,
        )
    }
    #[test]
    fn deadline_and_cancelled_requests_never_begin_late() {
        let (expired, _) = pending(Instant::now() - Duration::from_millis(1));
        assert!(!expired.begin());
        let (cancelled, state) = pending(Instant::now() + Duration::from_secs(1));
        assert_eq!(
            state.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire),
            Ok(0)
        );
        assert!(!cancelled.begin());
    }
    #[test]
    fn request_begins_once_and_running_work_is_not_reclassified_pending() {
        let (request, state) = pending(Instant::now() + Duration::from_secs(1));
        assert!(request.begin());
        assert!(!request.begin());
        assert_eq!(
            state.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire),
            Err(1)
        );
    }
    #[test]
    fn transfer_owner_survives_source_unregister_and_close_removes_it() {
        let pane = next_pane_id();
        let source = HWND(usize::MAX as _);
        let dest = HWND((usize::MAX - 1) as _);
        register(source);
        register(dest);
        set_owner(pane, source);
        set_owner(pane, dest);
        unregister(source);
        assert_eq!(
            OWNERS.lock().unwrap().get(&pane).copied(),
            Some(dest.0 as isize)
        );
        pane_closed(pane);
        assert!(!OWNERS.lock().unwrap().contains_key(&pane));
        unregister(dest);
    }

    #[test]
    fn removing_a_window_replies_no_window_to_unclaimed_requests() {
        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!("mailbox test"),
                Default::default(),
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        register(hwnd);
        mark_ready(hwnd);
        let raw = hwnd.0 as isize;
        let worker = std::thread::spawn(move || {
            call(
                HWND(raw as _),
                IpcRequest::new(9, "list"),
                Instant::now() + Duration::from_secs(1),
            )
        });
        let deadline = Instant::now() + Duration::from_millis(800);
        while MAILBOXES
            .lock()
            .unwrap()
            .get(&raw)
            .is_some_and(VecDeque::is_empty)
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        unregister(hwnd);
        let response = worker.join().unwrap();
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        }
        assert_eq!(response.detail.as_deref(), Some("no_window"));
    }

    #[test]
    fn cold_window_bootstrap_uses_global_budget_before_window_call_budget() {
        unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            if msg == WM_APP_WINDOW_IPC {
                drain(hwnd);
                unsafe {
                    PostQuitMessage(0);
                }
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, w, l) }
            }
        }
        let (send, receive) = mpsc::sync_channel(1);
        let ui = std::thread::spawn(move || unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(proc),
                hInstance: instance,
                lpszClassName: w!("mica_ready_mailbox_test"),
                ..Default::default()
            };
            RegisterClassExW(&class);
            let hwnd = CreateWindowExW(
                Default::default(),
                w!("mica_ready_mailbox_test"),
                w!("ready test"),
                Default::default(),
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )
            .unwrap();
            register(hwnd);
            send.send(hwnd.0 as isize).unwrap();
            // Model GPU initialization after the HWND/mailbox are registered.
            std::thread::sleep(Duration::from_millis(1650));
            mark_ready(hwnd);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                DispatchMessageW(&msg);
            }
            unregister(hwnd);
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        });
        let hwnd = HWND(receive.recv_timeout(Duration::from_secs(1)).unwrap() as _);
        let start = Instant::now();
        let response = call(
            hwnd,
            IpcRequest::new(10, "list"),
            start + Duration::from_secs(3),
        );
        let elapsed = start.elapsed();
        // A failed call may never have posted the mailbox message. Always
        // unblock this fixture's message loop before joining its UI thread.
        unsafe {
            let thread =
                windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, None);
            if thread != 0 {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    thread,
                    windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        ui.join().unwrap();
        assert_eq!(
            response.status,
            mica_core::ipc::IpcStatus::Applied,
            "{response:?}"
        );
        assert!(elapsed >= Duration::from_millis(1650));
        assert!(elapsed < Duration::from_secs(3));
    }
}
