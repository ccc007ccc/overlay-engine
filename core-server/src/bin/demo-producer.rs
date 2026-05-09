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

    // Keep the connection open for a moment
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    println!("Demo producer exiting.");
    Ok(())
}
