//! Producer 进程：生成逻辑坐标网格。
//!
//! 参数：
//!   desktop-demo-producer.exe [logical_w logical_h render_w render_h]
//! 默认：logical=当前主屏物理分辨率，render=logical（点对点）。
//!
//! logical canvas 与 render resolution 解耦；render 越低越模糊，但坐标/比例不变。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::time::Duration;

use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, DuplicateHandle, LocalFree, DUPLICATE_SAME_ACCESS, HANDLE, HLOCAL};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, NAMED_PIPE_MODE, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE};

use desktop_window::{dcomp, handle_to_u64, PIPE_PATH};
use desktop_window::dcomp::CanvasMeta;

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

struct PipeSecurity {
    sd: PSECURITY_DESCRIPTOR,
    attrs: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    fn new() -> windows::core::Result<Self> {
        let sddl = to_wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)(A;;GA;;;AC)");
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?;
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        Ok(Self { sd, attrs })
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.sd.0.is_null() {
            unsafe { let _ = LocalFree(Some(HLOCAL(self.sd.0))); }
        }
    }
}

fn parse_meta() -> CanvasMeta {
    let args: Vec<String> = std::env::args().collect();
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1) as u32;
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1) as u32;

    if args.len() == 5 {
        let logical_w = args[1].parse().unwrap_or(screen_w);
        let logical_h = args[2].parse().unwrap_or(screen_h);
        let render_w = args[3].parse().unwrap_or(logical_w);
        let render_h = args[4].parse().unwrap_or(logical_h);
        return CanvasMeta { logical_w, logical_h, render_w, render_h };
    }
    if args.len() == 3 {
        let logical_w = args[1].parse().unwrap_or(screen_w);
        let logical_h = args[2].parse().unwrap_or(screen_h);
        return CanvasMeta { logical_w, logical_h, render_w: logical_w, render_h: logical_h };
    }
    CanvasMeta { logical_w: screen_w, logical_h: screen_h, render_w: screen_w, render_h: screen_h }
}

fn run() -> windows::core::Result<()> {
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let meta = parse_meta();
    eprintln!("[producer] logical={}x{} render={}x{}", meta.logical_w, meta.logical_h, meta.render_w, meta.render_h);

    let devices = dcomp::create_devices()?;
    let producer = dcomp::producer_create(&devices.d3d, meta)?;
    dcomp::producer_present_grid(&devices.d3d_ctx, &producer, 0)?;

    eprintln!(
        "[producer] surface ready: handle={:#x} pid={}",
        handle_to_u64(producer.handle), std::process::id()
    );
    eprintln!("[producer] listening on {PIPE_PATH}");

    let pipe_name = to_wide(PIPE_PATH);
    let pipe_security = PipeSecurity::new()?;
    let open_mode = PIPE_ACCESS_DUPLEX;
    let pipe_mode = NAMED_PIPE_MODE(PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0);
    let cur_proc = unsafe { GetCurrentProcess() };
    loop {
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(pipe_name.as_ptr()),
                open_mode,
                pipe_mode,
                1,
                256,
                256,
                0,
                Some(&pipe_security.attrs),
            )
        };
        if pipe.is_invalid() {
            return Err(windows::core::Error::from_win32());
        }
        unsafe { ConnectNamedPipe(pipe, None)? };

        let mut pid_buf = [0u8; 4];
        let mut read_n = 0u32;
        unsafe { ReadFile(pipe, Some(&mut pid_buf), Some(&mut read_n), None)?; }
        let consumer_pid = u32::from_le_bytes(pid_buf);

        let consumer_proc = unsafe { OpenProcess(PROCESS_DUP_HANDLE, false, consumer_pid)? };
        let mut dup: HANDLE = HANDLE::default();
        unsafe {
            DuplicateHandle(cur_proc, producer.handle, consumer_proc, &mut dup, 0, false, DUPLICATE_SAME_ACCESS)?;
            let _ = CloseHandle(consumer_proc);
        }
        let payload = meta.to_payload(dup);
        let mut written_n = 0u32;
        unsafe { WriteFile(pipe, Some(&payload), Some(&mut written_n), None)?; }
        eprintln!("[producer] sent handle {:#x} + meta to consumer pid {}", handle_to_u64(dup), consumer_pid);

        unsafe {
            let _ = DisconnectNamedPipe(pipe);
            let _ = CloseHandle(pipe);
        }

        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

fn main() -> windows::core::Result<()> {
    match run() {
        Ok(_) => Ok(()),
        Err(e) => {
            eprintln!("[producer] FAIL: hr={:#x} msg={}", e.code().0, e.message());
            Err(e)
        }
    }
}
