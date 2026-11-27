//! Independent, bounded event fanout. No pipe I/O runs on a UI thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use mica_core::ipc::{IpcError, PaneBadge, PromptState};
use serde::{Deserialize, Serialize};

#[path = "event_pipe.rs"]
mod event_pipe;
pub(crate) use event_pipe::{serve, watch};

pub(crate) const MAX_WATCHERS: usize = 8;
pub(crate) const WATCHER_QUEUE_CAP: usize = 128;
// Bound memory even when a program supplies pathological titles or cwd values.
const MAX_EVENT_TEXT_CHARS: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WatchEvent {
    PaneCreated {
        window_id: u64,
        tab_index: usize,
        pane_id: u64,
        pid: Option<u32>,
        cwd: Option<String>,
    },
    PaneClosed {
        pane_id: u64,
    },
    PaneExited {
        pane_id: u64,
        exit_code: u32,
    },
    CwdChanged {
        pane_id: u64,
        cwd: String,
    },
    PromptStateChanged {
        pane_id: u64,
        state: PromptState,
        exit_code: Option<i32>,
    },
    TitleChanged {
        pane_id: u64,
        title: String,
    },
    BadgeChanged {
        pane_id: u64,
        badge: Option<PaneBadge>,
    },
}

impl WatchEvent {
    fn pane_id(&self) -> u64 {
        match self {
            Self::PaneCreated { pane_id, .. }
            | Self::PaneClosed { pane_id }
            | Self::PaneExited { pane_id, .. }
            | Self::CwdChanged { pane_id, .. }
            | Self::PromptStateChanged { pane_id, .. }
            | Self::TitleChanged { pane_id, .. }
            | Self::BadgeChanged { pane_id, .. } => *pane_id,
        }
    }

    fn category(&self) -> &'static str {
        match self {
            Self::PaneCreated { .. } | Self::PaneClosed { .. } => "lifecycle",
            Self::PaneExited { .. } => "exit",
            Self::CwdChanged { .. } => "cwd",
            Self::PromptStateChanged { .. } => "prompt",
            Self::TitleChanged { .. } => "title",
            Self::BadgeChanged { .. } => "badge",
        }
    }

    fn bound_text(&mut self) {
        fn truncate(value: &mut String, limit: usize) {
            if let Some((offset, _)) = value.char_indices().nth(limit) {
                value.truncate(offset);
            }
        }
        match self {
            Self::PaneCreated {
                cwd: Some(text), ..
            }
            | Self::CwdChanged { cwd: text, .. }
            | Self::TitleChanged { title: text, .. } => truncate(text, MAX_EVENT_TEXT_CHARS),
            Self::BadgeChanged {
                badge: Some(PaneBadge {
                    text: Some(text), ..
                }),
                ..
            } => truncate(text, 32),
            _ => {}
        }
    }
}

struct Subscriber {
    id: u64,
    sender: SyncSender<Arc<String>>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct Bus {
    subscribers: Vec<Subscriber>,
    next_id: u64,
}

struct Subscription {
    receiver: Receiver<Arc<String>>,
    cancelled: Arc<AtomicBool>,
    bus: Weak<Mutex<Bus>>,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(bus) = self.bus.upgrade() {
            bus.lock()
                .unwrap_or_else(|e| e.into_inner())
                .subscribers
                .retain(|s| s.id != self.id);
        }
    }
}

impl Bus {
    fn subscribe(bus: &Arc<Mutex<Self>>) -> Option<Subscription> {
        let mut state = bus.lock().unwrap_or_else(|e| e.into_inner());
        if state.subscribers.len() == MAX_WATCHERS {
            return None;
        }
        let (sender, receiver) = mpsc::sync_channel(WATCHER_QUEUE_CAP);
        let cancelled = Arc::new(AtomicBool::new(false));
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        state.subscribers.push(Subscriber {
            id,
            sender,
            cancelled: cancelled.clone(),
        });
        Some(Subscription {
            receiver,
            cancelled,
            bus: Arc::downgrade(bus),
            id,
        })
    }

    fn publish(&mut self, line: Arc<String>) {
        self.subscribers
            .retain(|s| match s.sender.try_send(line.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    s.cancelled.store(true, Ordering::Release);
                    false
                }
            });
    }
}

fn bus() -> &'static Arc<Mutex<Bus>> {
    static BUS: OnceLock<Arc<Mutex<Bus>>> = OnceLock::new();
    BUS.get_or_init(|| Arc::new(Mutex::new(Bus::default())))
}

pub(crate) fn publish(mut event: WatchEvent) {
    event.bound_text();
    let Ok(mut line) = serde_json::to_string(&event) else {
        return;
    };
    line.push('\n');
    // Only eight try_send calls under this lock; no I/O or consumer work.
    bus()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .publish(Arc::new(line));
}

struct Filter<'a> {
    pane_id: Option<u64>,
    categories: Option<Vec<&'a str>>,
}

impl<'a> Filter<'a> {
    fn new(pane_id: Option<u64>, categories: Option<&'a str>) -> Result<Self, IpcError> {
        let categories = categories.map(|s| s.split(',').collect::<Vec<_>>());
        if categories.as_ref().is_some_and(|items| {
            items.iter().any(|s| {
                !matches!(
                    *s,
                    "exit" | "cwd" | "prompt" | "title" | "badge" | "lifecycle"
                )
            })
        }) {
            return Err(IpcError::BadTarget);
        }
        Ok(Self {
            pane_id,
            categories,
        })
    }

    fn matches(&self, event: &WatchEvent) -> bool {
        self.pane_id.is_none_or(|id| id == event.pane_id())
            && self
                .categories
                .as_ref()
                .is_none_or(|items| items.contains(&event.category()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_limit_and_drop_reclaim_slot() {
        let bus = Arc::new(Mutex::new(Bus::default()));
        let mut subscriptions = (0..MAX_WATCHERS)
            .map(|_| Bus::subscribe(&bus).unwrap())
            .collect::<Vec<_>>();
        assert!(Bus::subscribe(&bus).is_none());
        subscriptions.pop();
        assert!(Bus::subscribe(&bus).is_some());
    }

    #[test]
    fn saturated_watcher_is_cancelled_and_capacity_reclaimed() {
        let bus = Arc::new(Mutex::new(Bus::default()));
        let slow = Bus::subscribe(&bus).unwrap();
        let line = Arc::new("{}\n".to_owned());
        for _ in 0..WATCHER_QUEUE_CAP {
            bus.lock().unwrap().publish(line.clone());
        }
        let start = std::time::Instant::now();
        bus.lock().unwrap().publish(line);
        assert!(start.elapsed() < std::time::Duration::from_millis(50));
        assert!(slow.cancelled.load(Ordering::Acquire));
        assert!(bus.lock().unwrap().subscribers.is_empty());
        let replacements = (0..MAX_WATCHERS)
            .map(|_| Bus::subscribe(&bus).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(replacements.len(), MAX_WATCHERS);
    }

    #[test]
    fn event_fanout_shares_serialized_line_and_filter_matches() {
        let bus = Arc::new(Mutex::new(Bus::default()));
        let first = Bus::subscribe(&bus).unwrap();
        let second = Bus::subscribe(&bus).unwrap();
        bus.lock().unwrap().publish(Arc::new("{}\n".into()));
        assert!(Arc::ptr_eq(
            &first.receiver.recv().unwrap(),
            &second.receiver.recv().unwrap()
        ));
        let event = WatchEvent::PaneExited {
            pane_id: 42,
            exit_code: 0,
        };
        assert!(
            Filter::new(Some(42), Some("exit,cwd"))
                .unwrap()
                .matches(&event)
        );
        assert!(!Filter::new(Some(41), Some("exit")).unwrap().matches(&event));
        assert!(
            !Filter::new(None, Some("lifecycle"))
                .unwrap()
                .matches(&event)
        );
        assert!(Filter::new(None, Some("exit,")).is_err());
    }

    #[test]
    fn event_strings_are_bounded_and_ndjson_escaped() {
        let mut event = WatchEvent::TitleChanged {
            pane_id: 1,
            title: "\n界".repeat(MAX_EVENT_TEXT_CHARS),
        };
        event.bound_text();
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains('\n'));
        let WatchEvent::TitleChanged { title, .. } = event else {
            unreachable!()
        };
        assert_eq!(title.chars().count(), MAX_EVENT_TEXT_CHARS);
    }
}
