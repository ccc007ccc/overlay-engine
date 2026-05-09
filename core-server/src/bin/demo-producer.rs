use bytes::BytesMut;
use core_server::ipc::protocol::ControlMessage;
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::ClientOptions;

const PIPE_NAME: &str = r"\\.\pipe\overlay-core";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Connecting to {}...", PIPE_NAME);

    // Wait for the server to be ready
    let mut client = loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(client) => break client,
            Err(e) if e.raw_os_error() == Some(windows::Win32::Foundation::ERROR_PIPE_BUSY.0 as i32) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    };

    println!("Connected!");

    // Send RegisterProducer message
    let msg = ControlMessage::RegisterProducer { pid: std::process::id() };
    let mut buf = BytesMut::new();
    msg.encode(&mut buf);

    client.write_all(&buf).await?;
    println!("Sent RegisterProducer");

    // Send CreateCanvas message
    let msg = ControlMessage::CreateCanvas {
        logical_w: 1920,
        logical_h: 1080,
        render_w: 1280,
        render_h: 720,
    };
    buf.clear();
    msg.encode(&mut buf);

    client.write_all(&buf).await?;
    println!("Sent CreateCanvas");

    // Wait for a second so consumer can register
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Send AttachConsumer
    let msg = ControlMessage::AttachConsumer {
        canvas_id: 1,
        consumer_id: 1,
    };
    buf.clear();
    msg.encode(&mut buf);

    client.write_all(&buf).await?;
    println!("Sent AttachConsumer");

    // 打开 core-server 为我们创建的共享内存 ringbuffer
    let shmem_name = format!("overlay-core-cmds-{}", std::process::id());
    let shmem_name_w: Vec<u16> = shmem_name.encode_utf16().chain(std::iter::once(0)).collect();
    let shmem_handle = unsafe {
        windows::Win32::System::Memory::OpenFileMappingW(
            windows::Win32::System::Memory::FILE_MAP_ALL_ACCESS.0,
            false,
            windows::core::PCWSTR(shmem_name_w.as_ptr()),
        )?
    };
    let shmem_ptr = unsafe {
        windows::Win32::System::Memory::MapViewOfFile(
            shmem_handle,
            windows::Win32::System::Memory::FILE_MAP_ALL_ACCESS,
            0,
            0,
            0,
        )
    };
    if shmem_ptr.Value.is_null() {
        return Err(anyhow::anyhow!("MapViewOfFile failed"));
    }
    println!("Opened shared memory: {}", shmem_name);

    // 写一条 CLEAR(cyan) 命令到 ringbuffer（跳过 header 区域 = 24 bytes）
    let cmd_offset: u32 = 24; // RingbufferHeader size
    let shmem_bytes = unsafe { std::slice::from_raw_parts_mut(shmem_ptr.Value as *mut u8, 4 * 1024 * 1024) };
    let mut pos = cmd_offset as usize;
    // opcode = 0x0101 (CLEAR), payload_len = 16
    shmem_bytes[pos..pos+2].copy_from_slice(&0x0101u16.to_le_bytes()); pos += 2;
    shmem_bytes[pos..pos+2].copy_from_slice(&16u16.to_le_bytes()); pos += 2;
    // RGBA: cyan = (0.0, 1.0, 1.0, 1.0)
    shmem_bytes[pos..pos+4].copy_from_slice(&0.0f32.to_le_bytes()); pos += 4;
    shmem_bytes[pos..pos+4].copy_from_slice(&1.0f32.to_le_bytes()); pos += 4;
    shmem_bytes[pos..pos+4].copy_from_slice(&1.0f32.to_le_bytes()); pos += 4;
    shmem_bytes[pos..pos+4].copy_from_slice(&1.0f32.to_le_bytes()); pos += 4;
    let cmd_length = (pos - cmd_offset as usize) as u32;

    // 发送 SubmitFrame
    let msg = ControlMessage::SubmitFrame {
        canvas_id: 1,
        frame_id: 1,
        offset: cmd_offset,
        length: cmd_length,
    };
    buf.clear();
    msg.encode(&mut buf);
    client.write_all(&buf).await?;
    println!("Sent SubmitFrame (offset={}, length={})", cmd_offset, cmd_length);

    // Keep the connection open for a moment
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    println!("Demo producer exiting.");
    Ok(())
}
