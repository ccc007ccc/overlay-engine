use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::RwLock;
use windows::Win32::Foundation::HANDLE;

use crate::ipc::protocol::{ControlMessage, DesktopWindowMode, MonitorKind, MonitorRequestStatus};
use crate::ipc::shmem::SharedMemory;
use crate::renderer::dcomp::{
    CanvasResources, CompositeOutputResources, CoreDevices, PerMonitorResources,
};
use crate::renderer::resources::BitmapHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorMetrics {
    pub logical_w: u32,
    pub logical_h: u32,
}

impl MonitorMetrics {
    fn from_canvas(canvas: &Canvas) -> Self {
        Self {
            logical_w: canvas.logical_w,
            logical_h: canvas.logical_h,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualWindow {
    pub x: i32,
    pub y: i32,
    pub logical_w: u32,
    pub logical_h: u32,
    pub visible: bool,
}

impl VirtualWindow {
    fn from_canvas(canvas: &Canvas) -> Self {
        Self {
            x: 0,
            y: 0,
            logical_w: canvas.logical_w,
            logical_h: canvas.logical_h,
            visible: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorLayer {
    pub id: u32,
    pub app_id: u32,
    pub canvas_id: u32,
    pub z_order: u32,
    pub window: VirtualWindow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorScene {
    pub monitor_id: u32,
    pub metrics: MonitorMetrics,
    pub layers: Vec<MonitorLayer>,
    next_layer_id: u32,
}

impl MonitorScene {
    fn new(monitor_id: u32, metrics: MonitorMetrics) -> Self {
        Self {
            monitor_id,
            metrics,
            layers: Vec::new(),
            next_layer_id: 1,
        }
    }

    pub fn contains_app(&self, app_id: u32) -> bool {
        self.layers.iter().any(|layer| layer.app_id == app_id)
    }

    fn upsert_layer(&mut self, app_id: u32, canvas_id: u32, window: VirtualWindow) -> u32 {
        if let Some(layer) = self
            .layers
            .iter_mut()
            .find(|layer| layer.app_id == app_id && layer.canvas_id == canvas_id)
        {
            layer.window = window;
            return layer.id;
        }

        let id = self.next_layer_id;
        self.next_layer_id = self.next_layer_id.saturating_add(1);
        let z_order = self
            .layers
            .iter()
            .map(|layer| layer.z_order)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.layers.push(MonitorLayer {
            id,
            app_id,
            canvas_id,
            z_order,
            window,
        });
        id
    }

    fn remove_app_layers(&mut self, app_id: u32) {
        self.layers.retain(|layer| layer.app_id != app_id);
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }
}

pub struct Canvas {
    pub id: u32,
    pub owner_app_id: u32,
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
    pub attached_monitor_ids: HashSet<u32>,
}

pub struct App {
    pub id: u32,
    pub pid: u32,
    pub handle: HANDLE,
    pub canvas_ids: Vec<u32>, // Canvas IDs owned by this app
    pub command_ringbuffer: Option<SharedMemory>,
    pub bitmap_handles: HashMap<u32, BitmapHandle>,
}

pub struct Monitor {
    pub id: u32,
    pub pid: u32,
    pub handle: HANDLE,
    pub tx: tokio::sync::mpsc::UnboundedSender<ControlMessage>,
    pub kind: MonitorKind,
    pub owner_app_id: Option<u32>,
    pub target_canvas_id: Option<u32>,
    pub start_request_id: Option<u32>,
    pub core_managed: bool,
    pub manual_lifecycle: bool,
    pub mode: DesktopWindowMode,
    pub flags: u32,
}

pub struct ServerState {
    pub devices: Arc<CoreDevices>,
    pub apps: HashMap<u32, App>,         // Keyed by App ID
    pub monitors: HashMap<u32, Monitor>, // Keyed by Monitor ID
    pub canvases: HashMap<u32, Canvas>,  // Keyed by Canvas ID
    pub monitor_scenes: HashMap<u32, MonitorScene>,
    pub monitor_scene_outputs: HashMap<u32, CompositeOutputResources>,

    next_app_id: u32,
    next_monitor_id: u32,
    next_canvas_id: u32,
}

unsafe impl Send for ServerState {}
unsafe impl Sync for ServerState {}

impl ServerState {
    pub fn new() -> anyhow::Result<Self> {
        let devices = Arc::new(CoreDevices::new()?);
        Ok(Self {
            devices,
            apps: HashMap::new(),
            monitors: HashMap::new(),
            canvases: HashMap::new(),
            monitor_scenes: HashMap::new(),
            monitor_scene_outputs: HashMap::new(),
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
                bitmap_handles: HashMap::new(),
            },
        );
        Ok(id)
    }

    pub fn register_monitor(
        &mut self,
        pid: u32,
        handle: HANDLE,
        tx: tokio::sync::mpsc::UnboundedSender<ControlMessage>,
        kind: MonitorKind,
        owner_app_id: Option<u32>,
        request_id: Option<u32>,
        target_canvas_id: Option<u32>,
        mode: DesktopWindowMode,
        flags: u32,
        manual_lifecycle: bool,
    ) -> (u32, bool) {
        let id = self.next_monitor_id;
        self.next_monitor_id += 1;
        let core_managed =
            kind == MonitorKind::DesktopWindow && owner_app_id.is_some() && !manual_lifecycle;
        self.monitors.insert(
            id,
            Monitor {
                id,
                pid,
                handle,
                tx,
                kind,
                owner_app_id,
                target_canvas_id,
                start_request_id: request_id,
                core_managed,
                manual_lifecycle,
                mode,
                flags,
            },
        );

        let Some(app_id) = owner_app_id else {
            return (id, false);
        };
        if !self.apps.contains_key(&app_id) {
            return (id, true);
        }

        match target_canvas_id {
            Some(canvas_id) => {
                if !self.app_owns_canvas(app_id, canvas_id) {
                    return (id, true);
                }
                if let Err(e) = self.attach_monitor(canvas_id, id) {
                    eprintln!(
                        "attach monitor {} to canvas {} failed: {}",
                        id, canvas_id, e
                    );
                    return (id, true);
                }
                if let Err(e) = self.attach_canvas_to_monitor_scene(canvas_id, id) {
                    eprintln!(
                        "add monitor scene layer for monitor {} canvas {} failed: {}",
                        id, canvas_id, e
                    );
                    return (id, true);
                }
                if let Err(e) = self.ensure_monitor_scene_output_and_notify(id) {
                    eprintln!(
                        "create monitor scene output for monitor {} canvas {} failed: {}",
                        id, canvas_id, e
                    );
                    return (id, true);
                }
            }
            None if core_managed => return (id, true),
            None => {}
        }

        (id, false)
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
                    owner_app_id: owner_id,
                    owner_pid: app.pid,
                    logical_w,
                    logical_h,
                    resources,
                    per_monitor_surfaces: HashMap::new(),
                    attached_monitor_ids: HashSet::new(),
                },
            );
            app.canvas_ids.push(id);

            Ok(id)
        } else {
            Err(anyhow::anyhow!("App not found"))
        }
    }

    pub fn app_owns_canvas(&self, app_id: u32, canvas_id: u32) -> bool {
        self.apps
            .get(&app_id)
            .is_some_and(|app| app.canvas_ids.contains(&canvas_id))
    }

    pub fn resolve_app_canvas_id(&self, app_id: u32, requested_canvas_id: u32) -> Option<u32> {
        if requested_canvas_id == 0 {
            self.apps
                .get(&app_id)
                .and_then(|app| app.canvas_ids.first().copied())
        } else {
            self.app_owns_canvas(app_id, requested_canvas_id)
                .then_some(requested_canvas_id)
        }
    }

    pub fn add_canvas_layer_to_monitor_scene_for_app(
        &mut self,
        app_id: u32,
        requested_canvas_id: u32,
        monitor_id: u32,
    ) -> anyhow::Result<u32> {
        let canvas_id = self
            .resolve_app_canvas_id(app_id, requested_canvas_id)
            .ok_or_else(|| anyhow::anyhow!("App {app_id} does not own requested Canvas"))?;
        let monitor = self
            .monitors
            .get(&monitor_id)
            .ok_or_else(|| anyhow::anyhow!("Monitor not found"))?;
        if let Some(owner_app_id) = monitor.owner_app_id {
            if owner_app_id != app_id {
                anyhow::bail!("App {app_id} does not own Monitor {monitor_id}");
            }
        } else if !monitor.manual_lifecycle {
            anyhow::bail!(
                "Ownerless Monitor {monitor_id} is not a shared manual lifecycle monitor"
            );
        }

        self.attach_canvas_to_monitor_scene(canvas_id, monitor_id)
    }

    fn attach_canvas_to_monitor_scene(
        &mut self,
        canvas_id: u32,
        monitor_id: u32,
    ) -> anyhow::Result<u32> {
        if !self.monitors.contains_key(&monitor_id) {
            anyhow::bail!("Monitor not found");
        }

        let (owner_app_id, metrics, window) = {
            let canvas = self
                .canvases
                .get(&canvas_id)
                .ok_or_else(|| anyhow::anyhow!("Canvas not found"))?;
            (
                canvas.owner_app_id,
                MonitorMetrics::from_canvas(canvas),
                VirtualWindow::from_canvas(canvas),
            )
        };

        let layer_id = self
            .monitor_scenes
            .entry(monitor_id)
            .or_insert_with(|| MonitorScene::new(monitor_id, metrics))
            .upsert_layer(owner_app_id, canvas_id, window);

        if let Some(canvas) = self.canvases.get_mut(&canvas_id) {
            canvas.attached_monitor_ids.insert(monitor_id);
        }

        Ok(layer_id)
    }

    pub fn ensure_monitor_scene_output_and_notify(
        &mut self,
        monitor_id: u32,
    ) -> anyhow::Result<bool> {
        if self.monitor_scene_outputs.contains_key(&monitor_id) {
            return Ok(false);
        }

        let scene = self
            .monitor_scenes
            .get(&monitor_id)
            .ok_or_else(|| anyhow::anyhow!("MonitorScene not found"))?;
        let output = CompositeOutputResources::new(
            &self.devices.d3d,
            scene.metrics.logical_w,
            scene.metrics.logical_h,
        )?;
        {
            let guard = self.devices.render_ctx.lock().unwrap();
            let _ = output.present_color(&guard.d3d_ctx, [0.0, 0.0, 0.0, 0.0]);
        }
        self.monitor_scene_outputs.insert(monitor_id, output);

        if let Err(e) = self.notify_monitor_composite_attached(monitor_id) {
            self.monitor_scene_outputs.remove(&monitor_id);
            return Err(e);
        }

        Ok(true)
    }

    fn notify_monitor_composite_attached(&self, monitor_id: u32) -> anyhow::Result<()> {
        let scene = self
            .monitor_scenes
            .get(&monitor_id)
            .ok_or_else(|| anyhow::anyhow!("MonitorScene not found"))?;
        let output = self
            .monitor_scene_outputs
            .get(&monitor_id)
            .ok_or_else(|| anyhow::anyhow!("MonitorScene output not found"))?;
        let monitor = self
            .monitors
            .get(&monitor_id)
            .ok_or_else(|| anyhow::anyhow!("Monitor not found"))?;

        let monitor_proc = unsafe {
            windows::Win32::System::Threading::OpenProcess(
                windows::Win32::System::Threading::PROCESS_DUP_HANDLE,
                false,
                monitor.pid,
            )?
        };
        let mut dup_output: HANDLE = HANDLE::default();
        let cur_proc = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
        let dup_result = unsafe {
            windows::Win32::Foundation::DuplicateHandle(
                cur_proc,
                output.handle,
                monitor_proc,
                &mut dup_output,
                0,
                false,
                windows::Win32::Foundation::DUPLICATE_SAME_ACCESS,
            )
        };
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(monitor_proc);
        }
        dup_result?;

        monitor.tx.send(ControlMessage::MonitorCompositeAttached {
            monitor_id,
            scene_id: scene.monitor_id,
            surface_handle: dup_output.0 as u64,
            logical_w: output.logical_w,
            logical_h: output.logical_h,
            render_w: output.render_w,
            render_h: output.render_h,
        })?;
        Ok(())
    }

    pub fn attach_game_bar_monitor_for_app(
        &mut self,
        app_id: u32,
        requested_canvas_id: u32,
        count: u32,
    ) -> (MonitorRequestStatus, Vec<u32>) {
        let Some(canvas_id) = self.resolve_app_canvas_id(app_id, requested_canvas_id) else {
            return (MonitorRequestStatus::InvalidCanvas, Vec::new());
        };

        let mut candidates: Vec<u32> = self
            .monitors
            .iter()
            .filter_map(|(id, monitor)| {
                (monitor.kind == MonitorKind::GameBar
                    && monitor.owner_app_id.is_none()
                    && monitor.manual_lifecycle)
                    .then_some(*id)
            })
            .collect();
        candidates.sort_unstable();
        candidates.truncate(count as usize);

        if candidates.is_empty() {
            return (MonitorRequestStatus::ManualOpenRequired, Vec::new());
        }

        let mut attached = Vec::new();
        for monitor_id in candidates {
            let needs_surface_handoff = !self.monitor_attached_to_any_canvas(monitor_id);
            match self.add_canvas_layer_to_monitor_scene_for_app(app_id, canvas_id, monitor_id) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!(
                        "add Game Bar scene layer for app {} canvas {} monitor {} failed: {}",
                        app_id, canvas_id, monitor_id, e
                    );
                    continue;
                }
            }

            if needs_surface_handoff {
                match self.attach_monitor(canvas_id, monitor_id) {
                    Ok(()) => attached.push(monitor_id),
                    Err(e) => eprintln!(
                        "attach Game Bar monitor {} to canvas {} failed: {}",
                        monitor_id, canvas_id, e
                    ),
                }
            } else {
                attached.push(monitor_id);
            }

            if let Err(e) = self.ensure_monitor_scene_output_and_notify(monitor_id) {
                eprintln!(
                    "create Game Bar composite output for monitor {} failed: {}",
                    monitor_id, e
                );
            }
        }

        if attached.is_empty() {
            (MonitorRequestStatus::SpawnFailed, Vec::new())
        } else {
            (MonitorRequestStatus::Ok, attached)
        }
    }

    fn monitor_attached_to_any_canvas(&self, monitor_id: u32) -> bool {
        self.canvases
            .values()
            .any(|canvas| canvas.attached_monitor_ids.contains(&monitor_id))
    }

    pub fn close_owned_desktop_monitors(
        &self,
        app_id: u32,
    ) -> Vec<(u32, tokio::sync::mpsc::UnboundedSender<ControlMessage>)> {
        self.monitors
            .iter()
            .filter_map(|(id, monitor)| {
                (monitor.kind == MonitorKind::DesktopWindow
                    && monitor.owner_app_id == Some(app_id)
                    && monitor.core_managed)
                    .then_some((*id, monitor.tx.clone()))
            })
            .collect()
    }

    pub fn close_monitor_if_owned(
        &self,
        app_id: u32,
        monitor_id: u32,
    ) -> (
        crate::ipc::protocol::MonitorRequestStatus,
        Option<tokio::sync::mpsc::UnboundedSender<ControlMessage>>,
    ) {
        let Some(monitor) = self.monitors.get(&monitor_id) else {
            return (crate::ipc::protocol::MonitorRequestStatus::NotFound, None);
        };
        if monitor.owner_app_id != Some(app_id) {
            return (crate::ipc::protocol::MonitorRequestStatus::NotOwner, None);
        }
        if !monitor.core_managed || monitor.manual_lifecycle {
            return (
                crate::ipc::protocol::MonitorRequestStatus::NotCoreManaged,
                None,
            );
        }
        (
            crate::ipc::protocol::MonitorRequestStatus::Ok,
            Some(monitor.tx.clone()),
        )
    }

    pub fn remove_app(&mut self, id: u32) {
        if let Some(app) = self.apps.remove(&id) {
            for scene in self.monitor_scenes.values_mut() {
                scene.remove_app_layers(id);
            }
            self.monitor_scenes.retain(|_, scene| !scene.is_empty());
            self.monitor_scene_outputs
                .retain(|monitor_id, _| self.monitor_scenes.contains_key(monitor_id));

            // Task 3.3 / Preservation 3.5: release every
            // `PerMonitorResources` along with the World `CanvasResources`
            // for each owned canvas.
            for canvas_id in &app.canvas_ids {
                if let Some(canvas) = self.canvases.get(canvas_id) {
                    let _notified: Vec<u32> = canvas.per_monitor_surfaces.keys().copied().collect();
                    // Future: send ControlMessage::CanvasDetached to each
                    // monitor in `_notified`. Today the drop below is the
                    // observable cleanup.
                }
            }
            if !app.bitmap_handles.is_empty() {
                let guard = self.devices.render_ctx.lock().unwrap();
                for handle in app.bitmap_handles.into_values() {
                    let _ = guard.d2d.destroy_bitmap(handle);
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
            canvas.attached_monitor_ids.remove(&id);
        }
        self.monitor_scenes.remove(&id);
        self.monitor_scene_outputs.remove(&id);
        self.monitors.remove(&id);
    }

    pub fn attach_monitor_for_app(
        &mut self,
        app_id: u32,
        canvas_id: u32,
        monitor_id: u32,
    ) -> anyhow::Result<()> {
        if !self.app_owns_canvas(app_id, canvas_id) {
            anyhow::bail!("App {app_id} does not own Canvas {canvas_id}");
        }

        let monitor = self
            .monitors
            .get(&monitor_id)
            .ok_or_else(|| anyhow::anyhow!("Monitor not found"))?;
        if monitor.owner_app_id != Some(app_id) {
            anyhow::bail!("App {app_id} does not own Monitor {monitor_id}");
        }
        if let Some(target_canvas_id) = monitor.target_canvas_id {
            if target_canvas_id != canvas_id {
                anyhow::bail!(
                    "Monitor {monitor_id} targets Canvas {target_canvas_id}, not Canvas {canvas_id}"
                );
            }
        }

        self.add_canvas_layer_to_monitor_scene_for_app(app_id, canvas_id, monitor_id)?;
        self.attach_monitor(canvas_id, monitor_id)?;
        self.ensure_monitor_scene_output_and_notify(monitor_id)?;
        Ok(())
    }

    pub fn attach_monitor(&mut self, canvas_id: u32, monitor_id: u32) -> anyhow::Result<()> {
        // First phase: borrow-immutably to read Canvas metadata and the
        // World surface handle; build the CanvasAttached message.
        let (surface_handle, logical_w, logical_h, render_w, render_h) = {
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
        if monitor_tx.send(world_msg).is_err() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(monitor_proc);
            }
            return Ok(());
        }
        if let Some(canvas) = self.canvases.get_mut(&canvas_id) {
            canvas.attached_monitor_ids.insert(monitor_id);
        }

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
                            let _ = windows::Win32::Foundation::CloseHandle(monitor_proc);
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

        // Send MonitorLocalSurfaceAttached immediately after CanvasAttached.
        // It is part of the current protocol; protocol drift is rejected by
        // the decoder instead of silently downgrading.
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
    pub static ref SERVER_STATE: RwLock<ServerState> = RwLock::new(
        ServerState::new().expect("Failed to initialize ServerState")
    );
}
