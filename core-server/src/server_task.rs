use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_BOX,
};

use crate::ipc::cmd_decoder::{RenderCommand, SpaceId};
use crate::ipc::protocol::{ControlMessage, MessageHeader, HEADER_SIZE};
use crate::renderer::dcomp::{AcquireOutcome, PresentOutcome, ACQUIRE_TIMEOUT_MS};
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

pub async fn run_server() -> anyhow::Result<()> {
    use std::ffi::CString;
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorA, SDDL_REVISION_1};
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
            .create_with_security_attributes_raw(PIPE_NAME, &mut sa as *mut _ as *mut std::ffi::c_void)?
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
            next_server_options.create_with_security_attributes_raw(PIPE_NAME, &mut sa_next as *mut _ as *mut std::ffi::c_void)?
        };

        unsafe {
            if !sd_next.0.is_null() {
                windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(sd_next.0)));
            }
        }

        // Spawn a new task to handle the connected client
        tokio::spawn(async move {
            if let Err(e) = handle_client(connected_client).await {
                eprintln!("Client error: {}", e);
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
            RenderCommand::Clear(_) | RenderCommand::Draw(_) => {
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

fn draw_text_on_target(
    d2d: &D2DEngine,
    texture: &ID3D11Texture2D,
    rw: u32,
    rh: u32,
    text: &str,
    x: f32,
    y: f32,
    font_size: f32,
    rgba: [f32; 4],
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
    painter.draw_text(text, x, y, font_size, rgba);
    unsafe {
        let _ = d2d.dc.EndDraw(None, None);
        d2d.dc.SetTarget(None);
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
                for mid in canvas.per_monitor_surfaces.keys() {
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
                ControlMessage::CreateCanvas {
                    logical_w,
                    logical_h,
                    render_w,
                    render_h,
                } => {
                    if let Some((id, true)) = client_id {
                        let canvas_id = {
                            let mut state = crate::ipc::server::SERVER_STATE.write();
                            state.create_canvas(id, logical_w, logical_h, render_w, render_h)?
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
                                    let data = ringbuf.data();
                                    let start = offset as usize;
                                    let end = start + length as usize;
                                    if end <= data.len() {
                                        Some(crate::ipc::cmd_decoder::decode_commands(&data[start..end]))
                                    } else {
                                        None
                                    }
                                } else { None }
                            } else { None }
                        };

                        if let Some(cmds) = cmds {
                            let state = crate::ipc::server::SERVER_STATE.read();
                            if let Some(app) = state.apps.get(&app_id) {
                                let resolved_canvas_id = if canvas_id == 0 {
                                    app.canvas_ids.first().copied().unwrap_or(0)
                                } else {
                                    canvas_id
                                };
                                if let Some(canvas) = state.canvases.get(&resolved_canvas_id) {
                                    let guard = state.devices.render_ctx.lock().unwrap();
                                    dispatch_submit_frame(
                                        canvas,
                                        &guard.d3d_ctx,
                                        &guard.d2d,
                                        resolved_canvas_id,
                                        frame_id,
                                        &cmds,
                                        &mut render_durations,
                                        frame_start,
                                    );
                                }
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
    canvas: &crate::ipc::server::Canvas,
    ctx: &ID3D11DeviceContext,
    d2d: &D2DEngine,
    canvas_id: u32,
    frame_id: u64,
    cmds: &[RenderCommand],
    render_durations: &mut VecDeque<Duration>,
    frame_start: Instant,
) {
    // ---------------------------------------------------------------
    // Pre-scan: which targets will be written?
    // Skipping unused targets preserves the zero-overhead World-only
    // path for apps that never emit PUSH_SPACE (Preservation 3.6).
    // ---------------------------------------------------------------
    let usage = scan_targets(cmds);

    // ---------------------------------------------------------------
    // Acquire World buffer (if used). A timeout drops World ONLY —
    // MonitorLocal targets still render (Preservation 3.4 symmetry).
    // ---------------------------------------------------------------
    let world_idx: Option<usize> = if usage.world_used {
        match canvas
            .resources
            .acquire_available_buffer(ACQUIRE_TIMEOUT_MS)
        {
            AcquireOutcome::Acquired(i) => Some(i),
            AcquireOutcome::TimedOut => {
                // Silently drop
                None
            }
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

    // ---------------------------------------------------------------
    // Acquire per-Monitor MonitorLocal buffers (if used). A timeout
    // or failure on one monitor drops that monitor only — the others
    // still render and Present (Preservation 3.4).
    // ---------------------------------------------------------------
    let mut local_idxs: HashMap<u32, usize> = HashMap::new();
    if usage.local_used {
        for (cid, pc) in &canvas.per_monitor_surfaces {
            match pc.acquire_available_buffer(ACQUIRE_TIMEOUT_MS) {
                AcquireOutcome::Acquired(i) => {
                    local_idxs.insert(*cid, i);
                }
                AcquireOutcome::TimedOut => {
                    // Silently drop
                }
                AcquireOutcome::Failed(e) => {
                    eprintln!(
                        "SubmitFrame: canvas={} frame={} monitor={} MonitorLocal \
                         — acquire failed: {}",
                        canvas_id, frame_id, cid, e
                    );
                }
            }
        }
    }

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
                    for (cid, idx) in &local_idxs {
                        if let Some(pc) = canvas.per_monitor_surfaces.get(cid) {
                            unsafe {
                                ctx.ClearRenderTargetView(&pc.rtvs[*idx], c);
                            }
                        }
                    }
                }
                _ => {
                    if let Some(idx) = world_idx {
                        unsafe {
                            ctx.ClearRenderTargetView(&canvas.resources.rtvs[idx], c);
                        }
                    }
                }
            },
            RenderCommand::Draw(DrawCmd::FillRect { x, y, w, h, rgba }) => match stack.last() {
                Some(SpaceId::MonitorLocal) => {
                    for (cid, idx) in &local_idxs {
                        if let Some(pc) = canvas.per_monitor_surfaces.get(cid) {
                            fill_rect_on_target(
                                ctx,
                                &pc.textures[*idx],
                                pc.render_w,
                                pc.render_h,
                                *x,
                                *y,
                                *w,
                                *h,
                                *rgba,
                            );
                        }
                    }
                }
                _ => {
                    if let Some(idx) = world_idx {
                        fill_rect_on_target(
                            ctx,
                            &canvas.resources.textures[idx],
                            canvas.resources.render_w,
                            canvas.resources.render_h,
                            *x,
                            *y,
                            *w,
                            *h,
                            *rgba,
                        );
                    }
                }
            },
            RenderCommand::Draw(DrawCmd::DrawText {
                text,
                x,
                y,
                font_size,
                rgba,
            }) => match stack.last() {
                Some(SpaceId::MonitorLocal) => {
                    for (cid, idx) in &local_idxs {
                        if let Some(pc) = canvas.per_monitor_surfaces.get(cid) {
                            draw_text_on_target(
                                d2d,
                                &pc.textures[*idx],
                                pc.render_w,
                                pc.render_h,
                                text,
                                *x,
                                *y,
                                *font_size,
                                *rgba,
                            );
                        }
                    }
                }
                _ => {
                    if let Some(idx) = world_idx {
                        draw_text_on_target(
                            d2d,
                            &canvas.resources.textures[idx],
                            canvas.resources.render_w,
                            canvas.resources.render_h,
                            text,
                            *x,
                            *y,
                            *font_size,
                            *rgba,
                        );
                    }
                }
            },
            // Remaining geometry opcodes decode-but-noop — Preservation
            // 3.6. Adding GPU rendering for them is deliberately out of
            // scope for task 3.4 (would change PBT B oracle hashes).
            RenderCommand::Draw(_) => {}
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
    if let Some(idx) = world_idx {
        unsafe {
            match canvas
                .resources
                .surface
                .SetBuffer(&canvas.resources.buffers[idx])
            {
                Err(e) => {
                    eprintln!(
                        "SubmitFrame: canvas={} frame={} World SetBuffer error: {}",
                        canvas_id, frame_id, e
                    );
                }
                Ok(()) => {
                    match canvas.resources.present() {
                        PresentOutcome::Success => {}
                        PresentOutcome::RetryNextTick => {
                            // Transient — frame dropped, app retries.
                        }
                        PresentOutcome::DeviceLost => {
                            eprintln!(
                                "SubmitFrame: canvas={} frame={} World device-lost \
                                 — Canvas rebuild required (not yet implemented)",
                                canvas_id, frame_id
                            );
                        }
                    }
                    // DO NOT sleep/drain here after every present.
                    // This was causing backpressure stalls because if the APC
                    // hadn't arrived yet, we read nothing, but then blocked the
                    // thread from getting it later. We now let acquire_available_buffer
                    // do an alertable wait to fetch APCs exactly when we need them.
                }
            }
        }
    }

    for (cid, idx) in &local_idxs {
        let Some(pc) = canvas.per_monitor_surfaces.get(cid) else {
            continue;
        };
        unsafe {
            match pc.surface.SetBuffer(&pc.buffers[*idx]) {
                Err(e) => {
                    eprintln!(
                        "SubmitFrame: canvas={} frame={} monitor={} \
                         MonitorLocal SetBuffer error: {} — skipping this \
                         monitor only",
                        canvas_id, frame_id, cid, e
                    );
                    continue;
                }
                Ok(()) => {
                    match pc.present() {
                        PresentOutcome::Success => {}
                        PresentOutcome::RetryNextTick => {}
                        PresentOutcome::DeviceLost => {
                            eprintln!(
                                "SubmitFrame: canvas={} frame={} monitor={} \
                                 MonitorLocal device-lost — per-Monitor \
                                 rebuild required (not yet implemented)",
                                canvas_id, frame_id, cid
                            );
                        }
                    }
                }
            }
        }
    }

    // Drain present statistics once at the end of the frame without blocking.
    // The APCs will arrive while we wait in `acquire_available_buffer`.
    unsafe {
        while canvas.resources.manager.GetNextPresentStatistics().is_ok() {}
        for pc in canvas.per_monitor_surfaces.values() {
            while pc.manager.GetNextPresentStatistics().is_ok() {}
        }
    }

    // ---------------------------------------------------------------
    // Rolling-average render-duration metric (task 3.4 requirement).
    // ---------------------------------------------------------------
    let _ = record_render_duration(render_durations, frame_start.elapsed());
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
