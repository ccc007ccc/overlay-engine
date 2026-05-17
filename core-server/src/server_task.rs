use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use windows::core::Interface;
use windows::Win32::Graphics::CompositionSwapchain::{
    IPresentationBuffer, IPresentationManager, IPresentationSurface,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Resource, ID3D11Texture2D, D3D11_BOX,
};

use crate::ipc::cmd_decoder::{BitmapDrawCommand, RenderCommand, SpaceId};
use crate::ipc::protocol::{
    ControlMessage, MessageHeader, MonitorKind, MonitorRequestStatus, HEADER_SIZE,
};
use crate::renderer::dcomp::{present_manager, AcquireOutcome, PresentOutcome, ACQUIRE_TIMEOUT_MS};
use crate::renderer::painter::{D2DEngine, DrawCmd};

pub const PIPE_NAME: &str = r"\\.\pipe\overlay-core";

/// Rolling-average window for per-frame render-duration monitoring. Task 3.4
/// of the `animation-and-viewport-fix` spec requires a log metric so the
/// cost of replaying MonitorLocal commands onto N per-Monitor targets is
/// observable (design.md §Fix Implementation → Change 7 note on
/// per-Monitor cost).
const RENDER_DURATION_WINDOW: usize = 60;

/// Warning threshold for the rolling average. 8ms is the design-document
/// soft ceiling: anything above that means the submit path is starting to
/// eat into a 120Hz app's budget. Crossing the threshold emits a warn
/// log (not a crash / backpressure) — the submit path still serves frames,
/// but operators are alerted.
const RENDER_DURATION_WARN_MS: u128 = 8;
const MONITOR_START_TIMEOUT: Duration = Duration::from_secs(5);

type PendingMonitorStartKey = (u32, u32);

struct PendingMonitorStart {
    app_id: u32,
    requested_count: u32,
    monitor_ids: Vec<u32>,
    tx: tokio::sync::mpsc::UnboundedSender<ControlMessage>,
}

lazy_static::lazy_static! {
    static ref PENDING_MONITOR_STARTS: Mutex<HashMap<PendingMonitorStartKey, PendingMonitorStart>> = Mutex::new(HashMap::new());
}

pub async fn run_server() -> anyhow::Result<()> {
    use std::ffi::CString;
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorA, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    // SDDL meaning:
    // D: (Discretionary ACL)
    // (A;;GA;;;WD) -> Allow, Generic All, to Everyone (WD)
    // (A;;GA;;;AC) -> Allow, Generic All, to All Application Packages (UWP Sandbox) (AC)
    // This allows UWP apps (like Xbox Game Bar widgets) to connect to this named pipe.
    let sddl = CString::new("D:(A;;GA;;;WD)(A;;GA;;;AC)").unwrap();
    let mut sd: PSECURITY_DESCRIPTOR = PSECURITY_DESCRIPTOR::default();

    unsafe {
        let _ = ConvertStringSecurityDescriptorToSecurityDescriptorA(
            windows::core::PCSTR(sddl.as_ptr() as *const u8),
            SDDL_REVISION_1,
            &mut sd,
            None,
        );
    }

    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0 as *mut _,
        bInheritHandle: false.into(),
    };

    let mut server_options = ServerOptions::new();
    let mut server = unsafe {
        server_options
            .first_pipe_instance(true)
            .create_with_security_attributes_raw(
                PIPE_NAME,
                &mut sa as *mut _ as *mut std::ffi::c_void,
            )?
    };

    // Free the security descriptor buffer allocated by Windows
    unsafe {
        if !sd.0.is_null() {
            windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(sd.0)));
        }
    }

    println!("Core Server listening on {}", PIPE_NAME);

    loop {
        // Wait for a client to connect
        server.connect().await?;
        println!("Client connected");

        let connected_client = server;

        // Prepare a new pipe instance for the next client
        // Need to recreate the security attributes for the next pipe instance
        let mut sd_next: PSECURITY_DESCRIPTOR = PSECURITY_DESCRIPTOR::default();
        unsafe {
            let _ = ConvertStringSecurityDescriptorToSecurityDescriptorA(
                windows::core::PCSTR(sddl.as_ptr() as *const u8),
                SDDL_REVISION_1,
                &mut sd_next,
                None,
            );
        }
        let mut sa_next = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd_next.0 as *mut _,
            bInheritHandle: false.into(),
        };

        let next_server_options = ServerOptions::new();
        server = unsafe {
            next_server_options.create_with_security_attributes_raw(
                PIPE_NAME,
                &mut sa_next as *mut _ as *mut std::ffi::c_void,
            )?
        };

        unsafe {
            if !sd_next.0.is_null() {
                windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
                    sd_next.0,
                )));
            }
        }

        // Spawn a new task to handle the connected client
        tokio::spawn(async move {
            if let Err(e) = handle_client(connected_client).await {
                eprintln!("Client error: {:#}", e);
            }
        });
    }
}

/// Per-frame targeting derived from the decoded `RenderCommand` stream.
///
/// Filled by `scan_targets`: walks the stream once with an auxiliary space
/// stack and records whether a geometry command was ever dispatched against
/// the World target (`world_used`) or against MonitorLocal (`local_used`).
/// The dispatcher uses these flags to avoid acquiring buffers for targets
/// that will never be written — important for preserving the existing
/// World-only pixel output path (streams without `PUSH_SPACE` skip
/// per-Monitor acquires entirely, so no extra GPU work happens for pre-fix
/// apps).
struct TargetUsage {
    world_used: bool,
    local_used: bool,
}

fn scan_targets(cmds: &[RenderCommand]) -> TargetUsage {
    let mut stack: Vec<SpaceId> = Vec::new();
    let mut world_used = false;
    let mut local_used = false;
    for cmd in cmds {
        match cmd {
            RenderCommand::PushSpace(s) => stack.push(*s),
            RenderCommand::PopSpace => {
                let _ = stack.pop();
            }
            RenderCommand::Clear(_) | RenderCommand::Draw(_) | RenderCommand::DrawBitmap(_) => {
                // Default space is World (empty stack or bottom-of-stack),
                // per design.md §Fix Implementation → Change 6 and
                // Preservation 3.6 / Bugfix 2.6. Only an explicit
                // MonitorLocal top-of-stack routes to per-Monitor targets.
                match stack.last() {
                    Some(SpaceId::MonitorLocal) => local_used = true,
                    _ => world_used = true,
                }
            }
        }
    }
    TargetUsage {
        world_used,
        local_used,
    }
}

struct RenderTargetSnapshot {
    render_w: u32,
    render_h: u32,
    rtv: ID3D11RenderTargetView,
    buffer: IPresentationBuffer,
    texture: ID3D11Texture2D,
    surface: IPresentationSurface,
    manager: IPresentationManager,
}

struct SubmitFrameTargets {
    world: Option<RenderTargetSnapshot>,
    locals: HashMap<u32, RenderTargetSnapshot>,
}

fn snapshot_canvas_target(
    resources: &crate::renderer::dcomp::CanvasResources,
    idx: usize,
) -> Option<RenderTargetSnapshot> {
    Some(RenderTargetSnapshot {
        render_w: resources.render_w,
        render_h: resources.render_h,
        rtv: resources.rtvs.get(idx)?.clone(),
        buffer: resources.buffers.get(idx)?.clone(),
        texture: resources.textures.get(idx)?.clone(),
        surface: resources.surface.clone(),
        manager: resources.manager.clone(),
    })
}

fn snapshot_monitor_target(
    resources: &crate::renderer::dcomp::PerMonitorResources,
    idx: usize,
) -> Option<RenderTargetSnapshot> {
    Some(RenderTargetSnapshot {
        render_w: resources.render_w,
        render_h: resources.render_h,
        rtv: resources.rtvs.get(idx)?.clone(),
        buffer: resources.buffers.get(idx)?.clone(),
        texture: resources.textures.get(idx)?.clone(),
        surface: resources.surface.clone(),
        manager: resources.manager.clone(),
    })
}

fn snapshot_submit_frame_targets(
    canvas: &crate::ipc::server::Canvas,
    canvas_id: u32,
    frame_id: u64,
    usage: &TargetUsage,
) -> SubmitFrameTargets {
    let world = if usage.world_used {
        match canvas
            .resources
            .acquire_available_buffer(ACQUIRE_TIMEOUT_MS)
        {
            AcquireOutcome::Acquired(i) => snapshot_canvas_target(&canvas.resources, i),
            AcquireOutcome::TimedOut => None,
            AcquireOutcome::Failed(e) => {
                eprintln!(
                    "SubmitFrame: canvas={} frame={} World — acquire failed: {}",
                    canvas_id, frame_id, e
                );
                None
            }
        }
    } else {
        None
    };

    let mut locals = HashMap::new();
    if usage.local_used {
        for (monitor_id, resources) in &canvas.per_monitor_surfaces {
            match resources.acquire_available_buffer(ACQUIRE_TIMEOUT_MS) {
                AcquireOutcome::Acquired(i) => {
                    if let Some(target) = snapshot_monitor_target(resources, i) {
                        locals.insert(*monitor_id, target);
                    }
                }
                AcquireOutcome::TimedOut => {}
                AcquireOutcome::Failed(e) => {
                    eprintln!(
                        "SubmitFrame: canvas={} frame={} monitor={} MonitorLocal \
                         — acquire failed: {}",
                        canvas_id, frame_id, monitor_id, e
                    );
                }
            }
        }
    }

    SubmitFrameTargets { world, locals }
}

/// Mirror of the original inline `FILL_RECT` → `UpdateSubresource` path
/// (design.md §Preservation 3.6: World-only pixel output byte-for-byte
/// unchanged). Hoisted to a helper so the dispatcher loop can call the same
/// code against World textures and against any per-Monitor surface without
/// duplicating clamp + BGRA-packing logic.
///
/// The clamp / byte packing / `D3D11_BOX` layout exactly matches the
/// pre-task-3.4 code in this file — PBT B (World-only pixel equivalence)
/// hashes the software model of this logic, so any change here is a
/// preservation regression.
fn fill_rect_on_target(
    ctx: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    rw: u32,
    rh: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    rgba: [f32; 4],
) {
    let x0 = (x as u32).min(rw);
    let y0 = (y as u32).min(rh);
    let x1 = ((x + w) as u32).min(rw);
    let y1 = ((y + h) as u32).min(rh);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let bw = x1 - x0;
    let bh = y1 - y0;
    let b = (rgba[2].clamp(0.0, 1.0) * 255.0) as u8;
    let g = (rgba[1].clamp(0.0, 1.0) * 255.0) as u8;
    let r = (rgba[0].clamp(0.0, 1.0) * 255.0) as u8;
    let a = (rgba[3].clamp(0.0, 1.0) * 255.0) as u8;

    // PERF: Instead of allocating a new Vec and zeroing it per draw call,
    // we allocate once with capacity and push the bytes.
    let total_bytes = (bw * bh * 4) as usize;
    let mut pixels = Vec::with_capacity(total_bytes);

    // Fast path: if the buffer is small enough or we are pushing uniform bytes,
    // we can avoid extending and mutating.
    let pixel = [b, g, r, a];
    for _ in 0..(bw * bh) {
        pixels.extend_from_slice(&pixel);
    }

    if let Ok(resource) = texture.cast::<ID3D11Resource>() {
        let d3d_box = D3D11_BOX {
            left: x0,
            top: y0,
            front: 0,
            right: x1,
            bottom: y1,
            back: 1,
        };
        unsafe {
            ctx.UpdateSubresource(
                &resource,
                0,
                Some(&d3d_box),
                pixels.as_ptr() as *const _,
                bw * 4,
                bw * bh * 4,
            );
        }
    }
}

fn draw_command_on_target(
    d2d: &D2DEngine,
    texture: &ID3D11Texture2D,
    rw: u32,
    rh: u32,
    cmd: &DrawCmd,
) {
    let Ok(bitmap) = d2d.create_target_bitmap(texture) else {
        return;
    };
    unsafe {
        d2d.dc.SetTarget(&bitmap);
        d2d.dc.BeginDraw();
        d2d.dc
            .SetTransform(&windows::Foundation::Numerics::Matrix3x2 {
                M11: 1.0,
                M12: 0.0,
                M21: 0.0,
                M22: 1.0,
                M31: 0.0,
                M32: 0.0,
            });
    }
    let mut painter = crate::renderer::painter::Painter::new(d2d, (rw, rh));
    painter.execute(cmd);
    unsafe {
        let _ = d2d.dc.EndDraw(None, None);
        d2d.dc.SetTarget(None);
    }
}

fn draw_command_for_space(
    d2d: &D2DEngine,
    world_target: Option<&RenderTargetSnapshot>,
    local_targets: &HashMap<u32, RenderTargetSnapshot>,
    stack: &[SpaceId],
    cmd: &DrawCmd,
) {
    match stack.last() {
        Some(SpaceId::MonitorLocal) => {
            for target in local_targets.values() {
                draw_command_on_target(d2d, &target.texture, target.render_w, target.render_h, cmd);
            }
        }
        _ => {
            if let Some(target) = world_target {
                draw_command_on_target(d2d, &target.texture, target.render_w, target.render_h, cmd);
            }
        }
    }
}

fn draw_bitmap_command(draw: &BitmapDrawCommand, bitmap: u32) -> DrawCmd {
    DrawCmd::DrawBitmap {
        bitmap,
        src_x: draw.src_x,
        src_y: draw.src_y,
        src_w: draw.src_w,
        src_h: draw.src_h,
        dst_x: draw.dst_x,
        dst_y: draw.dst_y,
        dst_w: draw.dst_w,
        dst_h: draw.dst_h,
        opacity: draw.opacity,
        interp_mode: draw.interp_mode,
    }
}

/// Record a new per-frame render duration into the rolling window and
/// compute the current average. Returns `(avg, warn)` where `warn` is true
/// iff the window is full AND the average has crossed
/// `RENDER_DURATION_WARN_MS`. The window-full check keeps the first N-1
/// frames after startup from spuriously tripping the threshold while the
/// average is still dominated by warm-up outliers.
fn record_render_duration(
    durations: &mut VecDeque<Duration>,
    sample: Duration,
) -> (Duration, bool) {
    durations.push_back(sample);
    while durations.len() > RENDER_DURATION_WINDOW {
        durations.pop_front();
    }
    if durations.is_empty() {
        return (Duration::ZERO, false);
    }

    // Although an O(N) sum here is technically an optimization target, N=60
    // is so small that vectorization and CPU cache locality makes iter().sum()
    // practically as fast as tracking a running total, while avoiding
    // floating-point drift or complex state management across resets.
    let total_nanos: u128 = durations.iter().map(|d| d.as_nanos()).sum();
    let avg_nanos = total_nanos / durations.len() as u128;
    let avg = Duration::from_nanos(avg_nanos as u64);
    let warn =
        durations.len() == RENDER_DURATION_WINDOW && avg.as_millis() > RENDER_DURATION_WARN_MS;
    (avg, warn)
}

pub fn broadcast_app_detached(
    state: &crate::ipc::server::ServerState,
    app_id: u32,
    reason: crate::ipc::protocol::AppDetachReason,
) {
    if let Some(app) = state.apps.get(&app_id) {
        let msg = crate::ipc::protocol::ControlMessage::AppDetached {
            app_id,
            reason: reason as u8,
        };
        let mut notified = std::collections::HashSet::new();
        for cid in &app.canvas_ids {
            if let Some(canvas) = state.canvases.get(cid) {
                for mid in &canvas.attached_monitor_ids {
                    if notified.insert(*mid) {
                        if let Some(monitor) = state.monitors.get(mid) {
                            if let Err(e) = monitor.tx.send(msg.clone()) {
                                eprintln!(
                                    "[server_task] broadcast AppDetached to monitor {} failed: {}",
                                    mid, e
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

fn send_start_monitor_result(
    tx: &tokio::sync::mpsc::UnboundedSender<ControlMessage>,
    request_id: u32,
    status: MonitorRequestStatus,
    monitor_ids: Vec<u32>,
) {
    let _ = tx.send(ControlMessage::StartMonitorResult {
        request_id,
        status,
        monitor_ids,
    });
}

fn complete_pending_monitor_start(request_id: u32, app_id: u32, monitor_id: u32) {
    let key = (app_id, request_id);
    let completed = {
        let mut pending = PENDING_MONITOR_STARTS.lock().unwrap();
        let Some(entry) = pending.get_mut(&key) else {
            return;
        };
        entry.monitor_ids.push(monitor_id);
        if entry.monitor_ids.len() as u32 >= entry.requested_count {
            let tx = entry.tx.clone();
            let monitor_ids = entry.monitor_ids.clone();
            pending.remove(&key);
            Some((tx, monitor_ids))
        } else {
            None
        }
    };

    if let Some((tx, monitor_ids)) = completed {
        send_start_monitor_result(&tx, request_id, MonitorRequestStatus::Ok, monitor_ids);
    }
}

fn spawn_pending_monitor_timeout(app_id: u32, request_id: u32) {
    tokio::spawn(async move {
        tokio::time::sleep(MONITOR_START_TIMEOUT).await;
        let timed_out = {
            let mut pending = PENDING_MONITOR_STARTS.lock().unwrap();
            pending
                .remove(&(app_id, request_id))
                .map(|entry| (entry.tx, entry.monitor_ids))
        };
        if let Some((tx, monitor_ids)) = timed_out {
            send_start_monitor_result(&tx, request_id, MonitorRequestStatus::Timeout, monitor_ids);
        }
    });
}

fn cancel_pending_monitor_starts_for_app(app_id: u32) {
    let mut pending = PENDING_MONITOR_STARTS.lock().unwrap();
    pending.retain(|_, entry| entry.app_id != app_id);
}

fn send_close_monitor(tx: &tokio::sync::mpsc::UnboundedSender<ControlMessage>, monitor_id: u32) {
    let _ = tx.send(ControlMessage::CloseMonitor { monitor_id });
}

async fn handle_client(pipe: NamedPipeServer) -> anyhow::Result<()> {
    let (mut rh, mut wh) = tokio::io::split(pipe);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ControlMessage>();

    let mut client_id: Option<(u32, bool)> = None; // (id, is_app)
    let mut detach_reason = crate::ipc::protocol::AppDetachReason::GracefulExit;

    // Spawn writer task
    let _writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let mut buf = BytesMut::new();
            msg.encode(&mut buf);
            if let Err(e) = wh.write_all(&buf).await {
                eprintln!("Write error: {}", e);
                break;
            }
        }
    });

    let mut buf = BytesMut::with_capacity(1024);

    // Per-client rolling render-duration window. Local to `handle_client`
    // so each connected app has its own metric — avoids cross-talk
    // between clients. Size is bounded (`RENDER_DURATION_WINDOW`), satisfies
    // Preservation 3.8 "no unbounded queueing".
    let mut render_durations: VecDeque<Duration> = VecDeque::with_capacity(RENDER_DURATION_WINDOW);

    let res: anyhow::Result<()> = async {
        loop {
            // Read header
            let mut header_buf = [0u8; HEADER_SIZE];
            let bytes_read = match rh.read_exact(&mut header_buf).await {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
                Err(e) => return Err(e.into()),
            };

            if bytes_read == 0 {
                println!("Client disconnected");
                break;
            }

            buf.extend_from_slice(&header_buf);
            let header = MessageHeader::decode(&mut buf)?;

            // Read payload
            let mut payload_buf = vec![0u8; header.payload_len as usize];
            if header.payload_len > 0 {
                rh.read_exact(&mut payload_buf).await?;
                buf.extend_from_slice(&payload_buf);
            }

            let msg = match ControlMessage::decode(header.opcode, header.payload_len, &mut buf)? {
                Some(m) => m,
                None => {
                    // Unknown opcode (task 3.3 backward-compat downgrade): the
                    // decoder skipped the advertised payload and logged a warn.
                    // We continue the IPC read loop rather than tearing down the
                    // client, preserving forward compatibility with future Core
                    // opcodes.
                    continue;
                }
            };
            // Process message
            match msg {
                ControlMessage::RegisterApp { pid } => {
                    let id = {
                        let mut state = crate::ipc::server::SERVER_STATE.write();
                        state.register_app(pid, windows::Win32::Foundation::HANDLE::default())?
                    };
                    client_id = Some((id, true));
                    println!("Registered App with ID: {} (PID: {})", id, pid);
                }
                ControlMessage::RegisterMonitor { pid } => {
                    let id = {
                        let mut state = crate::ipc::server::SERVER_STATE.write();
                        state.register_monitor(
                            pid,
                            windows::Win32::Foundation::HANDLE::default(),
                            tx.clone(),
                        )
                    };
                    client_id = Some((id, false));
                    println!("Registered Monitor with ID: {} (PID: {})", id, pid);
                }
                ControlMessage::RegisterMonitorV2 {
                    pid,
                    kind,
                    owner_app_id,
                    request_id,
                    target_canvas_id,
                    mode,
                    flags,
                    manual_lifecycle,
                } => {
                    let owner_app_id_opt = (owner_app_id != 0).then_some(owner_app_id);
                    let request_id_opt = (request_id != 0).then_some(request_id);
                    let target_canvas_id_opt = (target_canvas_id != 0).then_some(target_canvas_id);
                    let (id, should_close) = {
                        let mut state = crate::ipc::server::SERVER_STATE.write();
                        state.register_monitor_v2(
                            pid,
                            windows::Win32::Foundation::HANDLE::default(),
                            tx.clone(),
                            kind,
                            owner_app_id_opt,
                            request_id_opt,
                            target_canvas_id_opt,
                            mode,
                            flags,
                            manual_lifecycle,
                        )
                    };
                    client_id = Some((id, false));
                    println!("Registered {:?} Monitor with ID: {} (PID: {})", kind, id, pid);
                    if should_close {
                        send_close_monitor(&tx, id);
                    } else if request_id != 0 && owner_app_id != 0 {
                        complete_pending_monitor_start(request_id, owner_app_id, id);
                    }
                }
                ControlMessage::CreateCanvas {
                    logical_w,
                    logical_h,
                    render_w,
                    render_h,
                } => {
                    if let Some((id, true)) = client_id {
                        let canvas_id = {
                            let mut state = crate::ipc::server::SERVER_STATE.write();
                            state
                                .create_canvas(id, logical_w, logical_h, render_w, render_h)
                                .with_context(|| {
                                    format!(
                                        "CreateCanvas failed: app_id={} logical={}x{} render={}x{}",
                                        id, logical_w, logical_h, render_w, render_h
                                    )
                                })?
                        };
                        println!("CreateCanvas created ID {} for App {}", canvas_id, id);
                    } else {
                        eprintln!(
                            "Error: CreateCanvas received but client is not a registered app"
                        );
                    }
                }
                ControlMessage::AttachMonitor {
                    canvas_id,
                    monitor_id,
                } => {
                    if let Some((_id, true)) = client_id {
                        let mut state = crate::ipc::server::SERVER_STATE.write();
                        if let Err(e) = state.attach_monitor(canvas_id, monitor_id) {
                            eprintln!("AttachMonitor error: {}", e);
                        } else {
                            println!("Attached Canvas {} to Monitor {}", canvas_id, monitor_id);
                        }
                    } else {
                        eprintln!(
                            "Error: AttachMonitor received but client is not a registered app"
                        );
                    }
                }
                ControlMessage::ListMonitorTypes { request_id } => {
                    if let Some((_app_id, true)) = client_id {
                        let entries = crate::process_manager::get_monitor_catalog().to_monitor_type_entries();
                        let _ = tx.send(ControlMessage::MonitorTypes { request_id, entries });
                    } else {
                        eprintln!("ListMonitorTypes received but client is not a registered app");
                    }
                }
                ControlMessage::StartMonitor {
                    request_id,
                    kind,
                    count,
                    target_canvas_id,
                    mode,
                    flags,
                    x,
                    y,
                    w,
                    h,
                } => {
                    let Some((app_id, true)) = client_id else {
                        send_start_monitor_result(&tx, request_id, MonitorRequestStatus::NotOwner, Vec::new());
                        continue;
                    };

                    if count == 0 {
                        send_start_monitor_result(&tx, request_id, MonitorRequestStatus::Ok, Vec::new());
                        continue;
                    }

                    match kind {
                        MonitorKind::GameBar => {
                            send_start_monitor_result(
                                &tx,
                                request_id,
                                MonitorRequestStatus::ManualOpenRequired,
                                Vec::new(),
                            );
                        }
                        MonitorKind::DesktopWindow => {
                            let catalog = crate::process_manager::get_monitor_catalog();
                            let Some(desktop) = catalog.desktop_window else {
                                send_start_monitor_result(
                                    &tx,
                                    request_id,
                                    MonitorRequestStatus::Unavailable,
                                    Vec::new(),
                                );
                                continue;
                            };

                            if desktop.window_modes & mode.bit() == 0 || flags & !desktop.flags != 0 {
                                send_start_monitor_result(
                                    &tx,
                                    request_id,
                                    MonitorRequestStatus::Unavailable,
                                    Vec::new(),
                                );
                                continue;
                            }

                            let (resolved_target_canvas_id, existing_count) = {
                                let state = crate::ipc::server::SERVER_STATE.read();
                                let existing_count = state
                                    .monitors
                                    .values()
                                    .filter(|monitor| {
                                        monitor.kind == MonitorKind::DesktopWindow
                                            && monitor.owner_app_id == Some(app_id)
                                            && monitor.core_managed
                                    })
                                    .count() as u32;
                                (
                                    state.resolve_app_canvas_id(app_id, target_canvas_id),
                                    existing_count,
                                )
                            };
                            let Some(target_canvas_id) = resolved_target_canvas_id else {
                                send_start_monitor_result(
                                    &tx,
                                    request_id,
                                    MonitorRequestStatus::InvalidCanvas,
                                    Vec::new(),
                                );
                                continue;
                            };
                            if existing_count.saturating_add(count) > desktop.max_instances_per_app {
                                send_start_monitor_result(
                                    &tx,
                                    request_id,
                                    MonitorRequestStatus::LimitExceeded,
                                    Vec::new(),
                                );
                                continue;
                            }

                            {
                                let mut pending = PENDING_MONITOR_STARTS.lock().unwrap();
                                pending.insert(
                                    (app_id, request_id),
                                    PendingMonitorStart {
                                        app_id,
                                        requested_count: count,
                                        monitor_ids: Vec::new(),
                                        tx: tx.clone(),
                                    },
                                );
                            }

                            let mut started_count = 0u32;
                            for _ in 0..count {
                                let result = crate::process_manager::start_desktop_window_monitor(
                                    crate::process_manager::DesktopWindowLaunchOptions {
                                        request_id,
                                        owner_app_id: app_id,
                                        target_canvas_id,
                                        mode,
                                        flags,
                                        x,
                                        y,
                                        w,
                                        h,
                                    },
                                );
                                match result {
                                    Ok(_) => started_count += 1,
                                    Err(e) => eprintln!(
                                        "StartMonitor: Desktop Window Monitor spawn failed: {}",
                                        e
                                    ),
                                }
                            }

                            if started_count == 0 {
                                PENDING_MONITOR_STARTS
                                    .lock()
                                    .unwrap()
                                    .remove(&(app_id, request_id));
                                send_start_monitor_result(
                                    &tx,
                                    request_id,
                                    MonitorRequestStatus::SpawnFailed,
                                    Vec::new(),
                                );
                                continue;
                            }

                            let completed = if started_count < count {
                                let mut pending = PENDING_MONITOR_STARTS.lock().unwrap();
                                if let Some(entry) = pending.get_mut(&(app_id, request_id)) {
                                    entry.requested_count = started_count;
                                    if entry.monitor_ids.len() as u32 >= started_count {
                                        let tx = entry.tx.clone();
                                        let monitor_ids = entry.monitor_ids.clone();
                                        pending.remove(&(app_id, request_id));
                                        Some((tx, monitor_ids))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            };
                            if let Some((pending_tx, monitor_ids)) = completed {
                                send_start_monitor_result(
                                    &pending_tx,
                                    request_id,
                                    MonitorRequestStatus::Ok,
                                    monitor_ids,
                                );
                            } else {
                                spawn_pending_monitor_timeout(app_id, request_id);
                            }
                        }
                    }
                }
                ControlMessage::StopMonitor { request_id, monitor_id } => {
                    if let Some((app_id, true)) = client_id {
                        let (status, monitor_tx) = {
                            let state = crate::ipc::server::SERVER_STATE.read();
                            state.close_monitor_if_owned(app_id, monitor_id)
                        };
                        if let Some(monitor_tx) = monitor_tx {
                            send_close_monitor(&monitor_tx, monitor_id);
                        }
                        let _ = tx.send(ControlMessage::StopMonitorResult { request_id, status });
                    } else {
                        let _ = tx.send(ControlMessage::StopMonitorResult {
                            request_id,
                            status: MonitorRequestStatus::NotOwner,
                        });
                    }
                }
                ControlMessage::LoadBitmap { bitmap_id, bytes } => {
                    if let Some((app_id, true)) = client_id {
                        let devices = {
                            let state = crate::ipc::server::SERVER_STATE.read();
                            state.devices.clone()
                        };
                        match {
                            let guard = devices.render_ctx.lock().unwrap();
                            guard.d2d.load_bitmap_from_memory(&bytes)
                        } {
                            Ok(handle) => {
                                let mut app_missing = false;
                                let previous = {
                                    let mut state = crate::ipc::server::SERVER_STATE.write();
                                    match state.apps.get_mut(&app_id) {
                                        Some(app) => app.bitmap_handles.insert(bitmap_id, handle),
                                        None => {
                                            app_missing = true;
                                            None
                                        }
                                    }
                                };
                                if app_missing {
                                    let guard = devices.render_ctx.lock().unwrap();
                                    let _ = guard.d2d.destroy_bitmap(handle);
                                } else if let Some(old) = previous {
                                    let guard = devices.render_ctx.lock().unwrap();
                                    let _ = guard.d2d.destroy_bitmap(old);
                                }
                            }
                            Err(e) => {
                                eprintln!("LoadBitmap: bitmap_id={} failed: {}", bitmap_id, e);
                            }
                        }
                    } else {
                        eprintln!("LoadBitmap received but client is not a registered app");
                    }
                }
                ControlMessage::SubmitFrame {
                    canvas_id,
                    frame_id,
                    offset,
                    length,
                } => {
                    if let Some((app_id, true)) = client_id {
                        let frame_start = Instant::now();
                        let cmds = {
                            let state = crate::ipc::server::SERVER_STATE.read();
                            if let Some(app) = state.apps.get(&app_id) {
                                if let Some(ref ringbuf) = app.command_ringbuffer {
                                    match ringbuf.command_slice(offset, length, 64 * 1024) {
                                        Ok(data) => Some(crate::ipc::cmd_decoder::decode_commands(data)),
                                        Err(e) => {
                                            eprintln!(
                                                "SubmitFrame: invalid command slice offset={} length={}: {}",
                                                offset, length, e
                                            );
                                            None
                                        }
                                    }
                                } else { None }
                            } else { None }
                        };

                        if let Some(cmds) = cmds {
                            let usage = scan_targets(&cmds);
                            let snapshot = {
                                let state = crate::ipc::server::SERVER_STATE.read();
                                state
                                    .resolve_app_canvas_id(app_id, canvas_id)
                                    .and_then(|resolved_canvas_id| {
                                        state.apps.get(&app_id).and_then(|app| {
                                            state.canvases.get(&resolved_canvas_id).map(|canvas| {
                                                (
                                                    resolved_canvas_id,
                                                    snapshot_submit_frame_targets(
                                                        canvas,
                                                        resolved_canvas_id,
                                                        frame_id,
                                                        &usage,
                                                    ),
                                                    state.devices.clone(),
                                                    app.bitmap_handles.clone(),
                                                )
                                            })
                                        })
                                    })
                            };

                            if let Some((resolved_canvas_id, targets, devices, bitmap_handles)) = snapshot {
                                let guard = devices.render_ctx.lock().unwrap();
                                dispatch_submit_frame(
                                    targets,
                                    &guard.d3d_ctx,
                                    &guard.d2d,
                                    resolved_canvas_id,
                                    frame_id,
                                    &cmds,
                                    &bitmap_handles,
                                    &mut render_durations,
                                    frame_start,
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }
    .await;

    if res.is_err() {
        detach_reason = crate::ipc::protocol::AppDetachReason::IoError;
    }

    // Cleanup on disconnect
    if let Some((id, is_app)) = client_id {
        let mut state = crate::ipc::server::SERVER_STATE.write();
        if is_app {
            println!("Cleaning up App {}", id);
            cancel_pending_monitor_starts_for_app(id);
            for (monitor_id, monitor_tx) in state.close_owned_desktop_monitors(id) {
                send_close_monitor(&monitor_tx, monitor_id);
            }

            // Task 6.2: Broadcast AppDetached before remove_app
            broadcast_app_detached(&state, id, detach_reason);

            state.remove_app(id);
        } else {
            println!("Cleaning up Monitor {}", id);
            state.remove_monitor(id);
        }
    }

    res
}

/// Dispatch one decoded `SubmitFrame` command stream onto the World surface
/// and any per-Monitor MonitorLocal surfaces referenced by the stream's
/// space stack.
///
/// Task 3.4 of the `animation-and-viewport-fix` spec (design.md §Fix
/// Implementation → Change 7). Responsibilities:
///
/// * Walk the `RenderCommand` stream once, maintaining a per-frame space
///   stack (tasks 3.2 emits the opcodes; 3.4 owns the stack semantics here).
/// * Route `CLEAR` / `FILL_RECT` to `canvas.resources` (World) or to every
///   entry in `canvas.per_monitor_surfaces` (MonitorLocal) based on
///   top-of-stack. Default (empty stack or bottom-of-stack) is World
///   (Bugfix 2.6).
/// * Acquire a non-DWM-held buffer for every target that will be written to,
///   with the bounded-wait policy from task 3.1 (Preservation 3.8). A
///   timeout on one target drops **only** that target for the frame.
/// * Flush once, then Present each target **independently** — a
///   per-Monitor Present failure MUST NOT propagate to World or to any
///   other monitor (Preservation 3.4).
/// * Record the frame render duration into a rolling 60-frame window and
///   warn when the average exceeds 8ms.
///
/// Preservation note: if the stream contains no `PUSH_SPACE`, `local_used`
/// is `false`, no per-Monitor acquires happen, and the World render path
/// is byte-for-byte the pre-task-3.4 path — which is exactly what PBT B
/// (preservation.rs) pins.
fn dispatch_submit_frame(
    targets: SubmitFrameTargets,
    ctx: &ID3D11DeviceContext,
    d2d: &D2DEngine,
    canvas_id: u32,
    frame_id: u64,
    cmds: &[RenderCommand],
    bitmap_handles: &HashMap<u32, u32>,
    render_durations: &mut VecDeque<Duration>,
    frame_start: Instant,
) {
    let world_target = targets.world.as_ref();
    let local_targets = &targets.locals;

    // ---------------------------------------------------------------
    // Single-pass render walk with the per-frame space stack.
    //
    // Geometry opcodes other than CLEAR/FILL_RECT are preserved as
    // no-ops here to match the pre-task-3.4 dispatch behavior (the
    // decoder accepts them for Preservation 3.6 but the renderer has
    // never drawn them — PBT B hashes this exact behavior).
    // ---------------------------------------------------------------
    let mut stack: Vec<SpaceId> = Vec::new();
    for cmd in cmds {
        match cmd {
            RenderCommand::PushSpace(s) => stack.push(*s),
            RenderCommand::PopSpace => {
                if stack.pop().is_none() {
                    eprintln!(
                        "[server_task] canvas={} frame={} POP_SPACE on empty \
                         stack — ignored (task 3.2 misuse policy)",
                        canvas_id, frame_id
                    );
                }
            }
            RenderCommand::Clear(c) => match stack.last() {
                Some(SpaceId::MonitorLocal) => {
                    for target in local_targets.values() {
                        unsafe {
                            ctx.ClearRenderTargetView(&target.rtv, c);
                        }
                    }
                }
                _ => {
                    if let Some(target) = world_target {
                        unsafe {
                            ctx.ClearRenderTargetView(&target.rtv, c);
                        }
                    }
                }
            },
            RenderCommand::Draw(DrawCmd::FillRect { x, y, w, h, rgba }) => match stack.last() {
                Some(SpaceId::MonitorLocal) => {
                    for target in local_targets.values() {
                        fill_rect_on_target(
                            ctx,
                            &target.texture,
                            target.render_w,
                            target.render_h,
                            *x,
                            *y,
                            *w,
                            *h,
                            *rgba,
                        );
                    }
                }
                _ => {
                    if let Some(target) = world_target {
                        fill_rect_on_target(
                            ctx,
                            &target.texture,
                            target.render_w,
                            target.render_h,
                            *x,
                            *y,
                            *w,
                            *h,
                            *rgba,
                        );
                    }
                }
            },
            RenderCommand::Draw(cmd) => {
                draw_command_for_space(d2d, world_target, local_targets, &stack, cmd);
            }
            RenderCommand::DrawBitmap(draw) => {
                if let Some(bitmap) = bitmap_handles.get(&draw.bitmap_id) {
                    let cmd = draw_bitmap_command(draw, *bitmap);
                    draw_command_for_space(d2d, world_target, local_targets, &stack, &cmd);
                }
            }
        }
    }
    if !stack.is_empty() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "[server_task] canvas={} frame={} frame ended with non-empty \
                 space stack (depth={}) — auto-dropped (further occurrences suppressed)",
                canvas_id,
                frame_id,
                stack.len()
            );
        }
    }

    // One Flush covers all GPU work issued above for every target.
    unsafe {
        ctx.Flush();
    }

    // ---------------------------------------------------------------
    // Present each target INDEPENDENTLY. A per-Monitor failure MUST
    // NOT affect World or other Monitors (Preservation 3.4).
    // ---------------------------------------------------------------
    if let Some(target) = world_target {
        unsafe {
            match target.surface.SetBuffer(&target.buffer) {
                Err(e) => {
                    eprintln!(
                        "SubmitFrame: canvas={} frame={} World SetBuffer error: {}",
                        canvas_id, frame_id, e
                    );
                }
                Ok(()) => match present_manager(&target.manager, "CanvasResources") {
                    PresentOutcome::Success => {}
                    PresentOutcome::RetryNextTick => {}
                    PresentOutcome::DeviceLost => {
                        eprintln!(
                            "SubmitFrame: canvas={} frame={} World device-lost \
                             — Canvas rebuild required (not yet implemented)",
                            canvas_id, frame_id
                        );
                    }
                },
            }
        }
    }

    for (monitor_id, target) in local_targets {
        unsafe {
            match target.surface.SetBuffer(&target.buffer) {
                Err(e) => {
                    eprintln!(
                        "SubmitFrame: canvas={} frame={} monitor={} \
                         MonitorLocal SetBuffer error: {} — skipping this \
                         monitor only",
                        canvas_id, frame_id, monitor_id, e
                    );
                    continue;
                }
                Ok(()) => match present_manager(&target.manager, "PerMonitorResources") {
                    PresentOutcome::Success => {}
                    PresentOutcome::RetryNextTick => {}
                    PresentOutcome::DeviceLost => {
                        eprintln!(
                            "SubmitFrame: canvas={} frame={} monitor={} \
                             MonitorLocal device-lost — per-Monitor \
                             rebuild required (not yet implemented)",
                            canvas_id, frame_id, monitor_id
                        );
                    }
                },
            }
        }
    }

    // Drain present statistics once at the end of the frame without blocking.
    // The APCs will arrive while we wait in `acquire_available_buffer`.
    unsafe {
        if let Some(target) = world_target {
            while target.manager.GetNextPresentStatistics().is_ok() {}
        }
        for target in local_targets.values() {
            while target.manager.GetNextPresentStatistics().is_ok() {}
        }
    }

    // ---------------------------------------------------------------
    // Rolling-average render-duration metric (task 3.4 requirement).
    // ---------------------------------------------------------------
    let (avg, warn) = record_render_duration(render_durations, frame_start.elapsed());
    if warn {
        eprintln!(
            "[server_task] canvas={} frame={} avg render duration over last {} frames is {:.2}ms",
            canvas_id,
            frame_id,
            RENDER_DURATION_WINDOW,
            avg.as_secs_f64() * 1000.0
        );
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the task-3.4 helpers that are pure (no GPU needed).
    //!
    //! The end-to-end dispatcher (`dispatch_submit_frame`) exercises D3D11
    //! + DComp and is covered by the integration tests under
    //! `core-server/tests/`; here we only test the space-stack pre-scan
    //! and the rolling-average metric.

    use super::*;
    use crate::renderer::painter::DrawCmd;

    #[test]
    fn scan_targets_empty_stream_is_all_false() {
        let t = scan_targets(&[]);
        assert!(!t.world_used);
        assert!(!t.local_used);
    }

    #[test]
    fn scan_targets_world_only_stream_sets_world_used() {
        let cmds = vec![
            RenderCommand::Clear([0.0, 0.0, 0.0, 1.0]),
            RenderCommand::Draw(DrawCmd::FillRect {
                x: 1.0,
                y: 2.0,
                w: 3.0,
                h: 4.0,
                rgba: [1.0, 0.0, 0.0, 1.0],
            }),
        ];
        let t = scan_targets(&cmds);
        assert!(t.world_used);
        assert!(!t.local_used);
    }

    #[test]
    fn scan_targets_monitor_local_region_sets_local_used() {
        // PUSH(MonitorLocal) / FILL_RECT / POP — only local_used, not world.
        let cmds = vec![
            RenderCommand::PushSpace(SpaceId::MonitorLocal),
            RenderCommand::Draw(DrawCmd::FillRect {
                x: 10.0,
                y: 10.0,
                w: 20.0,
                h: 4.0,
                rgba: [0.0, 1.0, 0.0, 1.0],
            }),
            RenderCommand::PopSpace,
        ];
        let t = scan_targets(&cmds);
        assert!(!t.world_used);
        assert!(t.local_used);
    }

    #[test]
    fn scan_targets_mixed_stream_sets_both() {
        let cmds = vec![
            RenderCommand::Clear([0.0, 0.0, 0.0, 1.0]),
            RenderCommand::PushSpace(SpaceId::MonitorLocal),
            RenderCommand::Draw(DrawCmd::FillRect {
                x: 10.0,
                y: 10.0,
                w: 20.0,
                h: 4.0,
                rgba: [0.0, 1.0, 0.0, 1.0],
            }),
            RenderCommand::PopSpace,
            RenderCommand::Draw(DrawCmd::FillRect {
                x: 0.0,
                y: 0.0,
                w: 5.0,
                h: 5.0,
                rgba: [1.0, 0.0, 0.0, 1.0],
            }),
        ];
        let t = scan_targets(&cmds);
        assert!(t.world_used);
        assert!(t.local_used);
    }

    #[test]
    fn scan_targets_push_world_stays_world() {
        // An explicit PUSH(World) must still route to the World target.
        let cmds = vec![
            RenderCommand::PushSpace(SpaceId::World),
            RenderCommand::Clear([0.0, 0.0, 0.0, 1.0]),
            RenderCommand::PopSpace,
        ];
        let t = scan_targets(&cmds);
        assert!(t.world_used);
        assert!(!t.local_used);
    }

    #[test]
    fn scan_targets_nested_spaces_uses_top_of_stack() {
        // PUSH(World) → PUSH(MonitorLocal) → FILL → POP → FILL
        // First FILL must land in MonitorLocal (top = MonitorLocal).
        // Second FILL must land in World (top = World after one POP).
        let cmds = vec![
            RenderCommand::PushSpace(SpaceId::World),
            RenderCommand::PushSpace(SpaceId::MonitorLocal),
            RenderCommand::Draw(DrawCmd::FillRect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                rgba: [0.0, 1.0, 0.0, 1.0],
            }),
            RenderCommand::PopSpace,
            RenderCommand::Draw(DrawCmd::FillRect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                rgba: [1.0, 0.0, 0.0, 1.0],
            }),
            RenderCommand::PopSpace,
        ];
        let t = scan_targets(&cmds);
        assert!(t.world_used);
        assert!(t.local_used);
    }

    #[test]
    fn scan_targets_pop_on_empty_stack_tolerates_misuse() {
        // POP on empty stack is allowed (dispatcher warns + skips). The
        // scan must not panic; subsequent FILL defaults to World.
        let cmds = vec![
            RenderCommand::PopSpace,
            RenderCommand::Clear([0.0, 0.0, 0.0, 1.0]),
        ];
        let t = scan_targets(&cmds);
        assert!(t.world_used);
        assert!(!t.local_used);
    }

    #[test]
    fn record_render_duration_below_threshold_does_not_warn() {
        let mut w: VecDeque<Duration> = VecDeque::new();
        // Fill the window with 1ms samples — avg well below 8ms.
        for _ in 0..RENDER_DURATION_WINDOW {
            let (_, warn) = record_render_duration(&mut w, Duration::from_millis(1));
            assert!(!warn);
        }
        assert_eq!(w.len(), RENDER_DURATION_WINDOW);
    }

    #[test]
    fn record_render_duration_partial_window_never_warns() {
        // Even with each sample > 8ms, the window is not full until we have
        // RENDER_DURATION_WINDOW samples, so warn must stay false.
        let mut w: VecDeque<Duration> = VecDeque::new();
        for _ in 0..(RENDER_DURATION_WINDOW - 1) {
            let (_, warn) = record_render_duration(&mut w, Duration::from_millis(100));
            assert!(!warn, "partial window must not trip the warn threshold");
        }
    }

    #[test]
    fn record_render_duration_full_window_over_threshold_warns() {
        let mut w: VecDeque<Duration> = VecDeque::new();
        // Fill with 10ms samples — avg = 10ms > 8ms threshold, and the
        // window is full on the 60th sample.
        let mut last_warn = false;
        for _ in 0..RENDER_DURATION_WINDOW {
            let (_, warn) = record_render_duration(&mut w, Duration::from_millis(10));
            last_warn = warn;
        }
        assert!(
            last_warn,
            "full window of 10ms samples should trip the >8ms rolling-avg warn"
        );
    }

    #[test]
    fn record_render_duration_bounds_window_size() {
        let mut w: VecDeque<Duration> = VecDeque::new();
        for _ in 0..(RENDER_DURATION_WINDOW * 3) {
            let _ = record_render_duration(&mut w, Duration::from_micros(500));
        }
        assert_eq!(
            w.len(),
            RENDER_DURATION_WINDOW,
            "rolling window must cap at RENDER_DURATION_WINDOW entries — \
             preservation 3.8 'no unbounded queueing'"
        );
    }
}
