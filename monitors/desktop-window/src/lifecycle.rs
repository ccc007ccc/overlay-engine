use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorLifecycleKind {
    Standalone,
    Hosted,
}

pub struct MonitorWindow {
    pub hwnd: isize, // Store as isize to avoid HWND thread-safety issues in pure tests
    pub monitor_id: u32,
    pub canvas_id: u32,
    pub owner_app_id: Option<u32>,
    pub pending_close: Arc<AtomicBool>,
    pub in_frame: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppDetachedEvent {
    pub app_id: u32,
    pub reason: u8,
}

pub fn apply_app_detached_events(windows: &mut [MonitorWindow], events: &[AppDetachedEvent]) {
    for event in events {
        for w in windows.iter_mut() {
            if w.owner_app_id == Some(event.app_id) || w.owner_app_id.is_none() {
                w.pending_close.store(true, Ordering::SeqCst);
            }
        }
    }
}

pub fn should_destroy_now(w: &MonitorWindow) -> bool {
    w.pending_close.load(Ordering::SeqCst) && !w.in_frame
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectOutcome {
    Success,
    Failed,
}

pub struct ReconnectState {
    pub windows: Vec<MonitorWindow>,
    pub attempts: u32,
}

pub fn reconnect_step(
    state: &mut ReconnectState,
    outcome: ReconnectOutcome,
    lifecycle_kind: MonitorLifecycleKind,
    max_attempts: u32,
) {
    match outcome {
        ReconnectOutcome::Success => {
            state.attempts = 0;
            // On success, windows are not pending_close
            for w in &mut state.windows {
                w.pending_close.store(false, Ordering::SeqCst);
            }
        }
        ReconnectOutcome::Failed => {
            state.attempts += 1;
            if state.attempts >= max_attempts {
                if lifecycle_kind == MonitorLifecycleKind::Standalone {
                    for w in &mut state.windows {
                        w.pending_close.store(true, Ordering::SeqCst);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn test_property_3_frame_boundary_safe_shutdown(
            in_frames in prop::collection::vec(any::<bool>(), 1..10),
            has_owners in prop::collection::vec(any::<bool>(), 1..10),
            events_len in 0..5usize
        ) {
            let mut windows = Vec::new();
            let n = std::cmp::min(in_frames.len(), has_owners.len());
            for i in 0..n {
                windows.push(MonitorWindow {
                    hwnd: i as isize,
                    monitor_id: i as u32,
                    canvas_id: i as u32,
                    owner_app_id: if has_owners[i] { Some(42) } else { None },
                    pending_close: Arc::new(AtomicBool::new(false)),
                    in_frame: in_frames[i],
                });
            }

            let mut events = Vec::new();
            for _ in 0..events_len {
                events.push(AppDetachedEvent { app_id: 42, reason: 0 });
            }

            apply_app_detached_events(&mut windows, &events);

            for w in &windows {
                let pending = w.pending_close.load(Ordering::SeqCst);
                if events_len > 0 {
                    assert!(pending); // Since owner is 42 or None, it should match
                } else {
                    assert!(!pending);
                }

                if w.in_frame {
                    assert!(!should_destroy_now(w));
                } else {
                    assert_eq!(should_destroy_now(w), pending);
                }
            }
        }

        #[test]
        fn test_property_4_reconnect_state_machine_terminal_state(
            outcomes in prop::collection::vec(prop_oneof![Just(ReconnectOutcome::Success), Just(ReconnectOutcome::Failed)], 1..20),
            kind in prop_oneof![Just(MonitorLifecycleKind::Standalone), Just(MonitorLifecycleKind::Hosted)]
        ) {
            let mut state = ReconnectState {
                windows: vec![
                    MonitorWindow {
                        hwnd: 1,
                        monitor_id: 1,
                        canvas_id: 1,
                        owner_app_id: None,
                        pending_close: Arc::new(AtomicBool::new(false)),
                        in_frame: false,
                    }
                ],
                attempts: 0,
            };

            let max_attempts = 10;
            let mut consecutive_failures = 0;

            for outcome in outcomes {
                reconnect_step(&mut state, outcome, kind, max_attempts);
                match outcome {
                    ReconnectOutcome::Success => {
                        consecutive_failures = 0;
                        for w in &state.windows {
                            assert!(!w.pending_close.load(Ordering::SeqCst));
                        }
                    }
                    ReconnectOutcome::Failed => {
                        consecutive_failures += 1;
                    }
                }
            }

            if consecutive_failures >= max_attempts && kind == MonitorLifecycleKind::Standalone {
                for w in &state.windows {
                    assert!(w.pending_close.load(Ordering::SeqCst));
                }
            } else if consecutive_failures < max_attempts || kind == MonitorLifecycleKind::Hosted {
                for w in &state.windows {
                    assert!(!w.pending_close.load(Ordering::SeqCst));
                }
            }
        }
    }
}
