//! Producer 进程：CompositionSwapchain 路径
//! 1. CreatePresentationFactory + Manager + DCompositionCreateSurfaceHandle + Surface
//! 2. 创建 D3D11 BGRA8 rendertarget texture，AddBufferFromResource → IPresentationBuffer
//! 3. ClearRenderTargetView 红色 → SetBuffer → Present
//! 4. 在 \.\pipe\overlay-spike-dcomp 监听
//! 5. 每个 consumer 连上来：读它的 PID → DuplicateHandle → 写回 dup HANDLE 值

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, NAMED_PIPE_MODE, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE};

use spike_dcomp::{dcomp, handle_to_u64, PIPE_PATH};

const SURFACE_W: u32 = 256;
const SURFACE_H: u32 = 256;

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn run() -> windows::core::Result<()> {
    eprintln!("[producer] step 1: create devices");
    let devices = dcomp::create_devices()?;
    eprintln!("[producer] step 2: producer_create (factory/manager/surface/buffer)");
    let producer = dcomp::producer_create(&devices.d3d, SURFACE_W, SURFACE_H)?;
    eprintln!("[producer] step 3: present red");
    dcomp::producer_present_color(&devices.d3d_ctx, &producer, [1.0, 0.0, 0.0, 1.0])?;
    eprintln!(
        "[producer] surface ready: handle={:#x} pid={} size={}x{}",
        handle_to_u64(producer.handle), std::process::id(), SURFACE_W, SURFACE_H
    );
    eprintln!("[producer] listening on {PIPE_PATH}");

    let pipe_name = to_wide(PIPE_PATH);
    let open_mode = PIPE_ACCESS_DUPLEX;
    let pipe_mode = NAMED_PIPE_MODE(PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0);
    let cur_proc = unsafe { GetCurrentProcess() };

    loop {
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(pipe_name.as_ptr()),
                open_mode, pipe_mode,
                1, 256, 256, 0, None,
            )
        };
        if pipe.is_invalid() { return Err(windows::core::Error::from_win32()); }
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
        let dup_bytes = handle_to_u64(dup).to_le_bytes();
        let mut written_n = 0u32;
        unsafe { WriteFile(pipe, Some(&dup_bytes), Some(&mut written_n), None)?; }
        eprintln!("[producer] dup handle {:#x} → consumer pid {}", handle_to_u64(dup), consumer_pid);

        unsafe {
            let _ = DisconnectNamedPipe(pipe);
            let _ = CloseHandle(pipe);
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
