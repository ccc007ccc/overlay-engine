use std::collections::HashMap;

use parking_lot::RwLock;
use windows::Win32::Foundation::HANDLE;

use crate::renderer::dcomp::{CanvasResources, CoreDevices, PerMonitorResources};
use crate::ipc::shmem::SharedMemory;

pub struct Canvas {
    pub id: u32,
    pub owner_pid: u32,
    pub logical_w: u32,
    pub logical_h: u32,
    pub resources: CanvasResources,
    /// Per-Monitor MonitorLocal surfaces, keyed by `monitor_id`. Task 3.3
    /// of the `animation-and-viewport-fix` spec (design.md §Fix
    /// Implementation → Change 4).
    ///
    /// Each entry is created at `attach_monitor` time with its own NT
    /// handle + multi-buffer ring + DComp surface. Task 3.4's dispatcher
    /// replays MonitorLocal-scoped commands onto every entry in this map
    /// so MonitorLocal content appears independently at each monitor's
    /// client-area origin.
    ///
    /// Lifecycle (Preservation 3.4, 3.5):
    /// * entry added by `attach_monitor`,
    /// * entry removed by `remove_monitor`,
    /// * the whole map (along with `resources`) is dropped when the
    ///   canvas's owner app disconnects (`remove_app`).
    pub per_monitor_surfaces: HashMap<u32, PerMonitorResources>,
}

pub struct App {
    pub id: u32,
    pub pid: u32,
    pub handle: HANDLE,
    pub canvas_ids: Vec<u32>, // Canvas IDs owned by this app
    pub command_ringbuffer: Option<SharedMemory>,
}

pub struct Monitor {
    pub id: u32,
    pub pid: u32,
    pub handle: HANDLE,
    pub tx: tokio::sync::mpsc::UnboundedSender<crate::ipc::protocol::ControlMessage>,
}

pub struct ServerState {
    pub devices: CoreDevices,
    pub apps: HashMap<u32, App>, // Keyed by App ID
    pub monitors: HashMap<u32, Monitor>, // Keyed by Monitor ID
    pub canvases: HashMap<u32, Canvas>,    // Keyed by Canvas ID

    next_app_id: u32,
    next_monitor_id: u32,
    next_canvas_id: u32,
}

unsafe impl Send for ServerState {}
unsafe impl Sync for ServerState {}

impl ServerState {
    pub fn new() -> anyhow::Result<Self> {
        let devices = CoreDevices::new()?;
        Ok(Self {
            devices,
            apps: HashMap::new(),
            monitors: HashMap::new(),
            canvases: HashMap::new(),
            next_app_id: 1,
            next_monitor_id: 1,
            next_canvas_id: 1,
        })
    }

    pub fn register_app(&mut self, pid: u32, handle: HANDLE) -> anyhow::Result<u32> {
        let id = self.next_app_id;
        self.next_app_id += 1;

        let shmem_name = format!("overlay-core-cmds-{}", pid);
        let command_ringbuffer = SharedMemory::create(&shmem_name, 16 * 1024 * 1024)?; // 16MB ringbuffer

        self.apps.insert(
            id,
            App {
                id,
                pid,
                handle,
                canvas_ids: Vec::new(),
                command_ringbuffer: Some(command_ringbuffer),
            },
        );
        Ok(id)
    }

    pub fn register_monitor(&mut self, pid: u32, handle: HANDLE, tx: tokio::sync::mpsc::UnboundedSender<crate::ipc::protocol::ControlMessage>) -> u32 {
        let id = self.next_monitor_id;
        self.next_monitor_id += 1;
        self.monitors.insert(
            id,
            Monitor {
                id,
                pid,
                handle,
                tx,
            },
        );

        // auto-attach to all existing canvases
        let canvas_ids: Vec<u32> = self.canvases.keys().copied().collect();
        for cid in canvas_ids {
            if let Err(e) = self.attach_monitor(cid, id) {
                eprintln!("auto-attach monitor {} to canvas {} failed: {}", id, cid, e);
            }
        }

        id
    }

    pub fn create_canvas(
        &mut self,
        owner_id: u32,
        logical_w: u32,
        logical_h: u32,
        render_w: u32,
        render_h: u32,
    ) -> anyhow::Result<u32> {
        if let Some(app) = self.apps.get_mut(&owner_id) {
            // Allocate 32MB instead of 4MB for the shared memory to match demo-app's
            // unlocked buffer bloat padding. (Though core-server just opens what app gives it).
            let id = self.next_canvas_id;
            self.next_canvas_id += 1;

            let resources = CanvasResources::new(&self.devices.d3d, render_w, render_h)?;

            {
                let guard = self.devices.render_ctx.lock().unwrap();
                let _ = resources.present_color(&guard.d3d_ctx, [0.0, 0.0, 0.0, 0.0]);
            }

            self.canvases.insert(
                id,
                Canvas {
                    id,
                    owner_pid: app.pid,
                    logical_w,
                    logical_h,
                    resources,
                    per_monitor_surfaces: HashMap::new(),
                },
            );
            app.canvas_ids.push(id);

            // auto-attach all existing monitors
            let monitor_ids: Vec<u32> = self.monitors.keys().copied().collect();
            for cid in monitor_ids {
                if let Err(e) = self.attach_monitor(id, cid) {
                    eprintln!("auto-attach monitor {} to canvas {} failed: {}", cid, id, e);
                }
            }

            Ok(id)
        } else {
            Err(anyhow::anyhow!("App not found"))
        }
    }

    pub fn remove_app(&mut self, id: u32) {
        if let Some(app) = self.apps.remove(&id) {
            // Task 3.3 / Preservation 3.5: release every
            // `PerMonitorResources` along with the World `CanvasResources`
            // for each owned canvas.
            for canvas_id in &app.canvas_ids {
                if let Some(canvas) = self.canvases.get(canvas_id) {
                    let _notified: Vec<u32> =
                        canvas.per_monitor_surfaces.keys().copied().collect();
                    // Future: send ControlMessage::CanvasDetached to each
                    // monitor in `_notified`. Today the drop below is the
                    // observable cleanup.
                }
            }
            for canvas_id in app.canvas_ids {
                self.canvases.remove(&canvas_id);
            }
        }
    }

    pub fn remove_monitor(&mut self, id: u32) {
        // Task 3.3 / Preservation 3.4: a monitor drop MUST release its
        // `PerMonitorResources` from every canvas it was attached to, and
        // MUST NOT affect other monitors or World resources.
        for canvas in self.canvases.values_mut() {
            canvas.per_monitor_surfaces.remove(&id);
        }
        self.monitors.remove(&id);
    }

    pub fn attach_monitor(&mut self, canvas_id: u32, monitor_id: u32) -> anyhow::Result<()> {
        // First phase: borrow-immutably to read Canvas metadata and the
        // World surface handle; build the CanvasAttached message.
        let (
            surface_handle,
            logical_w,
            logical_h,
            render_w,
            render_h,
        ) = {
            let canvas = self
                .canvases
                .get(&canvas_id)
                .ok_or_else(|| anyhow::anyhow!("Canvas not found"))?;
            (
                canvas.resources.handle,
                canvas.logical_w,
                canvas.logical_h,
                canvas.resources.render_w,
                canvas.resources.render_h,
            )
        };

        let (monitor_pid, monitor_tx) = {
            let monitor = self
                .monitors
                .get(&monitor_id)
                .ok_or_else(|| anyhow::anyhow!("Monitor not found"))?;
            (monitor.pid, monitor.tx.clone())
        };

        // Duplicate the World surface handle into the monitor's process.
        // Preservation 3.1: this step and the `CanvasAttached` payload are
        // unchanged — existing monitors still see the same World handoff.
        let monitor_proc = unsafe {
            windows::Win32::System::Threading::OpenProcess(
                windows::Win32::System::Threading::PROCESS_DUP_HANDLE,
                false,
                monitor_pid,
            )?
        };

        let mut dup_world: HANDLE = HANDLE::default();
        let cur_proc = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
        unsafe {
            windows::Win32::Foundation::DuplicateHandle(
                cur_proc,
                surface_handle,
                monitor_proc,
                &mut dup_world,
                0,
                false,
                windows::Win32::Foundation::DUPLICATE_SAME_ACCESS,
            )?;
        }

        // Send the CanvasAttached message first — its on-the-wire layout
        // MUST NOT change (Preservation 3.1, scheme α).
        let world_msg = crate::ipc::protocol::ControlMessage::CanvasAttached {
            canvas_id,
            surface_handle: dup_world.0 as u64,
            logical_w,
            logical_h,
            render_w,
            render_h,
        };
        let _ = monitor_tx.send(world_msg);

        // Second phase: create (or reuse) the per-Monitor MonitorLocal
        // surface. Task 3.3 / design.md §Fix Implementation → Change 4, 5.
        //
        // Sizing: per the task text, the MonitorLocal surface is sized to
        // the monitor's reported client-area logical dimensions OR a
        // bounded cap `min(canvas_logical, 4096)`. We don't yet have a
        // "monitor reports client-area logical size" opcode; as a
        // sensible default we use the canvas's logical size clamped to
        // `PER_MONITOR_MAX_DIM` — this matches the bounded-cap branch
        // spelled out in the task. A future task can layer in a
        // monitor-reported size without changing this struct's shape.
        let per_monitor_result = {
            // We hold only a short read-lock on the canvas. Because this
            // method already takes `&mut self`, we can take `&mut` on the
            // canvas directly.
            let canvas = self
                .canvases
                .get_mut(&canvas_id)
                .ok_or_else(|| anyhow::anyhow!("Canvas not found"))?;

            // Lazily create; if a prior attach for the same monitor left a
            // surface behind, reuse it instead of leaking a second one.
            if !canvas.per_monitor_surfaces.contains_key(&monitor_id) {
                match PerMonitorResources::new(
                    &self.devices.d3d,
                    canvas.logical_w,
                    canvas.logical_h,
                ) {
                    Ok(res) => {
                        // Initial transparent clear so DWM has a valid
                        // first buffer to show in the monitor's second
                        // visual before the app ever emits a
                        // MonitorLocal-scoped command.
                        {
                            let guard = self.devices.render_ctx.lock().unwrap();
                            let _ = res.present_color(&guard.d3d_ctx, [0.0, 0.0, 0.0, 0.0]);
                        }
                        canvas.per_monitor_surfaces.insert(monitor_id, res);
                    }
                    Err(e) => {
                        // Per-Monitor surface creation failure is
                        // non-fatal: the monitor still has its World
                        // handoff (Preservation 3.2 / 3.3 still hold).
                        // Log and move on — the MonitorLocal second
                        // visual just won't be mounted for this monitor.
                        eprintln!(
                            "[attach_monitor] canvas={} monitor={} \
                             PerMonitorResources::new failed: {} — \
                             MonitorLocal surface not created",
                            canvas_id, monitor_id, e
                        );
                        unsafe {
                            let _ =
                                windows::Win32::Foundation::CloseHandle(monitor_proc);
                        }
                        return Ok(());
                    }
                }
            }

            // SAFETY: we just inserted (or verified) the entry.
            let pc = canvas
                .per_monitor_surfaces
                .get(&monitor_id)
                .expect("per_monitor_surfaces entry just inserted");
            Ok::<_, anyhow::Error>((pc.handle, pc.logical_w, pc.logical_h))
        };

        let (pc_handle, pc_logical_w, pc_logical_h) = match per_monitor_result {
            Ok(t) => t,
            Err(e) => {
                unsafe {
                    let _ = windows::Win32::Foundation::CloseHandle(monitor_proc);
                }
                return Err(e);
            }
        };

        // Duplicate the MonitorLocal surface handle into the monitor's
        // process.
        let mut dup_monitor_local: HANDLE = HANDLE::default();
        let dup_result = unsafe {
            windows::Win32::Foundation::DuplicateHandle(
                cur_proc,
                pc_handle,
                monitor_proc,
                &mut dup_monitor_local,
                0,
                false,
                windows::Win32::Foundation::DUPLICATE_SAME_ACCESS,
            )
        };
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(monitor_proc);
        }

        if let Err(e) = dup_result {
            eprintln!(
                "[attach_monitor] canvas={} monitor={} DuplicateHandle on \
                 per-Monitor MonitorLocal surface failed: {} — skipping \
                 MonitorLocalSurfaceAttached send",
                canvas_id, monitor_id, e
            );
            return Ok(());
        }

        // Send MonitorLocalSurfaceAttached immediately after
        // CanvasAttached. Scheme α: this is a NEW opcode; monitors that
        // don't recognize it ignore it via the unknown-opcode downgrade.
        let ml_msg = crate::ipc::protocol::ControlMessage::MonitorLocalSurfaceAttached {
            canvas_id,
            monitor_id,
            surface_handle: dup_monitor_local.0 as u64,
            logical_w: pc_logical_w,
            logical_h: pc_logical_h,
        };
        let _ = monitor_tx.send(ml_msg);

        Ok(())
    }
}

// Global server state wrapped in an RwLock.
lazy_static::lazy_static! {
    pub static ref SERVER_STATE: RwLock<ServerState> = RwLock::new(ServerState::new().expect("Failed to initialize ServerState"));
}
