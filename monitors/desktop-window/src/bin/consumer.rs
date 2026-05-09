use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;
use windows::core::{w, IUnknown, Interface, PCWSTR};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR,
    IDCompositionDesktopDevice, IDCompositionVisual2,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, LoadCursorW,
    PeekMessageW, PostQuitMessage, RegisterClassExW, ShowWindow, TranslateMessage,
    IDC_ARROW, MSG, PM_REMOVE, SW_SHOW, WINDOW_EX_STYLE, WM_DESTROY, WM_QUIT,
    WNDCLASSEXW, WNDCLASS_STYLES, WS_OVERLAPPEDWINDOW,
};

use core_server::ipc::protocol::{ControlMessage, MessageHeader, HEADER_SIZE};

const PIPE_NAME: &str = r"\\.\pipe\overlay-core";

struct ViewportState {
    visual: IDCompositionVisual2,
    dcomp_dev: IDCompositionDesktopDevice,
    logical_w: u32,
    logical_h: u32,
    render_w: u32,
    render_h: u32,
}

unsafe impl Send for ViewportState {}
unsafe impl Sync for ViewportState {}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

fn update_viewport(hwnd: HWND, state: &ViewportState) {
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() { return; }
    let mut pt = POINT { x: 0, y: 0 };
    unsafe { ClientToScreen(hwnd, &mut pt) };
    let (cx, cy) = (pt.x, pt.y);

    let sx = state.logical_w as f32 / state.render_w as f32;
    let sy = state.logical_h as f32 / state.render_h as f32;
    let matrix = Matrix3x2 {
        M11: sx, M12: 0.0,
        M21: 0.0, M22: sy,
        M31: -(cx as f32), M32: -(cy as f32),
    };
    unsafe {
        let _ = state.visual.SetTransform2(&matrix);
        let _ = state.dcomp_dev.Commit();
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

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
    let hwnd: HWND = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("Desktop Monitor - connecting..."),
            WS_OVERLAPPEDWINDOW,
            100, 100, 720, 420,
            None, None,
            Some(HINSTANCE(hinst.0)),
            None,
        )?
    };

    println!("[desktop-monitor] connecting to {}", PIPE_NAME);
    let mut pipe = loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(windows::Win32::Foundation::ERROR_PIPE_BUSY.0 as i32) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e.into()),
        }
    };

    let msg = ControlMessage::RegisterConsumer { pid: std::process::id() };
    let mut buf = BytesMut::new();
    msg.encode(&mut buf);
    pipe.write_all(&buf).await?;
    println!("[desktop-monitor] sent RegisterConsumer");

    println!("[desktop-monitor] waiting for CanvasAttached...");
    let mut header_buf = [0u8; HEADER_SIZE];
    pipe.read_exact(&mut header_buf).await?;
    buf.clear();
    buf.extend_from_slice(&header_buf);
    let header = MessageHeader::decode(&mut buf)?;

    let mut payload_buf = vec![0u8; header.payload_len as usize];
    if header.payload_len > 0 {
        pipe.read_exact(&mut payload_buf).await?;
        buf.extend_from_slice(&payload_buf);
    }
    let msg = ControlMessage::decode(header.opcode, &mut buf)?;

    let (canvas_id, surface_handle_val, logical_w, logical_h, render_w, render_h) =
        if let ControlMessage::CanvasAttached {
            canvas_id, surface_handle, logical_w, logical_h, render_w, render_h,
        } = msg
        {
            (canvas_id, surface_handle, logical_w, logical_h, render_w, render_h)
        } else {
            return Err(anyhow::anyhow!("expected CanvasAttached, got {:?}", header.opcode));
        };

    println!(
        "[desktop-monitor] CanvasAttached: id={} handle={:#x} log={}x{} ren={}x{}",
        canvas_id, surface_handle_val, logical_w, logical_h, render_w, render_h
    );

    let dup_handle = windows::Win32::Foundation::HANDLE(surface_handle_val as *mut _);
    let mut d3d_opt = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>, D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None, D3D11_SDK_VERSION,
            Some(&mut d3d_opt), None, None,
        )?;
    }
    let d3d = d3d_opt.unwrap();
    let dxgi: IDXGIDevice = d3d.cast()?;
    let dcomp_dev: IDCompositionDesktopDevice = unsafe { DCompositionCreateDevice2(&dxgi)? };
    let surface_wrapper: IUnknown = unsafe { dcomp_dev.CreateSurfaceFromHandle(dup_handle)? };

    let visual = unsafe { dcomp_dev.CreateVisual()? };
    unsafe {
        visual.SetContent(&surface_wrapper)?;
        visual.SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)?;
    }
    let target = unsafe { dcomp_dev.CreateTargetForHwnd(hwnd, true)? };
    unsafe { target.SetRoot(&visual)? };

    let state = ViewportState {
        visual: visual.clone(),
        dcomp_dev: dcomp_dev.clone(),
        logical_w, logical_h, render_w, render_h,
    };

    update_viewport(hwnd, &state);
    unsafe { let _ = ShowWindow(hwnd, SW_SHOW); }
    println!("[desktop-monitor] visual tree attached, running render loop");

    // 非阻塞消息循环 + 自控帧率
    let frame_interval = std::time::Duration::from_micros(8000); // ~120hz commit
    let mut msg = MSG::default();
    loop {
        // 处理所有待处理的窗口消息
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                // 清理退出
                drop(target);
                drop(visual);
                drop(surface_wrapper);
                return Ok(());
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // 每帧更新 viewport + commit
        update_viewport(hwnd, &state);

        std::thread::sleep(frame_interval);
    }
}
