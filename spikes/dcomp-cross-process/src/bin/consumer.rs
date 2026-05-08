//! Consumer 进程：
//! 1. 创建 Win32 普通窗口（300x300）
//! 2. 连 \.\pipe\overlay-spike-dcomp，写自己 PID，读 dup HANDLE
//! 3. 用同一 HANDLE 调 dcomp.CreateSurfaceFromHandle 拿只读 surface
//! 4. 创建 DComp visual tree：visual.SetContent(surface) → target.SetRoot
//! 5. 消息循环。窗口里看到 producer 画的红色 = spike PASS。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{SetTimer, WM_TIMER,
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, LoadCursorW, PostQuitMessage,
    RegisterClassExW, ShowWindow, TranslateMessage, IDC_ARROW, MSG, SW_SHOW, WINDOW_EX_STYLE,
    WM_DESTROY, WNDCLASSEXW, WNDCLASS_STYLES, WS_OVERLAPPEDWINDOW,
};

use spike_dcomp::{dcomp, u64_to_handle, PIPE_PATH};

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_DESTROY || msg == WM_TIMER {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn main() -> windows::core::Result<()> {
    let hinst = unsafe { GetModuleHandleW(None)? };
    let class_name = w!("SpikeDCompConsumer");

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
            w!("Spike DComp Consumer"),
            WS_OVERLAPPEDWINDOW,
            100,
            100,
            300,
            300,
            None,
            None,
            Some(HINSTANCE(hinst.0)),
            None,
        )?
    };

    // 连 producer named pipe
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
    unsafe { WriteFile(pipe, Some(&our_pid_bytes), Some(&mut written), None)? };

    let mut handle_buf = [0u8; 8];
    let mut read_n = 0u32;
    unsafe { ReadFile(pipe, Some(&mut handle_buf), Some(&mut read_n), None)? };
    let dup_handle: HANDLE = u64_to_handle(u64::from_le_bytes(handle_buf));

    eprintln!(
        "[consumer] received dup handle {:#x} (pid {})",
        u64::from_le_bytes(handle_buf),
        std::process::id()
    );

    let devices = dcomp::create_devices()?;
    let dcomp_dev = devices.dcomp.clone();
    let surface_wrapper = dcomp::consumer_open_surface(&dcomp_dev, dup_handle)?;

    let visual = unsafe { dcomp_dev.CreateVisual()? };
    unsafe { visual.SetContent(&surface_wrapper)? };
    let target = unsafe { dcomp_dev.CreateTargetForHwnd(hwnd, true)? };
    unsafe { target.SetRoot(&visual)? };
    unsafe { dcomp_dev.Commit()? };

    eprintln!("[consumer] visual tree attached, showing window");

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW); let _ = SetTimer(Some(hwnd), 1, 5000, None);
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
    }

    drop(target);
    drop(visual);
    drop(surface_wrapper);
    Ok(())
}
