//! Window title formatting for the `desktop-window-monitor` monitor
//! (hotfix-visible-render spec — Change-B / 修 1.2).
//!
//! Factored out of `src/bin/monitor.rs` so the three attach states
//! — `Connecting`, `Attached { canvas_id, ml }`, `Reconnecting` — can be
//! unit-tested without standing up a real `HWND`, without pulling in the
//! Win32 `SetWindowTextW` call, and without any DComp / D3D state.
//!
//! The strings are exactly those prescribed by design.md §Fix Implementation
//! → Change-B:
//!
//!   * `Connecting`                              → `"Desktop Monitor - connecting..."`
//!   * `Attached { canvas_id, ml: false }`       → `"Desktop Monitor - canvas {id} (world only)"`
//!   * `Attached { canvas_id, ml: true }`        → `"Desktop Monitor - canvas {id} (world + monitor_local)"`
//!   * `Reconnecting`                            → `"Desktop Monitor - reconnecting..."`
//!
//! The `"Desktop Monitor - "` prefix is kept across every variant so
//! operators can still identify the window class by prefix regardless of
//! attach state.

/// Observable attach state of a `desktop-window-monitor` monitor window.
///
/// Drives [`format_window_title`]; three variants plus the `Connecting`
/// initial state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachState {
    /// Window created, `CanvasAttached` not yet received. Matches the
    /// initial title set at `CreateWindowExW` time.
    Connecting,
    /// `CanvasAttached` received. `ml == true` means the optional
    /// `MonitorLocalSurfaceAttached` follow-up was also observed and the
    /// dual-visual tree was mounted; `ml == false` means the World-only
    /// attach path was taken (older Core or `ml_info == None`).
    Attached { canvas_id: u32, ml: bool },
    /// A non-timeout I/O error was observed on the control-plane pipe
    /// after attach (design.md §Fix Implementation → Change-B, third
    /// call site).
    Reconnecting,
}

/// Format the monitor window title for the given [`AttachState`].
///
/// Pure: same input always produces the same `String`, never calls any
/// Win32 API, never allocates a shared `PCWSTR`. Encoding to
/// `Vec<u16>` and the `SetWindowTextW` dispatch live in
/// `src/bin/monitor.rs::set_window_title` so they can be swapped out
/// during test without a real `HWND`.
pub fn format_window_title(state: AttachState) -> String {
    match state {
        AttachState::Connecting => "Desktop Monitor - connecting...".to_string(),
        AttachState::Attached { canvas_id, ml: false } => {
            format!("Desktop Monitor - canvas {canvas_id} (world only)")
        }
        AttachState::Attached { canvas_id, ml: true } => {
            format!("Desktop Monitor - canvas {canvas_id} (world + monitor_local)")
        }
        AttachState::Reconnecting => "Desktop Monitor - reconnecting...".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- per-variant exact-string assertions -------------------------------

    #[test]
    fn connecting_is_exactly_the_initial_create_window_title() {
        // Must match the `w!("Desktop Monitor - connecting...")` literal
        // passed to `CreateWindowExW` in `monitor.rs` — otherwise the
        // startup title differs from what `format_window_title(Connecting)`
        // would produce, and downstream code paths that re-apply the
        // title on reconnect attempts would flicker.
        assert_eq!(
            format_window_title(AttachState::Connecting),
            "Desktop Monitor - connecting..."
        );
    }

    #[test]
    fn attached_world_only_contains_canvas_id_and_world_only_marker() {
        assert_eq!(
            format_window_title(AttachState::Attached { canvas_id: 7, ml: false }),
            "Desktop Monitor - canvas 7 (world only)"
        );
    }

    #[test]
    fn attached_dual_visual_contains_canvas_id_and_both_space_markers() {
        assert_eq!(
            format_window_title(AttachState::Attached { canvas_id: 42, ml: true }),
            "Desktop Monitor - canvas 42 (world + monitor_local)"
        );
    }

    #[test]
    fn reconnecting_has_its_own_distinct_suffix() {
        assert_eq!(
            format_window_title(AttachState::Reconnecting),
            "Desktop Monitor - reconnecting..."
        );
    }

    // ---- cross-cutting invariants (Property-style checks) ------------------

    #[test]
    fn every_variant_keeps_the_desktop_monitor_prefix() {
        let cases = [
            AttachState::Connecting,
            AttachState::Attached { canvas_id: 0, ml: false },
            AttachState::Attached { canvas_id: 1, ml: true },
            AttachState::Reconnecting,
        ];
        for state in cases {
            let s = format_window_title(state);
            assert!(
                s.starts_with("Desktop Monitor - "),
                "state={state:?} title={s:?} lost the common prefix"
            );
        }
    }

    #[test]
    fn attached_never_contains_connecting_or_reconnecting_substring() {
        // The whole point of Change-B: once `CanvasAttached` lands, the
        // title MUST drop the `"connecting..."` suffix; and it must never
        // accidentally claim `"reconnecting"` either. Sweep a few canvas
        // ids and both `ml` polarities.
        for ml in [false, true] {
            for canvas_id in [0u32, 1, 42, u32::MAX] {
                let s = format_window_title(AttachState::Attached { canvas_id, ml });
                assert!(
                    !s.contains("connecting..."),
                    "Attached title {s:?} still contains `connecting...`"
                );
                assert!(
                    !s.contains("reconnecting"),
                    "Attached title {s:?} leaks `reconnecting`"
                );
            }
        }
    }

    #[test]
    fn dual_visual_marker_only_appears_when_ml_is_true() {
        let world_only = format_window_title(
            AttachState::Attached { canvas_id: 1, ml: false },
        );
        let dual_visual = format_window_title(
            AttachState::Attached { canvas_id: 1, ml: true },
        );

        assert!(!world_only.contains("monitor_local"));
        assert!(dual_visual.contains("monitor_local"));
        assert!(dual_visual.contains("world +"));
    }

    #[test]
    fn connecting_and_reconnecting_are_distinguishable() {
        // If these ever collided, `isBugCondition_title` would be
        // unfalsifiable at runtime (the probe cannot tell `connecting...`
        // from `reconnecting...` apart on unfixed code).
        let c = format_window_title(AttachState::Connecting);
        let r = format_window_title(AttachState::Reconnecting);
        assert_ne!(c, r);
        assert!(c.contains("connecting..."));
        assert!(!c.contains("reconnecting"));
        assert!(r.contains("reconnecting..."));
    }
}
