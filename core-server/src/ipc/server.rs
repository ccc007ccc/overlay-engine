use std::collections::HashMap;

use parking_lot::RwLock;
use windows::Win32::Foundation::HANDLE;

use crate::renderer::dcomp::{CanvasResources, CoreDevices};
use crate::ipc::shmem::SharedMemory;

pub struct Canvas {
    pub id: u32,
    pub owner_pid: u32,
    pub logical_w: u32,
    pub logical_h: u32,
    pub resources: CanvasResources,
}

pub struct Producer {
    pub id: u32,
    pub pid: u32,
    pub handle: HANDLE,
    pub canvases: Vec<u32>, // Canvas IDs owned by this producer
    pub command_ringbuffer: Option<SharedMemory>,
}

pub struct Consumer {
    pub id: u32,
    pub pid: u32,
    pub handle: HANDLE,
    pub tx: tokio::sync::mpsc::UnboundedSender<crate::ipc::protocol::ControlMessage>,
}

pub struct ServerState {
    pub devices: CoreDevices,
    pub producers: HashMap<u32, Producer>, // Keyed by Producer ID
    pub consumers: HashMap<u32, Consumer>, // Keyed by Consumer ID
    pub canvases: HashMap<u32, Canvas>,    // Keyed by Canvas ID

    next_producer_id: u32,
    next_consumer_id: u32,
    next_canvas_id: u32,
}

unsafe impl Send for ServerState {}
unsafe impl Sync for ServerState {}

impl ServerState {
    pub fn new() -> anyhow::Result<Self> {
        let devices = CoreDevices::new()?;
        Ok(Self {
            devices,
            producers: HashMap::new(),
            consumers: HashMap::new(),
            canvases: HashMap::new(),
            next_producer_id: 1,
            next_consumer_id: 1,
            next_canvas_id: 1,
        })
    }

    pub fn register_producer(&mut self, pid: u32, handle: HANDLE) -> anyhow::Result<u32> {
        let id = self.next_producer_id;
        self.next_producer_id += 1;

        let shmem_name = format!("overlay-core-cmds-{}", pid);
        let command_ringbuffer = SharedMemory::create(&shmem_name, 4 * 1024 * 1024)?; // 4MB ringbuffer

        self.producers.insert(
            id,
            Producer {
                id,
                pid,
                handle,
                canvases: Vec::new(),
                command_ringbuffer: Some(command_ringbuffer),
            },
        );
        Ok(id)
    }

    pub fn register_consumer(&mut self, pid: u32, handle: HANDLE, tx: tokio::sync::mpsc::UnboundedSender<crate::ipc::protocol::ControlMessage>) -> u32 {
        let id = self.next_consumer_id;
        self.next_consumer_id += 1;
        self.consumers.insert(
            id,
            Consumer {
                id,
                pid,
                handle,
                tx,
            },
        );
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
        if let Some(producer) = self.producers.get_mut(&owner_id) {
            let id = self.next_canvas_id;
            self.next_canvas_id += 1;

            let resources = CanvasResources::new(&self.devices.d3d, render_w, render_h)?;

            // Fill canvas with an initial color (e.g. semi-transparent black or clear)
            let _ = resources.present_color(&self.devices.d3d_ctx, [0.0, 0.0, 0.0, 0.0]);

            self.canvases.insert(
                id,
                Canvas {
                    id,
                    owner_pid: producer.pid,
                    logical_w,
                    logical_h,
                    resources,
                },
            );
            producer.canvases.push(id);
            Ok(id)
        } else {
            Err(anyhow::anyhow!("Producer not found"))
        }
    }

    pub fn remove_producer(&mut self, id: u32) {
        if let Some(producer) = self.producers.remove(&id) {
            for canvas_id in producer.canvases {
                self.canvases.remove(&canvas_id);
            }
        }
    }

    pub fn remove_consumer(&mut self, id: u32) {
        self.consumers.remove(&id);
    }

    pub fn attach_consumer(&mut self, canvas_id: u32, consumer_id: u32) -> anyhow::Result<()> {
        let canvas = self.canvases.get(&canvas_id).ok_or_else(|| anyhow::anyhow!("Canvas not found"))?;
        let surface_handle = canvas.resources.handle;
        let logical_w = canvas.logical_w;
        let logical_h = canvas.logical_h;
        let render_w = canvas.resources.render_w;
        let render_h = canvas.resources.render_h;

        let consumer = self.consumers.get(&consumer_id).ok_or_else(|| anyhow::anyhow!("Consumer not found"))?;

        // Duplicate the handle into the consumer's process
        let mut dup_handle: HANDLE = HANDLE::default();
        let cur_proc = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
        let consumer_proc = unsafe { windows::Win32::System::Threading::OpenProcess(windows::Win32::System::Threading::PROCESS_DUP_HANDLE, false, consumer.pid)? };

        unsafe {
            windows::Win32::Foundation::DuplicateHandle(
                cur_proc,
                surface_handle,
                consumer_proc,
                &mut dup_handle,
                0,
                false,
                windows::Win32::Foundation::DUPLICATE_SAME_ACCESS,
            )?;
            let _ = windows::Win32::Foundation::CloseHandle(consumer_proc);
        }

        // Send CanvasAttached message to the consumer
        let msg = crate::ipc::protocol::ControlMessage::CanvasAttached {
            canvas_id,
            surface_handle: dup_handle.0 as u64,
            logical_w,
            logical_h,
            render_w,
            render_h,
        };

        let _ = consumer.tx.send(msg);

        Ok(())
    }
}

// Global server state wrapped in an RwLock.
lazy_static::lazy_static! {
    pub static ref SERVER_STATE: RwLock<ServerState> = RwLock::new(ServerState::new().expect("Failed to initialize ServerState"));
}
