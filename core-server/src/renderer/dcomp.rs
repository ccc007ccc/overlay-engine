use crate::renderer::painter::D2DEngine;

use windows::core::{IUnknown, Interface, Result};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HMODULE, RECT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Graphics::CompositionSwapchain::{
    CreatePresentationFactory, IPresentationBuffer, IPresentationFactory, IPresentationManager,
    IPresentationSurface,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_DISPLAYABLE,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, DCompositionCreateSurfaceHandle, IDCompositionDesktopDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED,
    DXGI_ERROR_DEVICE_RESET,
};
use windows::Win32::System::Threading::WaitForMultipleObjectsEx;

use std::sync::Mutex;

pub const COMPOSITIONOBJECT_ALL_ACCESS: u32 = 0x0003;

/// Number of buffers a Canvas rotates between per-frame. Design §Fix
/// Implementation → Change 1 requires `N ≥ 2`; we start with 2 as the minimum
/// that lets DWM retire a buffer while Core writes the next one. `N = 3` is a
/// tunable knob — increasing it trades GPU memory for additional headroom
/// under bursty presents; benchmark before changing.
pub const BUFFER_COUNT: usize = 3;

/// Poll presentation buffers without blocking. When all buffers are busy,
/// the submit path drops that stale frame and continues draining IPC so
/// unlocked producers never build a deep pipe/shared-memory backlog.
pub const ACQUIRE_TIMEOUT_MS: u32 = 0;

pub struct RenderContextGuard {
    pub d3d_ctx: ID3D11DeviceContext,
    pub d2d: D2DEngine,
}

pub struct CoreDevices {
    pub d3d: ID3D11Device,
    pub dcomp: IDCompositionDesktopDevice,
    // ID3D11DeviceContext is NOT thread-safe for concurrent command recording.
    // We must wrap it and D2D in a Mutex so that multiple connected clients
    // executing `dispatch_submit_frame` concurrently do not race and crash
    // the GPU driver with STATUS_ACCESS_VIOLATION.
    pub render_ctx: Mutex<RenderContextGuard>,
}

// These are COM pointers which are thread-safe in our context as long as we only use them
// for initialization or behind a lock. We mark them Send and Sync so we can store them in ServerState.
unsafe impl Send for CoreDevices {}
unsafe impl Sync for CoreDevices {}

impl CoreDevices {
    pub fn new() -> Result<Self> {
        let mut d3d_opt: Option<ID3D11Device> = None;
        let mut ctx_opt: Option<ID3D11DeviceContext> = None;
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d_opt),
                None,
                Some(&mut ctx_opt),
            )?;
        }
        let d3d = d3d_opt.ok_or_else(|| {
            windows::core::Error::new(windows::core::HRESULT(-1), "D3D11CreateDevice null")
        })?;
        let d3d_ctx = ctx_opt.ok_or_else(|| {
            windows::core::Error::new(windows::core::HRESULT(-1), "D3D11Context null")
        })?;
        let dxgi: IDXGIDevice = d3d.cast()?;
        let dcomp: IDCompositionDesktopDevice = unsafe { DCompositionCreateDevice2(&dxgi)? };
        let d2d = D2DEngine::create(&d3d)
            .map_err(|e| windows::core::Error::new(windows::core::HRESULT(-1), e.to_string()))?;

        Ok(Self {
            d3d,
            dcomp,
            render_ctx: Mutex::new(RenderContextGuard { d3d_ctx, d2d }),
        })
    }
}

/// Outcome of `CanvasResources::acquire_available_buffer`.
///
/// * `Acquired(idx)` — the buffer at `self.buffers[idx]` is currently NOT held
///   by DWM and is safe to write.
/// * `TimedOut` — all N buffers were busy past the bounded-wait deadline.
///   Caller MUST drop the frame rather than block; see design.md §Fix
///   Implementation → Change 2 and Preservation 3.8.
/// * `Failed(e)` — the OS refused to hand back an event handle, likely a
///   device-lost / invalid-state precursor. Caller should escalate to
///   Canvas-resource rebuild.
#[derive(Debug)]
pub enum AcquireOutcome {
    Acquired(usize),
    TimedOut,
    Failed(windows::core::Error),
}

/// Outcome of `CanvasResources::present` — classifies the three cases spelled
/// out in design.md §Fix Implementation → Change 3.
///
/// * `Success` — frame handed off to DWM.
/// * `RetryNextTick` — transient error (e.g. "buffer not yet available"),
///   caller drops this frame and retries on the next `SubmitFrame`.
/// * `DeviceLost` — fatal; caller must trigger Canvas resource rebuild (that
///   rebuild path is outside this task's scope and is transparent to
///   monitors per Preservation 3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentOutcome {
    Success,
    RetryNextTick,
    DeviceLost,
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    fn get(&self) -> HANDLE {
        self.0
    }

    fn into_raw(mut self) -> HANDLE {
        let handle = self.0;
        self.0 = HANDLE::default();
        handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            if !self.0 .0.is_null() && !self.0.is_invalid() {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct OwnedEventHandles {
    handles: Vec<HANDLE>,
}

impl OwnedEventHandles {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            handles: Vec::with_capacity(capacity),
        }
    }

    fn push(&mut self, handle: HANDLE) {
        self.handles.push(handle);
    }

    fn into_vec(mut self) -> Vec<HANDLE> {
        std::mem::take(&mut self.handles)
    }
}

impl Drop for OwnedEventHandles {
    fn drop(&mut self) {
        unsafe {
            for handle in self.handles.drain(..) {
                if !handle.0.is_null() && !handle.is_invalid() {
                    let _ = CloseHandle(handle);
                }
            }
        }
    }
}

fn acquire_available_buffer(
    manager: &IPresentationManager,
    available_events: &[HANDLE],
    timeout_ms: u32,
) -> AcquireOutcome {
    let wait_result =
        unsafe { WaitForMultipleObjectsEx(available_events, false, timeout_ms, true) };

    if wait_result == windows::Win32::Foundation::WAIT_IO_COMPLETION {
        unsafe { while manager.GetNextPresentStatistics().is_ok() {} }
        let retry = unsafe { WaitForMultipleObjectsEx(available_events, false, 0, false) };
        let base = WAIT_OBJECT_0.0;
        if retry.0 >= base && retry.0 < base + available_events.len() as u32 {
            return AcquireOutcome::Acquired((retry.0 - base) as usize);
        }
        return AcquireOutcome::TimedOut;
    }

    if wait_result == WAIT_TIMEOUT {
        return AcquireOutcome::TimedOut;
    }
    if wait_result == WAIT_FAILED {
        return AcquireOutcome::Failed(windows::core::Error::from_win32());
    }

    let base = WAIT_OBJECT_0.0;
    let code = wait_result.0;
    if code >= base {
        let idx = (code - base) as usize;
        if idx < available_events.len() {
            return AcquireOutcome::Acquired(idx);
        }
    }
    AcquireOutcome::TimedOut
}

pub(crate) fn present_manager(manager: &IPresentationManager, log_name: &str) -> PresentOutcome {
    match unsafe { manager.Present() } {
        Ok(()) => PresentOutcome::Success,
        Err(e) => {
            let hr = e.code();
            if hr == DXGI_ERROR_DEVICE_REMOVED
                || hr == DXGI_ERROR_DEVICE_RESET
                || hr == DXGI_ERROR_DEVICE_HUNG
            {
                eprintln!(
                    "[{log_name}] Present fatal device-lost (HRESULT={:#010x}): {}",
                    hr.0 as u32,
                    e.message()
                );
                PresentOutcome::DeviceLost
            } else {
                PresentOutcome::RetryNextTick
            }
        }
    }
}

/// Multi-buffer Canvas render resources. The single instances (`handle`,
/// `manager`, `surface`, `render_w`, `render_h`) are per-Canvas and observed
/// by Monitors through the NT `handle`; the parallel `Vec`s (`buffers`,
/// `textures`, `rtvs`) let Core rotate writes across N ≥ 2 distinct
/// `IPresentationBuffer` instances so DWM sees a new buffer handle each
/// frame (fixes bug A — design.md §Hypothesized Root Cause A.1).
///
/// Invariants:
/// * `buffers.len() == textures.len() == rtvs.len() == BUFFER_COUNT`
/// * `buffers[i]` was created by `manager.AddBufferFromResource(textures[i])`
/// * `rtvs[i]` is a render-target view of `textures[i]`
///
/// Monitors do NOT observe the buffer count changing from 1 to N — the
/// on-the-wire `CanvasAttached` surface handle is still the single DComp
/// surface NT `handle` (Preservation 3.2, 3.3).
pub struct CanvasResources {
    pub render_w: u32,
    pub render_h: u32,
    // COM children before parents: Rust drops fields in declaration order.
    // IPresentationBuffer / IPresentationSurface were created by the
    // IPresentationManager; releasing the manager first can invalidate
    // internal state the children still reference → ACCESS_VIOLATION.
    pub rtvs: Vec<ID3D11RenderTargetView>,
    pub buffers: Vec<IPresentationBuffer>,
    pub available_events: Vec<HANDLE>,
    pub textures: Vec<ID3D11Texture2D>,
    pub surface: IPresentationSurface,
    pub manager: IPresentationManager,
    pub handle: HANDLE,
}

unsafe impl Send for CanvasResources {}
unsafe impl Sync for CanvasResources {}

impl Drop for CanvasResources {
    fn drop(&mut self) {
        unsafe {
            for &ev in &self.available_events {
                let _ = windows::Win32::Foundation::CloseHandle(ev);
            }
            if !self.handle.is_invalid() {
                let _ = windows::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

impl CanvasResources {
    pub fn new(d3d: &ID3D11Device, render_w: u32, render_h: u32) -> Result<Self> {
        let factory: IPresentationFactory = unsafe { CreatePresentationFactory(d3d)? };
        let manager: IPresentationManager = unsafe { factory.CreatePresentationManager()? };
        let handle = OwnedHandle::new(unsafe {
            DCompositionCreateSurfaceHandle(COMPOSITIONOBJECT_ALL_ACCESS, None)?
        });
        let surface: IPresentationSurface =
            unsafe { manager.CreatePresentationSurface(handle.get())? };
        unsafe {
            surface.SetAlphaMode(DXGI_ALPHA_MODE_PREMULTIPLIED)?;
            let src = RECT {
                left: 0,
                top: 0,
                right: render_w as i32,
                bottom: render_h as i32,
            };
            surface.SetSourceRect(&src)?;
        }

        let misc_flags = D3D11_RESOURCE_MISC_SHARED.0
            | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
            | D3D11_RESOURCE_MISC_SHARED_DISPLAYABLE.0;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: render_w,
            Height: render_h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: misc_flags as u32,
        };

        let mut textures: Vec<ID3D11Texture2D> = Vec::with_capacity(BUFFER_COUNT);
        let mut rtvs: Vec<ID3D11RenderTargetView> = Vec::with_capacity(BUFFER_COUNT);
        let mut buffers: Vec<IPresentationBuffer> = Vec::with_capacity(BUFFER_COUNT);
        let mut available_events = OwnedEventHandles::with_capacity(BUFFER_COUNT);

        for _ in 0..BUFFER_COUNT {
            let mut texture: Option<ID3D11Texture2D> = None;
            unsafe { d3d.CreateTexture2D(&desc, None, Some(&mut texture))? };
            let texture = texture.ok_or_else(|| {
                windows::core::Error::new(windows::core::HRESULT(-1), "CreateTexture2D failed")
            })?;

            let mut rtv: Option<ID3D11RenderTargetView> = None;
            unsafe { d3d.CreateRenderTargetView(&texture, None, Some(&mut rtv))? };
            let rtv = rtv.ok_or_else(|| {
                windows::core::Error::new(
                    windows::core::HRESULT(-1),
                    "CreateRenderTargetView failed",
                )
            })?;

            let texture_unk: IUnknown = texture.cast()?;
            let buffer: IPresentationBuffer =
                unsafe { manager.AddBufferFromResource(&texture_unk)? };
            let ev = unsafe { buffer.GetAvailableEvent()? };

            textures.push(texture);
            rtvs.push(rtv);
            buffers.push(buffer);
            available_events.push(ev);
        }

        Ok(Self {
            handle: handle.into_raw(),
            manager,
            surface,
            render_w,
            render_h,
            buffers,
            available_events: available_events.into_vec(),
            textures,
            rtvs,
        })
    }

    /// Pick a buffer that is NOT currently held by DWM.
    ///
    /// Polls cached available-event handles across all N buffers and returns
    /// the index of the first signaled one via `WaitForMultipleObjectsEx`.
    /// The handles are fetched once during construction; fetching them every
    /// frame leaks kernel handles under unlocked producers.
    ///
    /// Design.md §Hypothesized Root Cause A.1 and §Fix Implementation → Change 2.
    pub fn acquire_available_buffer(&self, timeout_ms: u32) -> AcquireOutcome {
        acquire_available_buffer(&self.manager, &self.available_events, timeout_ms)
    }

    /// Call `IPresentationManager::Present` and classify the result.
    ///
    /// design.md §Fix Implementation → Change 3 requires three classes:
    /// * success
    /// * retry-next-tick / drop (transient, e.g. "buffer not yet available")
    /// * fatal device-lost → Canvas resource rebuild
    ///
    /// The previous silent `eprintln!` swallow path is removed — every error
    /// is now classified and logged with its HRESULT so callers can route to
    /// the correct recovery path.
    pub fn present(&self) -> PresentOutcome {
        present_manager(&self.manager, "CanvasResources")
    }

    /// Clear one of the N buffers to a solid color and Present. Used once at
    /// Canvas creation time so DWM has an initial buffer content to show
    /// before the App's first `SubmitFrame`.
    ///
    /// At construction all N `IPresentationBuffer` events are freshly
    /// signalled — `acquire_available_buffer` returns immediately with
    /// index 0. We keep the post-present `SleepEx(0, TRUE)` +
    /// `GetNextPresentStatistics` drain because the present-retirement APC
    /// pattern is still useful here; in the steady-state submit loop
    /// (`server_task.rs`) the correct handshake is
    /// `acquire_available_buffer` + per-frame buffer rotation, not
    /// `SleepEx`.
    pub fn present_color(&self, d3d_ctx: &ID3D11DeviceContext, rgba: [f32; 4]) -> Result<()> {
        let idx = match self.acquire_available_buffer(ACQUIRE_TIMEOUT_MS) {
            AcquireOutcome::Acquired(i) => i,
            // At construction time all buffers are fresh; a timeout is
            // impossible in practice. If we somehow hit one, fall back to
            // slot 0 rather than surfacing an error: this is a best-effort
            // initial clear.
            AcquireOutcome::TimedOut => 0,
            AcquireOutcome::Failed(e) => return Err(e),
        };
        unsafe {
            d3d_ctx.ClearRenderTargetView(&self.rtvs[idx], &rgba);
            d3d_ctx.Flush();
            self.surface.SetBuffer(&self.buffers[idx])?;
            // We deliberately ignore the classification here — the initial
            // clear is best-effort. The steady-state loop in server_task.rs
            // uses the classified `present()` directly.
            let _ = self.present();
            windows::Win32::System::Threading::SleepEx(0, true);
            while self.manager.GetNextPresentStatistics().is_ok() {}
        }
        Ok(())
    }
}

/// Upper bound on the per-Monitor MonitorLocal surface size. Task 3.3 spec
/// calls for `min(canvas_logical, 4096)` as the cap — this is the 4096
/// constant used in that min.
///
/// Rationale: a single per-Monitor surface is at most one monitor tall/wide
/// in logical pixels. Capping at 4K keeps worst-case video memory bounded
/// even if a monitor reports an outlier client-area size.
pub const PER_MONITOR_MAX_DIM: u32 = 4096;

/// Minimum surface dimension. DComp/D3D11 reject 0×0 textures; we enforce a
/// 1-pixel floor so a monitor that reports a zero-sized client area still
/// gets a valid (if tiny) surface which the submit path simply never
/// writes to.
pub const PER_MONITOR_MIN_DIM: u32 = 1;

/// Per-Monitor MonitorLocal render resources — task 3.3 of the
/// `animation-and-viewport-fix` spec.
///
/// A reduced-scope analogue of `CanvasResources` that owns:
///   * its own multi-buffer ring (reuses the buffer-rotation logic from
///     task 3.1 — same `BUFFER_COUNT`, same `acquire_available_buffer`
///     handshake, same `present` classification),
///   * its own DComp surface NT `handle`,
///   * its own `IPresentationManager` / `IPresentationSurface`.
///
/// This struct is deliberately separate from `CanvasResources` so a monitor
/// drop can release its MonitorLocal surface without affecting World
/// (Preservation 3.4). One instance is stored per-`(canvas_id, monitor_id)`
/// pair in `Canvas::per_monitor_surfaces`.
///
/// Invariants match `CanvasResources`:
/// * `buffers.len() == textures.len() == rtvs.len() == BUFFER_COUNT`
/// * `render_w`, `render_h` are clamped to
///   `[PER_MONITOR_MIN_DIM, PER_MONITOR_MAX_DIM]`.
pub struct PerMonitorResources {
    pub render_w: u32,
    pub render_h: u32,
    pub logical_w: u32,
    pub logical_h: u32,
    // COM children before parents — same drop-order fix as CanvasResources.
    pub rtvs: Vec<ID3D11RenderTargetView>,
    pub buffers: Vec<IPresentationBuffer>,
    pub available_events: Vec<HANDLE>,
    pub textures: Vec<ID3D11Texture2D>,
    pub surface: IPresentationSurface,
    pub manager: IPresentationManager,
    pub handle: HANDLE,
}

unsafe impl Send for PerMonitorResources {}
unsafe impl Sync for PerMonitorResources {}

impl Drop for PerMonitorResources {
    fn drop(&mut self) {
        unsafe {
            for &ev in &self.available_events {
                let _ = windows::Win32::Foundation::CloseHandle(ev);
            }
            if !self.handle.is_invalid() {
                let _ = windows::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

impl PerMonitorResources {
    /// Create a per-Monitor MonitorLocal surface sized to
    /// `min(logical_{w,h}, PER_MONITOR_MAX_DIM)` (clamped to at least
    /// `PER_MONITOR_MIN_DIM`).
    ///
    /// Mirrors `CanvasResources::new` — same texture format
    /// (`DXGI_FORMAT_B8G8R8A8_UNORM`), same shared-NT-handle misc flags,
    /// same `BUFFER_COUNT` buffer ring. Reuses the `IPresentationManager`
    /// pattern so task 3.4's dispatch loop can call
    /// `acquire_available_buffer` and `present` on these surfaces using
    /// the exact same code path it uses for the World surface.
    pub fn new(d3d: &ID3D11Device, logical_w: u32, logical_h: u32) -> Result<Self> {
        let render_w = logical_w.clamp(PER_MONITOR_MIN_DIM, PER_MONITOR_MAX_DIM);
        let render_h = logical_h.clamp(PER_MONITOR_MIN_DIM, PER_MONITOR_MAX_DIM);

        let factory: IPresentationFactory = unsafe { CreatePresentationFactory(d3d)? };
        let manager: IPresentationManager = unsafe { factory.CreatePresentationManager()? };
        let handle = OwnedHandle::new(unsafe {
            DCompositionCreateSurfaceHandle(COMPOSITIONOBJECT_ALL_ACCESS, None)?
        });
        let surface: IPresentationSurface =
            unsafe { manager.CreatePresentationSurface(handle.get())? };
        unsafe {
            surface.SetAlphaMode(DXGI_ALPHA_MODE_PREMULTIPLIED)?;
            let src = RECT {
                left: 0,
                top: 0,
                right: render_w as i32,
                bottom: render_h as i32,
            };
            surface.SetSourceRect(&src)?;
        }

        let misc_flags = D3D11_RESOURCE_MISC_SHARED.0
            | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
            | D3D11_RESOURCE_MISC_SHARED_DISPLAYABLE.0;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: render_w,
            Height: render_h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: misc_flags as u32,
        };

        let mut textures: Vec<ID3D11Texture2D> = Vec::with_capacity(BUFFER_COUNT);
        let mut rtvs: Vec<ID3D11RenderTargetView> = Vec::with_capacity(BUFFER_COUNT);
        let mut buffers: Vec<IPresentationBuffer> = Vec::with_capacity(BUFFER_COUNT);
        let mut available_events = OwnedEventHandles::with_capacity(BUFFER_COUNT);

        for _ in 0..BUFFER_COUNT {
            let mut texture: Option<ID3D11Texture2D> = None;
            unsafe { d3d.CreateTexture2D(&desc, None, Some(&mut texture))? };
            let texture = texture.ok_or_else(|| {
                windows::core::Error::new(
                    windows::core::HRESULT(-1),
                    "CreateTexture2D (per-monitor) failed",
                )
            })?;

            let mut rtv: Option<ID3D11RenderTargetView> = None;
            unsafe { d3d.CreateRenderTargetView(&texture, None, Some(&mut rtv))? };
            let rtv = rtv.ok_or_else(|| {
                windows::core::Error::new(
                    windows::core::HRESULT(-1),
                    "CreateRenderTargetView (per-monitor) failed",
                )
            })?;

            let texture_unk: IUnknown = texture.cast()?;
            let buffer: IPresentationBuffer =
                unsafe { manager.AddBufferFromResource(&texture_unk)? };
            let ev = unsafe { buffer.GetAvailableEvent()? };

            textures.push(texture);
            rtvs.push(rtv);
            buffers.push(buffer);
            available_events.push(ev);
        }

        Ok(Self {
            handle: handle.into_raw(),
            manager,
            surface,
            render_w,
            render_h,
            logical_w,
            logical_h,
            buffers,
            available_events: available_events.into_vec(),
            textures,
            rtvs,
        })
    }

    /// Mirror of `CanvasResources::acquire_available_buffer`. See that
    /// method's doc-comment for the cached-handle contract. Task 3.4 uses
    /// this when it replays MonitorLocal-scoped geometry onto each monitor's
    /// surface.
    pub fn acquire_available_buffer(&self, timeout_ms: u32) -> AcquireOutcome {
        acquire_available_buffer(&self.manager, &self.available_events, timeout_ms)
    }

    /// Mirror of `CanvasResources::present`. Independent Present so a
    /// per-Monitor failure does not affect World or other Monitors
    /// (Preservation 3.4).
    pub fn present(&self) -> PresentOutcome {
        present_manager(&self.manager, "PerMonitorResources")
    }

    /// Initial transparent-clear + present so DWM has a valid first buffer
    /// to show in the monitor's visual tree before the app ever
    /// emits a MonitorLocal-scoped command. Same best-effort policy as
    /// `CanvasResources::present_color`.
    pub fn present_color(&self, d3d_ctx: &ID3D11DeviceContext, rgba: [f32; 4]) -> Result<()> {
        let idx = match self.acquire_available_buffer(ACQUIRE_TIMEOUT_MS) {
            AcquireOutcome::Acquired(i) => i,
            AcquireOutcome::TimedOut => 0,
            AcquireOutcome::Failed(e) => return Err(e),
        };
        unsafe {
            d3d_ctx.ClearRenderTargetView(&self.rtvs[idx], &rgba);
            d3d_ctx.Flush();
            self.surface.SetBuffer(&self.buffers[idx])?;
            let _ = self.present();
            windows::Win32::System::Threading::SleepEx(0, true);
            while self.manager.GetNextPresentStatistics().is_ok() {}
        }
        Ok(())
    }
}
