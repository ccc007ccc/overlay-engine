use bytes::BytesMut;
use core_server::ipc::protocol::{
    ControlMessage, DesktopWindowMode, MessageHeader, MonitorKind,
    DESKTOP_WINDOW_FLAG_CLICK_THROUGH, HEADER_SIZE,
};
use desktop_window::singleton::{
    decode_request, decode_response, encode_request, encode_response, launcher_log_line,
    SingletonRequest, SingletonResponse, SINGLETON_PIPE_NAME,
};
use desktop_window::title::{format_window_title, AttachState};
use std::io::{Error as IoError, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::{mpsc, oneshot};
use windows::core::{w, IUnknown, Interface, PCWSTR};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, IDCompositionDesktopDevice, IDCompositionTarget,
    IDCompositionVisual, IDCompositionVisual2, DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetWindowLongPtrW, LoadCursorW, PeekMessageW, RegisterClassExW, SetWindowLongPtrW,
    SetWindowTextW, ShowWindow, TranslateMessage, GWLP_USERDATA, HTTRANSPARENT, IDC_ARROW, MSG,
    PM_REMOVE, SW_SHOW, WM_CLOSE, WM_DESTROY, WM_NCHITTEST, WM_QUIT, WM_WINDOWPOSCHANGED,
    WNDCLASSEXW, WNDCLASS_STYLES, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TRANSPARENT,
    WS_OVERLAPPEDWINDOW, WS_POPUP,
};

const PIPE_NAME: &str = r"\\.\pipe\overlay-core";
const RECONNECT_BACKOFF_MS: &[u64] = &[500, 1000, 2000];
const RECONNECT_MAX_ATTEMPTS: u32 = 10;
const LAUNCHER_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);
const LAUNCHER_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct ViewportState {
    visual: IDCompositionVisual2,
    surface: IUnknown,
    dcomp_dev: IDCompositionDesktopDevice,
    logical_w: u32,
    logical_h: u32,
    render_w: u32,
    render_h: u32,
    pending_close: Arc<AtomicBool>,
    click_through: bool,
}

unsafe impl Send for ViewportState {}
unsafe impl Sync for ViewportState {}

struct CanvasAttach {
    canvas_id: u32,
    surface_handle: u64,
    logical_w: u32,
    logical_h: u32,
    render_w: u32,
    render_h: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DesktopWindowOptions {
    request_id: u32,
    owner_app_id: Option<u32>,
    target_canvas_id: u32,
    mode: DesktopWindowMode,
    flags: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Default for DesktopWindowOptions {
    fn default() -> Self {
        Self {
            request_id: 0,
            owner_app_id: None,
            target_canvas_id: 0,
            mode: DesktopWindowMode::Bordered,
            flags: 0,
            x: 100,
            y: 100,
            w: 720,
            h: 420,
        }
    }
}

impl DesktopWindowOptions {
    fn core_requested(self) -> bool {
        self.request_id != 0 || self.owner_app_id.is_some() || self.target_canvas_id != 0
    }

    fn owner_app_id_wire(self) -> u32 {
        self.owner_app_id.unwrap_or(0)
    }

    fn click_through(self) -> bool {
        self.flags & DESKTOP_WINDOW_FLAG_CLICK_THROUGH != 0
    }
}

struct WindowAttachment {
    _canvas_id: u32,
    _target: IDCompositionTarget,
    _root_visual: IDCompositionVisual2,
    _world_visual: IDCompositionVisual2,
    _surface_wrapper: IUnknown,
    _ml_visual_state: Option<(IDCompositionVisual2, IUnknown)>,
    _dcomp_dev: IDCompositionDesktopDevice,
}

struct MonitorWindow {
    id: u32,
    hwnd: HWND,
    options: DesktopWindowOptions,
    owner_app_id: Option<u32>,
    core_monitor_id: Option<u32>,
    pending_close: Arc<AtomicBool>,
    in_frame: Arc<AtomicBool>,
    attachment: Option<WindowAttachment>,
}

impl Drop for MonitorWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

enum PipeEvent {
    Message { window_id: u32, msg: ControlMessage },
    Disconnected { window_id: u32, error: String },
}

enum SingletonEvent {
    OpenWindow {
        options: DesktopWindowOptions,
        reply: oneshot::Sender<SingletonResponse>,
    },
}

fn set_window_title(hwnd: HWND, state: AttachState) {
    let s = format_window_title(state);
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let pcwstr = PCWSTR::from_raw(wide.as_ptr());
    unsafe {
        if let Err(e) = SetWindowTextW(hwnd, pcwstr) {
            eprintln!(
                "[desktop-monitor] SetWindowTextW({s:?}) failed: {e} \
                 (continuing; window title update is cosmetic)"
            );
        }
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CLOSE => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if ptr != 0 {
                    let state = &*(ptr as *const ViewportState);
                    state.pending_close.store(true, Ordering::SeqCst);
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if ptr != 0 {
                    let state = &*(ptr as *const ViewportState);
                    state.pending_close.store(true, Ordering::SeqCst);
                    let _ = Box::from_raw(ptr as *mut ViewportState);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                return LRESULT(0);
            }
            WM_WINDOWPOSCHANGED => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if ptr != 0 {
                    let state = &*(ptr as *const ViewportState);
                    update_viewport(hwnd, state);
                }
            }
            WM_NCHITTEST => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if ptr != 0 {
                    let state = &*(ptr as *const ViewportState);
                    if state.click_through {
                        return LRESULT(HTTRANSPARENT as isize);
                    }
                }
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

fn update_viewport(hwnd: HWND, state: &ViewportState) {
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    let mut pt = POINT { x: 0, y: 0 };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut pt);
    }
    let (cx, cy) = (pt.x, pt.y);

    let sx = state.logical_w as f32 / state.render_w as f32;
    let sy = state.logical_h as f32 / state.render_h as f32;
    let matrix = Matrix3x2 {
        M11: sx,
        M12: 0.0,
        M21: 0.0,
        M22: sy,
        M31: -(cx as f32),
        M32: -(cy as f32),
    };
    unsafe {
        let _ = state.visual.SetContent(&state.surface);
        let _ = state.visual.SetTransform2(&matrix);
        let _ = state.dcomp_dev.Commit();
    }
}

fn install_viewport_state(hwnd: HWND, state: ViewportState) {
    unsafe {
        let old = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if old != 0 {
            let _ = Box::from_raw(old as *mut ViewportState);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(Box::new(state)) as isize);
    }
}

async fn read_next_control_message(
    pipe: &mut NamedPipeClient,
) -> anyhow::Result<Option<ControlMessage>> {
    let mut header_buf = [0u8; HEADER_SIZE];
    pipe.read_exact(&mut header_buf).await?;

    let mut buf = BytesMut::new();
    buf.extend_from_slice(&header_buf);
    let header = MessageHeader::decode(&mut buf)?;

    let mut payload_buf = vec![0u8; header.payload_len as usize];
    if header.payload_len > 0 {
        pipe.read_exact(&mut payload_buf).await?;
        buf.extend_from_slice(&payload_buf);
    }

    Ok(ControlMessage::decode(
        header.opcode,
        header.payload_len,
        &mut buf,
    )?)
}

async fn connect_to_core() -> anyhow::Result<NamedPipeClient> {
    loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(c) => return Ok(c),
            Err(e)
                if e.raw_os_error()
                    == Some(windows::Win32::Foundation::ERROR_PIPE_BUSY.0 as i32) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

async fn register_and_wait_attach(
    hwnd: HWND,
    options: DesktopWindowOptions,
) -> anyhow::Result<(NamedPipeClient, CanvasAttach)> {
    println!("[desktop-monitor] connecting to {}", PIPE_NAME);
    let mut pipe = connect_to_core().await?;

    let msg = if options.core_requested() {
        ControlMessage::RegisterMonitorV2 {
            pid: std::process::id(),
            kind: MonitorKind::DesktopWindow,
            owner_app_id: options.owner_app_id_wire(),
            request_id: options.request_id,
            target_canvas_id: options.target_canvas_id,
            mode: options.mode,
            flags: options.flags,
            manual_lifecycle: false,
        }
    } else {
        ControlMessage::RegisterMonitor {
            pid: std::process::id(),
        }
    };
    let mut buf = BytesMut::new();
    msg.encode(&mut buf);
    pipe.write_all(&buf).await?;
    println!("[desktop-monitor] sent {:?}", msg);
    println!("[desktop-monitor] waiting for CanvasAttached...");

    let attach = loop {
        match read_next_control_message(&mut pipe).await? {
            Some(ControlMessage::CanvasAttached {
                canvas_id,
                surface_handle,
                logical_w,
                logical_h,
                render_w,
                render_h,
            }) if options.target_canvas_id == 0 || canvas_id == options.target_canvas_id => {
                break CanvasAttach {
                    canvas_id,
                    surface_handle,
                    logical_w,
                    logical_h,
                    render_w,
                    render_h,
                };
            }
            Some(ControlMessage::CanvasAttached { canvas_id, .. }) => {
                eprintln!(
                    "[desktop-monitor] skipping CanvasAttached canvas={} while waiting for target canvas={}",
                    canvas_id, options.target_canvas_id
                );
            }
            Some(ControlMessage::CloseMonitor { monitor_id }) => {
                anyhow::bail!("Core closed monitor {} before CanvasAttached", monitor_id);
            }
            Some(other) => {
                eprintln!(
                    "[desktop-monitor] unexpected message before CanvasAttached: {:?}",
                    other
                );
            }
            None => {}
        }
    };

    println!(
        "[desktop-monitor] CanvasAttached: id={} handle={:#x} log={}x{} ren={}x{}",
        attach.canvas_id,
        attach.surface_handle,
        attach.logical_w,
        attach.logical_h,
        attach.render_w,
        attach.render_h
    );
    set_window_title(
        hwnd,
        AttachState::Attached {
            canvas_id: attach.canvas_id,
            ml: false,
        },
    );

    Ok((pipe, attach))
}

fn build_attachment(
    hwnd: HWND,
    attach: CanvasAttach,
    pending_close: Arc<AtomicBool>,
    click_through: bool,
) -> anyhow::Result<WindowAttachment> {
    let dup_handle = HANDLE(attach.surface_handle as *mut _);
    let mut d3d_opt = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut d3d_opt),
            None,
            None,
        )?;
    }
    let d3d = d3d_opt.unwrap();
    let dxgi: IDXGIDevice = d3d.cast()?;
    let dcomp_dev: IDCompositionDesktopDevice = unsafe { DCompositionCreateDevice2(&dxgi)? };
    let surface_wrapper_result = unsafe { dcomp_dev.CreateSurfaceFromHandle(dup_handle) };
    unsafe {
        let _ = CloseHandle(dup_handle);
    }
    let surface_wrapper: IUnknown = surface_wrapper_result?;

    let visual = unsafe { dcomp_dev.CreateVisual()? };
    unsafe {
        visual.SetContent(&surface_wrapper)?;
        visual.SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)?;
    }
    let target = unsafe { dcomp_dev.CreateTargetForHwnd(hwnd, true)? };

    let root = unsafe { dcomp_dev.CreateVisual()? };
    unsafe {
        root.AddVisual(&visual, false, None::<&IDCompositionVisual>)?;
        target.SetRoot(&root)?;
    }

    unsafe {
        dcomp_dev.Commit()?;
    }
    println!("[desktop-monitor] mounted single visual tree (World only)");
    let ml_visual_state = None;

    let state = ViewportState {
        visual: visual.clone(),
        surface: surface_wrapper.clone(),
        dcomp_dev: dcomp_dev.clone(),
        logical_w: attach.logical_w,
        logical_h: attach.logical_h,
        render_w: attach.render_w,
        render_h: attach.render_h,
        pending_close,
        click_through,
    };
    install_viewport_state(hwnd, state);

    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if ptr != 0 {
        let state = unsafe { &*(ptr as *const ViewportState) };
        update_viewport(hwnd, state);
    }

    Ok(WindowAttachment {
        _canvas_id: attach.canvas_id,
        _target: target,
        _root_visual: root,
        _world_visual: visual,
        _surface_wrapper: surface_wrapper,
        _ml_visual_state: ml_visual_state,
        _dcomp_dev: dcomp_dev,
    })
}

fn hot_attach_ml(att: &mut WindowAttachment, surface_handle: u64) -> anyhow::Result<()> {
    let ml_handle = HANDLE(surface_handle as *mut _);
    let ml_surface_result = unsafe { att._dcomp_dev.CreateSurfaceFromHandle(ml_handle) };
    unsafe {
        let _ = CloseHandle(ml_handle);
    }
    let ml_surface: IUnknown = ml_surface_result?;
    let ml_visual = unsafe { att._dcomp_dev.CreateVisual()? };
    unsafe {
        ml_visual.SetContent(&ml_surface)?;
        ml_visual.SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)?;
        att._root_visual
            .AddVisual(&ml_visual, true, &att._world_visual)?;
        att._dcomp_dev.Commit()?;
    }
    att._ml_visual_state = Some((ml_visual, ml_surface));
    Ok(())
}

fn reconnect_delay(attempt: u32) -> std::time::Duration {
    let idx = std::cmp::min(attempt as usize, RECONNECT_BACKOFF_MS.len() - 1);
    let jitter = (std::process::id() as u64 + attempt as u64 * 73) % 201;
    std::time::Duration::from_millis(RECONNECT_BACKOFF_MS[idx] + jitter)
}

async fn reconnect_with_backoff(
    window: &mut MonitorWindow,
    pipe_events: mpsc::Sender<PipeEvent>,
) -> anyhow::Result<()> {
    for attempt in 0..RECONNECT_MAX_ATTEMPTS {
        set_window_title(window.hwnd, AttachState::Reconnecting);
        let delay = reconnect_delay(attempt);
        eprintln!(
            "[desktop-monitor] reconnect attempt {}/{} after {:?}",
            attempt + 1,
            RECONNECT_MAX_ATTEMPTS,
            delay
        );
        tokio::time::sleep(delay).await;

        match create_monitor_hwnd(window.options) {
            Ok(new_hwnd) => match register_and_wait_attach(new_hwnd, window.options).await {
                Ok((new_pipe, attach)) => {
                    let old_hwnd = window.hwnd;
                    window.hwnd = new_hwnd;

                    window.attachment = Some(build_attachment(
                        new_hwnd,
                        attach,
                        window.pending_close.clone(),
                        window.options.click_through(),
                    )?);
                    spawn_pipe_reader(window.id, new_pipe, pipe_events);

                    unsafe {
                        let _ = ShowWindow(new_hwnd, SW_SHOW);
                        let _ = DestroyWindow(old_hwnd);
                    }
                    return Ok(());
                }
                Err(e) => {
                    unsafe {
                        let _ = DestroyWindow(new_hwnd);
                    }
                    eprintln!(
                        "[desktop-monitor] reconnect attempt {} failed: {e}",
                        attempt + 1
                    );
                }
            },
            Err(e) => {
                eprintln!(
                    "[desktop-monitor] reconnect attempt {} failed to create window: {e}",
                    attempt + 1
                );
            }
        }
    }

    anyhow::bail!("reconnect failed after {RECONNECT_MAX_ATTEMPTS} attempts")
}

fn window_dimension(value: u32, fallback: i32) -> i32 {
    if value == 0 {
        fallback
    } else {
        i32::try_from(value).unwrap_or(i32::MAX).max(1)
    }
}

fn fullscreen_rect_for_options(options: DesktopWindowOptions) -> (i32, i32, i32, i32) {
    let w = window_dimension(options.w, 720);
    let h = window_dimension(options.h, 420);
    let rect = RECT {
        left: options.x,
        top: options.y,
        right: options.x.saturating_add(w),
        bottom: options.y.saturating_add(h),
    };
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info).as_bool() } {
        let width = info
            .rcMonitor
            .right
            .saturating_sub(info.rcMonitor.left)
            .max(1);
        let height = info
            .rcMonitor
            .bottom
            .saturating_sub(info.rcMonitor.top)
            .max(1);
        (info.rcMonitor.left, info.rcMonitor.top, width, height)
    } else {
        (options.x, options.y, w, h)
    }
}

fn create_monitor_hwnd(options: DesktopWindowOptions) -> anyhow::Result<HWND> {
    let hinst = unsafe { GetModuleHandleW(None)? };
    let class_name = w!("OverlayDesktopMonitor");
    let ex_style = if options.click_through() {
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_TRANSPARENT
    } else {
        WS_EX_NOREDIRECTIONBITMAP
    };
    let style = match options.mode {
        DesktopWindowMode::Bordered => WS_OVERLAPPEDWINDOW,
        DesktopWindowMode::Borderless | DesktopWindowMode::BorderlessFullscreen => WS_POPUP,
    };
    let (x, y, w, h) = if options.mode == DesktopWindowMode::BorderlessFullscreen {
        fullscreen_rect_for_options(options)
    } else {
        (
            options.x,
            options.y,
            window_dimension(options.w, 720),
            window_dimension(options.h, 420),
        )
    };
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            class_name,
            w!("Desktop Monitor - connecting..."),
            style,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(HINSTANCE(hinst.0)),
            None,
        )?
    };
    Ok(hwnd)
}

fn register_window_class() -> anyhow::Result<()> {
    let hinst = unsafe { GetModuleHandleW(None)? };
    let class_name = w!("OverlayDesktopMonitor");
    let wcex = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES::default(),
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst.into(),
        lpszClassName: class_name,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        ..Default::default()
    };
    unsafe { RegisterClassExW(&wcex) };
    Ok(())
}

async fn open_monitor_window(
    id: u32,
    options: DesktopWindowOptions,
    pipe_events: mpsc::Sender<PipeEvent>,
) -> anyhow::Result<MonitorWindow> {
    let hwnd = create_monitor_hwnd(options)?;
    let pending_close = Arc::new(AtomicBool::new(false));
    let in_frame = Arc::new(AtomicBool::new(false));
    let (pipe, attach) = register_and_wait_attach(hwnd, options).await?;
    let attachment =
        build_attachment(hwnd, attach, pending_close.clone(), options.click_through())?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    spawn_pipe_reader(id, pipe, pipe_events);
    Ok(MonitorWindow {
        id,
        hwnd,
        options,
        owner_app_id: options.owner_app_id,
        core_monitor_id: None,
        pending_close,
        in_frame,
        attachment: Some(attachment),
    })
}

fn spawn_pipe_reader(id: u32, mut pipe: NamedPipeClient, tx: mpsc::Sender<PipeEvent>) {
    tokio::spawn(async move {
        loop {
            match read_next_control_message(&mut pipe).await {
                Ok(Some(msg)) => {
                    if tx
                        .send(PipeEvent::Message { window_id: id, msg })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = tx
                        .send(PipeEvent::Disconnected {
                            window_id: id,
                            error: e.to_string(),
                        })
                        .await;
                    break;
                }
            }
        }
    });
}

fn create_singleton_server(first: bool) -> std::io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options.first_pipe_instance(first);
    options.create(SINGLETON_PIPE_NAME)
}

async fn read_singleton_request<R>(reader: &mut R) -> std::io::Result<SingletonRequest>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 6];
    reader.read_exact(&mut header).await?;
    let len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
    let mut buf = BytesMut::from(&header[..]);
    if len > 0 {
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).await?;
        buf.extend_from_slice(&payload);
    }
    decode_request(&mut buf).map_err(|e| IoError::new(ErrorKind::InvalidData, format!("{e:?}")))
}

async fn read_singleton_response<R>(reader: &mut R) -> std::io::Result<SingletonResponse>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 6];
    reader.read_exact(&mut header).await?;
    let len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
    let mut buf = BytesMut::from(&header[..]);
    if len > 0 {
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).await?;
        buf.extend_from_slice(&payload);
    }
    decode_response(&mut buf).map_err(|e| IoError::new(ErrorKind::InvalidData, format!("{e:?}")))
}

async fn write_singleton_response<W>(
    writer: &mut W,
    response: &SingletonResponse,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut buf = BytesMut::new();
    encode_response(response, &mut buf);
    writer.write_all(&buf).await
}

async fn write_singleton_request<W>(
    writer: &mut W,
    request: SingletonRequest,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut buf = BytesMut::new();
    encode_request(request, &mut buf);
    writer.write_all(&buf).await
}

fn options_from_singleton_request(request: SingletonRequest) -> DesktopWindowOptions {
    match request {
        SingletonRequest::OpenWindow { target_canvas_id } => DesktopWindowOptions {
            target_canvas_id,
            ..Default::default()
        },
        SingletonRequest::OpenWindowV2 {
            request_id,
            owner_app_id,
            target_canvas_id,
            mode,
            flags,
            x,
            y,
            w,
            h,
        } => DesktopWindowOptions {
            request_id,
            owner_app_id: (owner_app_id != 0).then_some(owner_app_id),
            target_canvas_id,
            mode,
            flags,
            x,
            y,
            w,
            h,
        },
    }
}

fn singleton_request_from_options(options: DesktopWindowOptions) -> SingletonRequest {
    if options.core_requested() {
        SingletonRequest::OpenWindowV2 {
            request_id: options.request_id,
            owner_app_id: options.owner_app_id_wire(),
            target_canvas_id: options.target_canvas_id,
            mode: options.mode,
            flags: options.flags,
            x: options.x,
            y: options.y,
            w: options.w,
            h: options.h,
        }
    } else {
        SingletonRequest::OpenWindow {
            target_canvas_id: options.target_canvas_id,
        }
    }
}

async fn handle_singleton_connection(
    mut server: NamedPipeServer,
    tx: mpsc::Sender<SingletonEvent>,
) {
    match read_singleton_request(&mut server).await {
        Ok(request) => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(SingletonEvent::OpenWindow {
                    options: options_from_singleton_request(request),
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                return;
            }
            match reply_rx.await {
                Ok(response) => {
                    if let Err(e) = write_singleton_response(&mut server, &response).await {
                        eprintln!("[desktop-monitor] singleton response write failed: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("[desktop-monitor] singleton response canceled: {e}");
                }
            }
        }
        Err(e) => {
            let response = SingletonResponse::Nack {
                reason: 1,
                message: format!("malformed singleton request: {e}"),
            };
            let _ = write_singleton_response(&mut server, &response).await;
        }
    }
}

fn spawn_singleton_accept_loop(mut server: NamedPipeServer, tx: mpsc::Sender<SingletonEvent>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = server.connect().await {
                eprintln!("[desktop-monitor] singleton connect failed: {e}");
                break;
            }

            let connected_server = server;
            server = match create_singleton_server(false) {
                Ok(next) => next,
                Err(e) => {
                    eprintln!("[desktop-monitor] singleton next instance create failed: {e}");
                    tokio::spawn(handle_singleton_connection(connected_server, tx.clone()));
                    break;
                }
            };

            tokio::spawn(handle_singleton_connection(connected_server, tx.clone()));
        }
    });
}

fn pump_win32_messages() -> bool {
    let mut quit = false;
    let mut msg = MSG::default();
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
        if msg.message == WM_QUIT {
            quit = true;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    quit
}

async fn connect_singleton_client() -> std::io::Result<NamedPipeClient> {
    loop {
        match ClientOptions::new().open(SINGLETON_PIPE_NAME) {
            Ok(c) => return Ok(c),
            Err(e)
                if e.raw_os_error()
                    == Some(windows::Win32::Foundation::ERROR_PIPE_BUSY.0 as i32) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn run_as_launcher(options: DesktopWindowOptions) -> anyhow::Result<()> {
    let mut client =
        tokio::time::timeout(LAUNCHER_CONNECT_TIMEOUT, connect_singleton_client()).await??;
    write_singleton_request(&mut client, singleton_request_from_options(options)).await?;
    match tokio::time::timeout(LAUNCHER_ACK_TIMEOUT, read_singleton_response(&mut client)).await {
        Ok(Ok(SingletonResponse::Ack { pid, .. })) => {
            println!("{}", launcher_log_line(pid));
            Ok(())
        }
        Ok(Ok(SingletonResponse::Nack { reason, message })) => {
            anyhow::bail!("singleton nack reason={reason}: {message}")
        }
        Ok(Err(e)) => Err(e.into()),
        Err(_) => {
            anyhow::bail!(
                "singleton ack timeout; assuming existing monitor-process is stuck, exiting"
            )
        }
    }
}

async fn run_as_monitor_process(
    singleton_server: NamedPipeServer,
    initial_options: DesktopWindowOptions,
) -> anyhow::Result<()> {
    let (pipe_tx, mut pipe_rx) = mpsc::channel::<PipeEvent>(64);
    let (singleton_tx, mut singleton_rx) = mpsc::channel::<SingletonEvent>(8);
    spawn_singleton_accept_loop(singleton_server, singleton_tx);

    let mut next_window_id = 1u32;
    let mut windows =
        vec![open_monitor_window(next_window_id, initial_options, pipe_tx.clone()).await?];
    next_window_id = next_window_id.saturating_add(1);
    println!(
        "[desktop-monitor] {} windows attached, running render loop",
        windows.len()
    );

    let frame_interval = std::time::Duration::from_micros(8000);
    let mut next_tick = tokio::time::Instant::now() + frame_interval;

    loop {
        if pump_win32_messages() {
            break;
        }

        tokio::select! {
            Some(event) = singleton_rx.recv() => {
                match event {
                    SingletonEvent::OpenWindow { options, reply } => {
                        let window_id = next_window_id;
                        next_window_id = next_window_id.saturating_add(1);
                        let response = match open_monitor_window(window_id, options, pipe_tx.clone()).await {
                            Ok(window) => {
                                windows.push(window);
                                SingletonResponse::Ack {
                                    pid: std::process::id(),
                                    new_monitor_id: window_id,
                                }
                            }
                            Err(e) => SingletonResponse::Nack {
                                reason: 2,
                                message: e.to_string(),
                            },
                        };
                        let _ = reply.send(response);
                    }
                }
            }

            Some(event) = pipe_rx.recv() => {
                match event {
                    PipeEvent::Message { window_id, msg: ControlMessage::AppDetached { app_id, reason } } => {
                        println!("[desktop-monitor] AppDetached app={} reason={} window={}", app_id, reason, window_id);
                        if let Some(w) = windows.iter().find(|w| w.id == window_id) {
                            if w.owner_app_id == Some(app_id) || w.owner_app_id.is_none() {
                                w.pending_close.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    PipeEvent::Message { window_id, msg: ControlMessage::CloseMonitor { monitor_id } } => {
                        println!("[desktop-monitor] CloseMonitor monitor={} window={}", monitor_id, window_id);
                        if let Some(w) = windows.iter().find(|w| w.id == window_id) {
                            if w.core_monitor_id.is_none() || w.core_monitor_id == Some(monitor_id) {
                                w.pending_close.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    PipeEvent::Message { window_id, msg: ControlMessage::CanvasAttached {
                        canvas_id, surface_handle, logical_w, logical_h, render_w, render_h,
                    } } => {
                        println!("[desktop-monitor] CanvasAttached on window {}: canvas={}", window_id, canvas_id);
                        let options = windows
                            .iter()
                            .find(|w| w.id == window_id)
                            .map(|w| w.options)
                            .unwrap_or_else(|| DesktopWindowOptions {
                                target_canvas_id: canvas_id,
                                ..Default::default()
                            });

                        if let Some(window) = windows.iter_mut().find(|w| w.id == window_id && !w.pending_close.load(Ordering::SeqCst)) {
                            window.pending_close.store(true, Ordering::SeqCst);
                        }

                        match create_monitor_hwnd(options) {
                            Ok(hwnd) => {
                                let pending_close = Arc::new(AtomicBool::new(false));
                                let in_frame = Arc::new(AtomicBool::new(false));
                                let attach = CanvasAttach {
                                    canvas_id,
                                    surface_handle,
                                    logical_w,
                                    logical_h,
                                    render_w,
                                    render_h,
                                };
                                match build_attachment(hwnd, attach, pending_close.clone(), options.click_through()) {
                                    Ok(a) => {
                                        unsafe { let _ = ShowWindow(hwnd, SW_SHOW); }
                                        windows.push(MonitorWindow {
                                            id: window_id,
                                            hwnd,
                                            options,
                                            owner_app_id: options.owner_app_id,
                                            core_monitor_id: None,
                                            pending_close,
                                            in_frame,
                                            attachment: Some(a),
                                        });
                                        set_window_title(hwnd, AttachState::Attached { canvas_id, ml: false });
                                        println!("[desktop-monitor] re-created window {} for canvas {}", window_id, canvas_id);
                                    }
                                    Err(e) => {
                                        unsafe { let _ = DestroyWindow(hwnd); }
                                        eprintln!("[desktop-monitor] re-create window attach failed: {e}");
                                    }
                                }
                            }
                            Err(e) => eprintln!("[desktop-monitor] re-create window failed: {e}"),
                        }
                    }
                    PipeEvent::Message { window_id, msg: ControlMessage::MonitorLocalSurfaceAttached {
                        canvas_id, monitor_id, surface_handle, ..
                    } } => {
                        println!(
                            "[desktop-monitor] MonitorLocalSurfaceAttached on window {}: canvas={} monitor={} handle={:#x}",
                            window_id, canvas_id, monitor_id, surface_handle
                        );
                        if let Some(window) = windows.iter_mut().find(|w| w.id == window_id && !w.pending_close.load(Ordering::SeqCst)) {
                            window.core_monitor_id = Some(monitor_id);
                            if let Some(ref mut att) = window.attachment {
                                if att._canvas_id == canvas_id {
                                    match hot_attach_ml(att, surface_handle) {
                                        Ok(()) => {
                                            set_window_title(window.hwnd, AttachState::Attached { canvas_id, ml: true });
                                            println!("[desktop-monitor] hot re-attached MonitorLocal visual on window {}", window_id);
                                        }
                                        Err(e) => eprintln!("[desktop-monitor] MonitorLocal hot re-attach failed on window {}: {e}", window_id),
                                    }
                                }
                            }
                        }
                    }
                    PipeEvent::Message { window_id, msg } => {
                        eprintln!("[desktop-monitor] unexpected control message for window {}: {:?}", window_id, msg);
                    }
                    PipeEvent::Disconnected { window_id, error } => {
                        eprintln!("[desktop-monitor] pipe read failed for window {}: {}", window_id, error);
                        // Only reconnect if this isn't a window we intentionally closed.
                        if let Some(window) = windows.iter_mut().find(|w| w.id == window_id && !w.pending_close.load(Ordering::SeqCst)) {
                            if let Err(e) = reconnect_with_backoff(window, pipe_tx.clone()).await {
                                eprintln!("[desktop-monitor] {e}");
                                window.pending_close.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                }
            }

            _ = tokio::time::sleep_until(next_tick) => {
                next_tick += frame_interval;

                for w in &windows {
                    if !w.pending_close.load(Ordering::SeqCst) {
                        w.in_frame.store(true, Ordering::SeqCst);
                        let ptr = unsafe { GetWindowLongPtrW(w.hwnd, GWLP_USERDATA) };
                        if ptr != 0 {
                            let state = unsafe { &*(ptr as *const ViewportState) };
                            update_viewport(w.hwnd, state);
                        }
                        w.in_frame.store(false, Ordering::SeqCst);
                    }
                }

                windows.retain(|w| {
                    !(w.pending_close.load(Ordering::SeqCst) && !w.in_frame.load(Ordering::SeqCst))
                });
                if windows.is_empty() {
                    break;
                }
            }
        }
    }

    Ok(())
}

fn parse_mode(value: &str) -> anyhow::Result<DesktopWindowMode> {
    match value {
        "bordered" => Ok(DesktopWindowMode::Bordered),
        "borderless" => Ok(DesktopWindowMode::Borderless),
        "borderless-fullscreen" | "fullscreen" => Ok(DesktopWindowMode::BorderlessFullscreen),
        _ => anyhow::bail!("unknown desktop window mode: {value}"),
    }
}

fn parse_bool_flag(value: &str) -> anyhow::Result<bool> {
    match value {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => anyhow::bail!("invalid boolean flag value: {value}"),
    }
}

fn parse_initial_options() -> anyhow::Result<DesktopWindowOptions> {
    let args: Vec<String> = std::env::args().collect();
    let mut options = DesktopWindowOptions::default();
    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "--request-id" => {
                i += 1;
                options.request_id = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--request-id needs a value"))?
                    .parse()?;
            }
            "--owner-app-id" => {
                i += 1;
                let owner_app_id: u32 = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--owner-app-id needs a value"))?
                    .parse()?;
                options.owner_app_id = (owner_app_id != 0).then_some(owner_app_id);
            }
            "--target-canvas-id" => {
                i += 1;
                options.target_canvas_id = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--target-canvas-id needs a value"))?
                    .parse()?;
            }
            "--mode" => {
                i += 1;
                options.mode = parse_mode(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--mode needs a value"))?,
                )?;
            }
            "--click-through" => {
                let enabled = if args
                    .get(i + 1)
                    .is_some_and(|value| !value.starts_with("--"))
                {
                    i += 1;
                    parse_bool_flag(&args[i])?
                } else {
                    true
                };
                if enabled {
                    options.flags |= DESKTOP_WINDOW_FLAG_CLICK_THROUGH;
                } else {
                    options.flags &= !DESKTOP_WINDOW_FLAG_CLICK_THROUGH;
                }
            }
            "--x" => {
                i += 1;
                options.x = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--x needs a value"))?
                    .parse()?;
            }
            "--y" => {
                i += 1;
                options.y = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--y needs a value"))?
                    .parse()?;
            }
            "--w" => {
                i += 1;
                options.w = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--w needs a value"))?
                    .parse()?;
            }
            "--h" => {
                i += 1;
                options.h = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--h needs a value"))?
                    .parse()?;
            }
            other => eprintln!("[desktop-monitor] ignoring unknown arg: {other}"),
        }
        i += 1;
    }
    Ok(options)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let options = parse_initial_options()?;
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    match create_singleton_server(true) {
        Ok(server) => {
            register_window_class()?;
            run_as_monitor_process(server, options).await
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied => run_as_launcher(options).await,
        Err(e) => Err(e.into()),
    }
}
