use std::ffi::c_void;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};

/// 共享内存 Ringbuffer 的元数据区域（通常放在 buffer 的最开头）。
/// 用于 Producer 和 Core 之间同步读写偏移量。
#[repr(C)]
pub struct RingbufferHeader {
    pub write_offset: u32,
    pub read_offset: u32,
    pub capacity: u32,
    pub frame_counter: u64,
}

pub struct SharedMemory {
    handle: HANDLE,
    ptr: *mut c_void,
    size: usize,
}

unsafe impl Send for SharedMemory {}
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
    pub fn create(name: &str, size: usize) -> anyhow::Result<Self> {
        let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

        let handle = unsafe {
            CreateFileMappingW(
                HANDLE::default(), // INVALID_HANDLE_VALUE backed by page file
                None,
                PAGE_READWRITE,
                (size >> 32) as u32,
                (size & 0xFFFFFFFF) as u32,
                windows::core::PCWSTR(name_w.as_ptr()),
            )?
        };

        if handle.is_invalid() {
            return Err(anyhow::anyhow!("Failed to create file mapping"));
        }

        let ptr = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size) };

        if ptr.Value.is_null() {
            unsafe { let _ = CloseHandle(handle); }
            return Err(anyhow::anyhow!("Failed to map view of file"));
        }

        // 初始化 header
        unsafe {
            let header = &mut *(ptr.Value as *mut RingbufferHeader);
            header.write_offset = std::mem::size_of::<RingbufferHeader>() as u32;
            header.read_offset = std::mem::size_of::<RingbufferHeader>() as u32;
            header.capacity = size as u32;
            header.frame_counter = 0;
        }

        Ok(Self {
            handle,
            ptr: ptr.Value,
            size,
        })
    }

    pub fn header(&self) -> &RingbufferHeader {
        unsafe { &*(self.ptr as *const RingbufferHeader) }
    }

    pub fn header_mut(&mut self) -> &mut RingbufferHeader {
        unsafe { &mut *(self.ptr as *mut RingbufferHeader) }
    }

    pub fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.size) }
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr as *mut u8, self.size) }
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        unsafe {
            if !self.ptr.is_null() {
                let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr });
            }
            if !self.handle.is_invalid() {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}
