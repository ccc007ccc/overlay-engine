//! Consumer 进程：观察窗语义验证。
//!
//! 窗口 client 在屏幕上的物理坐标 = viewport origin。
//! surface render pixels 通过 transform 映射回 logical canvas 坐标：
//!   parent = render_pixel * (logical/render) - viewport_origin
//! 所以移动窗口时，看到的是同一张逻辑画布的不同区域；render resolution 只影响清晰度。

use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::core::{w, PCWSTR};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::DirectComposition::{
    DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR, IDCompositionDesktopDevice, IDCompositionVisual2,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, LoadCursorW,
    PostQuitMessage, RegisterClassExW, SetTimer, SetWindowTextW, ShowWindow, TranslateMessage,
    IDC_ARROW, MSG, SW_SHOW, WINDOW_EX_STYLE, WM_DESTROY, WM_DPICHANGED, WM_MOVE, WM_MOVING,
    WM_SIZE, WM_SIZING, WM_TIMER, WM_WINDOWPOSCHANGED, WNDCLASSEXW, WNDCLASS_STYLES,
    WS_OVERLAPPEDWINDOW,
};

use desktop_window::dcomp::{self, CanvasMeta};
use desktop_window::PIPE_PATH;

struct ViewportState {
    visual: IDCompositionVisual2,
    dcomp_dev: IDCompositionDesktopDevice,
    meta: CanvasMeta,
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

        if matches!(msg, WM_TIMER | WM_MOVE | WM_SIZE | WM_WINDOWPOSCHANGED | WM_MOVING | WM_SIZING | WM_DPICHANGED) {
            VIEWPORT_STATE.with(|state| {
                if let Some(state) = state.borrow().as_ref() {
                    let _ = update_viewport(hwnd, &state.visual, &state.dcomp_dev, state.meta);
                }
            });
        }

        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn read_exact_pipe(pipe: HANDLE, buf: &mut [u8]) -> windows::core::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let mut read_n = 0u32;
        unsafe { ReadFile(pipe, Some(&mut buf[done..]), Some(&mut read_n), None)?; }
        if read_n == 0 {
            return Err(windows::core::Error::from_win32());
        }
        done += read_n as usize;
    }
    Ok(())
}

fn client_origin_and_size(hwnd: HWND) -> windows::core::Result<(i32, i32, i32, i32)> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect)?; }
    let mut pt = POINT { x: 0, y: 0 };
    let ok = unsafe { ClientToScreen(hwnd, &mut pt) };
    if !ok.as_bool() {
        return Err(windows::core::Error::from_win32());
    }
    Ok((pt.x, pt.y, rect.right - rect.left, rect.bottom - rect.top))
}

fn update_viewport(
    hwnd: HWND,
    visual: &IDCompositionVisual2,
    dcomp_dev: &IDCompositionDesktopDevice,
    meta: CanvasMeta,
) -> windows::core::Result<()> {
    let (client_x, client_y, client_w, client_h) = client_origin_and_size(hwnd)?;
    let matrix = Matrix3x2 {
        M11: meta.render_to_logical_scale_x(),
        M12: 0.0,
        M21: 0.0,
        M22: meta.render_to_logical_scale_y(),
        M31: -(client_x as f32),
        M32: -(client_y as f32),
    };
    unsafe {
        visual.SetTransform2(&matrix)?;
        dcomp_dev.Commit()?;
    }

    let title = format!(
        "Desktop Consumer | vp=({}, {}) {}x{} | logical={}x{} render={}x{} scale={:.2}x{:.2}",
        client_x,
        client_y,
        client_w,
        client_h,
        meta.logical_w,
        meta.logical_h,
        meta.render_w,
        meta.render_h,
        meta.render_to_logical_scale_x(),
        meta.render_to_logical_scale_y(),
    );
    let title_w = to_wide(&title);
    unsafe { SetWindowTextW(hwnd, PCWSTR(title_w.as_ptr()))?; }
    Ok(())
}

fn main() -> windows::core::Result<()> {
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let hinst = unsafe { GetModuleHandleW(None)? };
    let class_name = w!("OverlayDesktopConsumer");

    let wcex = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES::default(),
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst.into(),
        lpszClassName: class_name,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        ..Default::default()
    };
    let atom = unsafe { RegisterClassExW(&wcex) };
    if atom == 0 {
        return Err(windows::core::Error::from_win32());
    }

    let hwnd: HWND = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("Desktop Consumer"),
            WS_OVERLAPPEDWINDOW,
            100,
            100,
            720,
            420,
            None,
            None,
            Some(HINSTANCE(hinst.0)),
            None,
        )?
    };

    let pipe_name = to_wide(PIPE_PATH);
    let pipe = unsafe {
        CreateFileW(
            PCWSTR(pipe_name.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )?
    };

    let our_pid_bytes = std::process::id().to_le_bytes();
    let mut written = 0u32;
    unsafe { WriteFile(pipe, Some(&our_pid_bytes), Some(&mut written), None)?; }

    let mut payload = [0u8; 24];
    read_exact_pipe(pipe, &mut payload)?;
    let (dup_handle, meta) = CanvasMeta::from_payload(payload);
    eprintln!(
        "[consumer] received handle={:#x} logical={}x{} render={}x{} pid={}",
        dup_handle.0 as u64,
        meta.logical_w,
        meta.logical_h,
        meta.render_w,
        meta.render_h,
        std::process::id()
    );

    let devices = dcomp::create_devices()?;
    let dcomp_dev = devices.dcomp.clone();
    let surface_wrapper = dcomp::consumer_open_surface(&dcomp_dev, dup_handle)?;

    let visual = unsafe { dcomp_dev.CreateVisual()? };
    unsafe {
        visual.SetContent(&surface_wrapper)?;
        visual.SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)?;
    }
    let target = unsafe { dcomp_dev.CreateTargetForHwnd(hwnd, true)? };
    unsafe { target.SetRoot(&visual)?; }

    VIEWPORT_STATE.with(|state| {
        *state.borrow_mut() = Some(ViewportState {
            visual: visual.clone(),
            dcomp_dev: dcomp_dev.clone(),
            meta,
        });
    });

    update_viewport(hwnd, &visual, &dcomp_dev, meta)?;
    eprintln!("[consumer] visual tree attached; drag/resize window to test viewport mapping");

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetTimer(Some(hwnd), 1, 33, None);
    }

    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !r.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = update_viewport(hwnd, &visual, &dcomp_dev, meta);
    }

    VIEWPORT_STATE.with(|state| {
        *state.borrow_mut() = None;
    });

    drop(target);
    drop(visual);
    drop(surface_wrapper);
    Ok(())
}
