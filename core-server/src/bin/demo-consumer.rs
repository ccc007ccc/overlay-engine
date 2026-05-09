use bytes::BytesMut;
use core_server::ipc::protocol::{ControlMessage, MessageHeader, HEADER_SIZE};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

    // Send RegisterConsumer message
    let msg = ControlMessage::RegisterConsumer { pid: std::process::id() };
    let mut buf = BytesMut::new();
    msg.encode(&mut buf);

    client.write_all(&buf).await?;
    println!("Sent RegisterConsumer");

    let mut buf = BytesMut::with_capacity(1024);

    loop {
        // Read header
        let mut header_buf = [0u8; HEADER_SIZE];
        let bytes_read = match client.read_exact(&mut header_buf).await {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e.into()),
        };

        if bytes_read == 0 {
            println!("Server disconnected");
            break;
        }

        buf.extend_from_slice(&header_buf);
        let header = MessageHeader::decode(&mut buf)?;

        // Read payload
        let mut payload_buf = vec![0u8; header.payload_len as usize];
        if header.payload_len > 0 {
            client.read_exact(&mut payload_buf).await?;
            buf.extend_from_slice(&payload_buf);
        }

        let msg = ControlMessage::decode(header.opcode, &mut buf)?;
        println!("Received message: {:?}", msg);

        if let ControlMessage::CanvasAttached { canvas_id, surface_handle, logical_w, logical_h, render_w, render_h } = msg {
            println!("==> Canvas Attached!");
            println!("    ID: {}", canvas_id);
            println!("    Handle: {:#x}", surface_handle);
            println!("    Logical Size: {}x{}", logical_w, logical_h);
            println!("    Render Size: {}x{}", render_w, render_h);
            break;
        }
    }

    println!("Demo consumer exiting.");
    Ok(())
}
