use std::ffi::c_void;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};

/// 共享内存 Ringbuffer 的元数据区域（通常放在 buffer 的最开头）。
/// 用于 App 和 Core 之间同步读写偏移量。
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
                INVALID_HANDLE_VALUE, // page-file backed mapping
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
            unsafe {
                let _ = CloseHandle(handle);
            }
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

    pub fn command_slice(&self, offset: u32, length: u32, max_len: usize) -> anyhow::Result<&[u8]> {
        let header_size = std::mem::size_of::<RingbufferHeader>();
        let offset = offset as usize;
        let length = length as usize;
        if offset < header_size {
            anyhow::bail!("command offset {} overlaps ringbuffer header", offset);
        }
        if length > max_len {
            anyhow::bail!("command length {} exceeds max {}", length, max_len);
        }
        let end = offset
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("command offset + length overflow"))?;
        if end > self.size {
            anyhow::bail!(
                "command slice end {} exceeds mapping size {}",
                end,
                self.size
            );
        }

        let capacity = self.header().capacity as usize;
        if capacity > self.size {
            anyhow::bail!(
                "ringbuffer capacity {} exceeds mapping size {}",
                capacity,
                self.size
            );
        }
        if end > capacity {
            anyhow::bail!(
                "command slice end {} exceeds ringbuffer capacity {}",
                end,
                capacity
            );
        }

        Ok(&self.data()[offset..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_shmem() -> SharedMemory {
        let name = format!(
            "overlay-engine-shmem-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("anon")
        );
        SharedMemory::create(&name, 4096).unwrap()
    }

    #[test]
    fn command_slice_accepts_valid_payload_region() {
        let shmem = create_test_shmem();
        let header_size = std::mem::size_of::<RingbufferHeader>() as u32;
        let slice = shmem.command_slice(header_size, 16, 1024).unwrap();
        assert_eq!(slice.len(), 16);
    }

    #[test]
    fn command_slice_rejects_header_overlap() {
        let shmem = create_test_shmem();
        let err = shmem.command_slice(0, 16, 1024).unwrap_err().to_string();
        assert!(err.contains("overlaps ringbuffer header"));
    }

    #[test]
    fn command_slice_rejects_length_limit() {
        let shmem = create_test_shmem();
        let header_size = std::mem::size_of::<RingbufferHeader>() as u32;
        let err = shmem
            .command_slice(header_size, 2048, 1024)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds max"));
    }

    #[test]
    fn command_slice_rejects_mapping_overflow() {
        let shmem = create_test_shmem();
        let err = shmem
            .command_slice(u32::MAX - 1, 16, 1024)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds mapping size"));
    }

    #[test]
    fn command_slice_rejects_capacity_overflow() {
        let mut shmem = create_test_shmem();
        shmem.header_mut().capacity = std::mem::size_of::<RingbufferHeader>() as u32 + 8;
        let header_size = std::mem::size_of::<RingbufferHeader>() as u32;
        let err = shmem
            .command_slice(header_size, 16, 1024)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds ringbuffer capacity"));
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        unsafe {
            if !self.ptr.is_null() {
                let _ =
                    UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.ptr,
                    });
            }
            if !self.handle.is_invalid() {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}
