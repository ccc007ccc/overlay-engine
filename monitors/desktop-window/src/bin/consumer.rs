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
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, LoadCursorW,
    PostQuitMessage, RegisterClassExW, SetTimer, ShowWindow, TranslateMessage,
    IDC_ARROW, MSG, SW_SHOW, WINDOW_EX_STYLE, WM_DESTROY, WM_DPICHANGED, WM_MOVE, WM_MOVING,
    WM_SIZE, WM_SIZING, WM_TIMER, WM_WINDOWPOSCHANGED, WNDCLASSEXW, WNDCLASS_STYLES,
    WS_OVERLAPPEDWINDOW,
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

thread_local! {
    static VIEWPORT_STATE: RefCell<Option<ViewportState>> = const { RefCell::new(None) };
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        if matches!(
            msg,
            WM_TIMER | WM_MOVE | WM_SIZE | WM_WINDOWPOSCHANGED | WM_MOVING | WM_SIZING | WM_DPICHANGED
        ) {
            VIEWPORT_STATE.with(|state| {
                if let Some(s) = state.borrow().as_ref() {
                    let _ = update_viewport(hwnd, s);
                }
            });
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

fn update_viewport(hwnd: HWND, state: &ViewportState) -> windows::core::Result<()> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect)? };
    let mut pt = POINT { x: 0, y: 0 };
    unsafe { ClientToScreen(hwnd, &mut pt) };
    let (cx, cy, cw, ch) = (pt.x, pt.y, rect.right - rect.left, rect.bottom - rect.top);

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
        state.visual.SetTransform2(&matrix)?;
        // 每次 timer tick 都 commit，让 DComp 重新拉取 surface 内容
        state.dcomp_dev.Commit()?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    // 创建窗口
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

    // 连接 core-server
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

    // RegisterConsumer
    let msg = ControlMessage::RegisterConsumer { pid: std::process::id() };
    let mut buf = BytesMut::new();
    msg.encode(&mut buf);
    pipe.write_all(&buf).await?;
    println!("[desktop-monitor] sent RegisterConsumer");

    // 等待 CanvasAttached
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

    // 用 handle 创建 DComp visual
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

    VIEWPORT_STATE.with(|state| {
        *state.borrow_mut() = Some(ViewportState {
            visual: visual.clone(),
            dcomp_dev: dcomp_dev.clone(),
            logical_w, logical_h, render_w, render_h,
        });
    });

    VIEWPORT_STATE.with(|state| {
        if let Some(s) = state.borrow().as_ref() {
            let _ = update_viewport(hwnd, s);
        }
    });

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetTimer(Some(hwnd), 1, 16, None);
    }

    println!("[desktop-monitor] visual tree attached, running message loop");

    // 消息循环在当前线程跑（Win32 要求）
    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !r.as_bool() { break; }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    VIEWPORT_STATE.with(|state| { *state.borrow_mut() = None; });
    drop(target);
    drop(visual);
    drop(surface_wrapper);
    Ok(())
}
