use core_server::ipc::protocol::{AppDetachReason, ControlMessage, DesktopWindowMode, MonitorKind};
use core_server::ipc::server::ServerState;
use core_server::server_task::broadcast_app_detached;

#[test]
fn app_detached_broadcast_hits_all_attached_monitors() {
    let mut state = ServerState::new().unwrap();

    let my_pid = std::process::id();
    let app_id = state
        .register_app(my_pid, windows::Win32::Foundation::HANDLE::default())
        .unwrap();

    let canvas1 = state.create_canvas(app_id, 1920, 1080, 1920, 1080).unwrap();
    let canvas2 = state.create_canvas(app_id, 1280, 720, 1280, 720).unwrap();

    let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
    let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
    let (tx3, mut rx3) = tokio::sync::mpsc::unbounded_channel();

    let (_mon1, should_close1) = state.register_monitor(
        my_pid,
        windows::Win32::Foundation::HANDLE::default(),
        tx1,
        MonitorKind::DesktopWindow,
        Some(app_id),
        None,
        Some(canvas1),
        DesktopWindowMode::Bordered,
        0,
        true,
    );
    let (mon2, should_close2) = state.register_monitor(
        my_pid,
        windows::Win32::Foundation::HANDLE::default(),
        tx2,
        MonitorKind::DesktopWindow,
        Some(app_id),
        None,
        Some(canvas1),
        DesktopWindowMode::Bordered,
        0,
        true,
    );
    let (_mon3, should_close3) = state.register_monitor(
        my_pid,
        windows::Win32::Foundation::HANDLE::default(),
        tx3,
        MonitorKind::DesktopWindow,
        Some(app_id),
        None,
        Some(canvas2),
        DesktopWindowMode::Bordered,
        0,
        true,
    );
    assert!(!should_close1 && !should_close2 && !should_close3);

    // Drop receiver for mon2 to simulate a closed connection.
    state.monitors.get_mut(&mon2).unwrap().tx = tokio::sync::mpsc::unbounded_channel().0;

    // Clear attach-time messages.
    while rx1.try_recv().is_ok() {}
    while rx3.try_recv().is_ok() {}

    // Broadcast
    broadcast_app_detached(&state, app_id, AppDetachReason::IoError);

    // Verify rx1 and rx3
    let msg1 = rx1.try_recv().expect("mon1 should receive AppDetached");
    assert!(matches!(msg1, ControlMessage::AppDetached { app_id: a, reason: 1 } if a == app_id));
    assert!(
        rx1.try_recv().is_err(),
        "mon1 should receive exactly ONE message"
    );

    let msg3 = rx3.try_recv().expect("mon3 should receive AppDetached");
    assert!(matches!(msg3, ControlMessage::AppDetached { app_id: a, reason: 1 } if a == app_id));
    assert!(
        rx3.try_recv().is_err(),
        "mon3 should receive exactly ONE message"
    );

    // The broadcast function should not panic or fail when mon2's tx fails to send.
}
