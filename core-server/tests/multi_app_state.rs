use core_server::ipc::protocol::{
    ControlMessage, DesktopWindowMode, MonitorKind, MonitorRequestStatus,
};
use core_server::ipc::server::ServerState;
use windows::Win32::Foundation::HANDLE;

fn current_pid() -> u32 {
    std::process::id()
}

fn register_app_with_canvas(state: &mut ServerState, logical_w: u32, logical_h: u32) -> (u32, u32) {
    let app_id = state
        .register_app(current_pid(), HANDLE::default())
        .unwrap();
    let canvas_id = state
        .create_canvas(app_id, logical_w, logical_h, logical_w, logical_h)
        .unwrap();
    (app_id, canvas_id)
}

fn register_shared_game_bar_monitor(state: &mut ServerState) -> u32 {
    let (monitor_id, _rx) = register_shared_game_bar_monitor_with_rx(state);
    monitor_id
}

fn register_shared_game_bar_monitor_with_rx(
    state: &mut ServerState,
) -> (u32, tokio::sync::mpsc::UnboundedReceiver<ControlMessage>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (monitor_id, should_close) = state.register_monitor(
        current_pid(),
        HANDLE::default(),
        tx,
        MonitorKind::GameBar,
        None,
        None,
        None,
        DesktopWindowMode::Borderless,
        0,
        true,
    );
    assert!(!should_close);
    (monitor_id, rx)
}

fn drain_control_messages(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ControlMessage>,
) -> Vec<ControlMessage> {
    let mut messages = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(msg) => messages.push(msg),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
        }
    }
    messages
}

#[test]
fn shared_manual_monitor_scene_accepts_multiple_app_layers() {
    let mut state = ServerState::new().unwrap();
    let (app_a, canvas_a) = register_app_with_canvas(&mut state, 1920, 1080);
    let (app_b, canvas_b) = register_app_with_canvas(&mut state, 1280, 720);
    let monitor_id = register_shared_game_bar_monitor(&mut state);

    let layer_a = state
        .add_canvas_layer_to_monitor_scene_for_app(app_a, canvas_a, monitor_id)
        .unwrap();
    let layer_b = state
        .add_canvas_layer_to_monitor_scene_for_app(app_b, canvas_b, monitor_id)
        .unwrap();

    let scene = state.monitor_scenes.get(&monitor_id).unwrap();
    assert_ne!(layer_a, layer_b);
    assert_eq!(scene.layers.len(), 2);
    assert_eq!(scene.layers[0].app_id, app_a);
    assert_eq!(scene.layers[0].canvas_id, canvas_a);
    assert_eq!(scene.layers[0].z_order, 1);
    assert_eq!(scene.layers[1].app_id, app_b);
    assert_eq!(scene.layers[1].canvas_id, canvas_b);
    assert_eq!(scene.layers[1].z_order, 2);
    assert!(scene.contains_app(app_a));
    assert!(scene.contains_app(app_b));
}

#[test]
fn owned_monitor_scene_rejects_other_app_canvas() {
    let mut state = ServerState::new().unwrap();
    let (app_a, _canvas_a) = register_app_with_canvas(&mut state, 1920, 1080);
    let (app_b, canvas_b) = register_app_with_canvas(&mut state, 1280, 720);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (monitor_id, should_close) = state.register_monitor(
        current_pid(),
        HANDLE::default(),
        tx,
        MonitorKind::DesktopWindow,
        Some(app_a),
        None,
        None,
        DesktopWindowMode::Bordered,
        0,
        true,
    );
    assert!(!should_close);

    let result = state.add_canvas_layer_to_monitor_scene_for_app(app_b, canvas_b, monitor_id);
    assert!(result.is_err());
    assert!(!state.monitor_scenes.contains_key(&monitor_id));
}

#[test]
fn removing_one_app_keeps_other_app_layers_on_same_scene() {
    let mut state = ServerState::new().unwrap();
    let (app_a, canvas_a) = register_app_with_canvas(&mut state, 1920, 1080);
    let (app_b, canvas_b) = register_app_with_canvas(&mut state, 1280, 720);
    let (monitor_id, _rx) = register_shared_game_bar_monitor_with_rx(&mut state);

    state
        .add_canvas_layer_to_monitor_scene_for_app(app_a, canvas_a, monitor_id)
        .unwrap();
    state
        .add_canvas_layer_to_monitor_scene_for_app(app_b, canvas_b, monitor_id)
        .unwrap();

    assert!(state
        .ensure_monitor_scene_output_and_notify(monitor_id)
        .unwrap());
    state.remove_app(app_a);

    let scene = state.monitor_scenes.get(&monitor_id).unwrap();
    assert_eq!(scene.layers.len(), 1);
    assert_eq!(scene.layers[0].app_id, app_b);
    assert_eq!(scene.layers[0].canvas_id, canvas_b);
    assert!(!state.canvases.contains_key(&canvas_a));
    assert!(state.canvases.contains_key(&canvas_b));
}

#[test]
fn removing_monitor_drops_scene_without_touching_canvases() {
    let mut state = ServerState::new().unwrap();
    let (app_a, canvas_a) = register_app_with_canvas(&mut state, 1920, 1080);
    let (app_b, canvas_b) = register_app_with_canvas(&mut state, 1280, 720);
    let monitor_id = register_shared_game_bar_monitor(&mut state);

    state
        .add_canvas_layer_to_monitor_scene_for_app(app_a, canvas_a, monitor_id)
        .unwrap();
    state
        .add_canvas_layer_to_monitor_scene_for_app(app_b, canvas_b, monitor_id)
        .unwrap();

    state.remove_monitor(monitor_id);

    assert!(!state.monitor_scenes.contains_key(&monitor_id));
    assert!(state.canvases.contains_key(&canvas_a));
    assert!(state.canvases.contains_key(&canvas_b));
    assert!(!state
        .canvases
        .get(&canvas_a)
        .unwrap()
        .attached_monitor_ids
        .contains(&monitor_id));
    assert!(!state
        .canvases
        .get(&canvas_b)
        .unwrap()
        .attached_monitor_ids
        .contains(&monitor_id));
}

#[test]
fn game_bar_start_joins_existing_single_widget_scene() {
    let mut state = ServerState::new().unwrap();
    let (app_a, canvas_a) = register_app_with_canvas(&mut state, 1920, 1080);
    let (app_b, canvas_b) = register_app_with_canvas(&mut state, 1280, 720);
    let (monitor_id, mut rx) = register_shared_game_bar_monitor_with_rx(&mut state);

    let (status_a, monitors_a) = state.attach_game_bar_monitor_for_app(app_a, canvas_a, 1);
    assert_eq!(status_a, MonitorRequestStatus::Ok);
    assert_eq!(monitors_a, vec![monitor_id]);
    let first_handoff_messages = drain_control_messages(&mut rx);
    assert_eq!(first_handoff_messages.len(), 3);
    assert!(matches!(
        first_handoff_messages[0],
        ControlMessage::CanvasAttached { canvas_id, .. } if canvas_id == canvas_a
    ));
    assert!(matches!(
        first_handoff_messages[1],
        ControlMessage::MonitorLocalSurfaceAttached { canvas_id, monitor_id: msg_monitor_id, .. }
            if canvas_id == canvas_a && msg_monitor_id == monitor_id
    ));
    assert!(matches!(
        first_handoff_messages[2],
        ControlMessage::MonitorCompositeAttached { monitor_id: msg_monitor_id, scene_id, .. }
            if msg_monitor_id == monitor_id && scene_id == monitor_id
    ));

    let (status_b, monitors_b) = state.attach_game_bar_monitor_for_app(app_b, canvas_b, 1);
    assert_eq!(status_b, MonitorRequestStatus::Ok);
    assert_eq!(monitors_b, vec![monitor_id]);
    assert!(drain_control_messages(&mut rx).is_empty());

    let scene = state.monitor_scenes.get(&monitor_id).unwrap();
    assert_eq!(scene.layers.len(), 2);
    assert_eq!(scene.layers[0].app_id, app_a);
    assert_eq!(scene.layers[0].canvas_id, canvas_a);
    assert_eq!(scene.layers[0].z_order, 1);
    assert_eq!(scene.layers[1].app_id, app_b);
    assert_eq!(scene.layers[1].canvas_id, canvas_b);
    assert_eq!(scene.layers[1].z_order, 2);
    assert!(state
        .canvases
        .get(&canvas_a)
        .unwrap()
        .attached_monitor_ids
        .contains(&monitor_id));
    assert!(state
        .canvases
        .get(&canvas_b)
        .unwrap()
        .attached_monitor_ids
        .contains(&monitor_id));
    assert!(!state
        .canvases
        .get(&canvas_b)
        .unwrap()
        .per_monitor_surfaces
        .contains_key(&monitor_id));
    assert!(state.monitor_scene_outputs.contains_key(&monitor_id));
}

#[test]
fn game_bar_start_requires_open_widget() {
    let mut state = ServerState::new().unwrap();
    let (app_id, canvas_id) = register_app_with_canvas(&mut state, 1920, 1080);

    let (status, monitor_ids) = state.attach_game_bar_monitor_for_app(app_id, canvas_id, 1);

    assert_eq!(status, MonitorRequestStatus::ManualOpenRequired);
    assert!(monitor_ids.is_empty());
    assert!(state.monitor_scenes.is_empty());
}

#[test]
fn ownerless_non_manual_monitor_scene_is_not_shared() {
    let mut state = ServerState::new().unwrap();
    let (app_id, canvas_id) = register_app_with_canvas(&mut state, 1920, 1080);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (monitor_id, should_close) = state.register_monitor(
        current_pid(),
        HANDLE::default(),
        tx,
        MonitorKind::GameBar,
        None,
        None,
        None,
        DesktopWindowMode::Borderless,
        0,
        false,
    );
    assert!(!should_close);

    let result = state.add_canvas_layer_to_monitor_scene_for_app(app_id, canvas_id, monitor_id);

    assert!(result.is_err());
    assert!(state.monitor_scenes.is_empty());
}

#[test]
fn rejoining_same_canvas_keeps_existing_layer_order() {
    let mut state = ServerState::new().unwrap();
    let (app_id, canvas_id) = register_app_with_canvas(&mut state, 1920, 1080);
    let monitor_id = register_shared_game_bar_monitor(&mut state);

    let first_layer = state
        .add_canvas_layer_to_monitor_scene_for_app(app_id, canvas_id, monitor_id)
        .unwrap();
    let second_layer = state
        .add_canvas_layer_to_monitor_scene_for_app(app_id, canvas_id, monitor_id)
        .unwrap();

    let scene = state.monitor_scenes.get(&monitor_id).unwrap();
    assert_eq!(first_layer, second_layer);
    assert_eq!(scene.layers.len(), 1);
    assert_eq!(scene.layers[0].z_order, 1);
}
