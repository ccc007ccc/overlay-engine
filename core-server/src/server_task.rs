use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use crate::ipc::protocol::{ControlMessage, MessageHeader, HEADER_SIZE};

pub const PIPE_NAME: &str = r"\\.\pipe\overlay-core";

pub async fn run_server() -> anyhow::Result<()> {
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(PIPE_NAME)?;

    println!("Core Server listening on {}", PIPE_NAME);

    loop {
        // Wait for a client to connect
        server.connect().await?;
        println!("Client connected");

        let connected_client = server;

        // Prepare a new pipe instance for the next client
        server = ServerOptions::new().create(PIPE_NAME)?;

        // Spawn a new task to handle the connected client
        tokio::spawn(async move {
            if let Err(e) = handle_client(connected_client).await {
                eprintln!("Client error: {}", e);
            }
        });
    }
}

async fn handle_client(pipe: NamedPipeServer) -> anyhow::Result<()> {
    let (mut rh, mut wh) = tokio::io::split(pipe);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ControlMessage>();

    let mut client_id: Option<(u32, bool)> = None; // (id, is_producer)

    // Spawn writer task
    let _writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let mut buf = BytesMut::new();
            msg.encode(&mut buf);
            if let Err(e) = wh.write_all(&buf).await {
                eprintln!("Write error: {}", e);
                break;
            }
        }
    });

    let mut buf = BytesMut::with_capacity(1024);

    loop {
        // Read header
        let mut header_buf = [0u8; HEADER_SIZE];
        let bytes_read = match rh.read_exact(&mut header_buf).await {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e.into()),
        };

        if bytes_read == 0 {
            println!("Client disconnected");
            break;
        }

        buf.extend_from_slice(&header_buf);
        let header = MessageHeader::decode(&mut buf)?;

        // Read payload
        let mut payload_buf = vec![0u8; header.payload_len as usize];
        if header.payload_len > 0 {
            rh.read_exact(&mut payload_buf).await?;
            buf.extend_from_slice(&payload_buf);
        }

        let msg = ControlMessage::decode(header.opcode, &mut buf)?;
        println!("Received message: {:?}", msg);

        // Process message
        match msg {
            ControlMessage::RegisterProducer { pid } => {
                let id = {
                    let mut state = crate::ipc::server::SERVER_STATE.write();
                    state.register_producer(pid, windows::Win32::Foundation::HANDLE::default())?
                };
                client_id = Some((id, true));
                println!("Registered Producer with ID: {} (PID: {})", id, pid);
            }
            ControlMessage::RegisterConsumer { pid } => {
                let id = {
                    let mut state = crate::ipc::server::SERVER_STATE.write();
                    state.register_consumer(pid, windows::Win32::Foundation::HANDLE::default(), tx.clone())
                };
                client_id = Some((id, false));
                println!("Registered Consumer with ID: {} (PID: {})", id, pid);
            }
            ControlMessage::CreateCanvas { logical_w, logical_h, render_w, render_h } => {
                if let Some((id, true)) = client_id {
                    let canvas_id = {
                        let mut state = crate::ipc::server::SERVER_STATE.write();
                        state.create_canvas(id, logical_w, logical_h, render_w, render_h)?
                    };
                    println!("CreateCanvas created ID {} for Producer {}", canvas_id, id);
                } else {
                    eprintln!("Error: CreateCanvas received but client is not a registered producer");
                }
            }
            ControlMessage::AttachConsumer { canvas_id, consumer_id } => {
                if let Some((_id, true)) = client_id {
                    let mut state = crate::ipc::server::SERVER_STATE.write();
                    if let Err(e) = state.attach_consumer(canvas_id, consumer_id) {
                        eprintln!("AttachConsumer error: {}", e);
                    } else {
                        println!("Attached Canvas {} to Consumer {}", canvas_id, consumer_id);
                    }
                } else {
                    eprintln!("Error: AttachConsumer received but client is not a registered producer");
                }
            }
            ControlMessage::SubmitFrame { canvas_id, frame_id, offset, length } => {
                if let Some((producer_id, true)) = client_id {
                    let state = crate::ipc::server::SERVER_STATE.read();
                    if let Some(producer) = state.producers.get(&producer_id) {
                        if let Some(ref ringbuf) = producer.command_ringbuffer {
                            let data = ringbuf.data();
                            let start = offset as usize;
                            let end = start + length as usize;
                            if end <= data.len() {
                                let cmds = crate::ipc::cmd_decoder::decode_commands(&data[start..end]);
                                if let Some(canvas) = state.canvases.get(&canvas_id) {
                                    let rw = canvas.resources.render_w;
                                    let rh = canvas.resources.render_h;
                                    let ctx = &state.devices.d3d_ctx;

                                    for cmd in &cmds {
                                        match cmd {
                                            crate::ipc::cmd_decoder::RenderCommand::Clear(c) => {
                                                unsafe {
                                                    ctx.ClearRenderTargetView(&canvas.resources.rtv, c);
                                                }
                                            }
                                            crate::ipc::cmd_decoder::RenderCommand::Draw(
                                                crate::renderer::painter::DrawCmd::FillRect { x, y, w, h, rgba }
                                            ) => {
                                                let x0 = (*x as u32).min(rw);
                                                let y0 = (*y as u32).min(rh);
                                                let x1 = ((*x + *w) as u32).min(rw);
                                                let y1 = ((*y + *h) as u32).min(rh);
                                                if x1 > x0 && y1 > y0 {
                                                    let bw = x1 - x0;
                                                    let bh = y1 - y0;
                                                    let b = (rgba[2].clamp(0.0, 1.0) * 255.0) as u8;
                                                    let g = (rgba[1].clamp(0.0, 1.0) * 255.0) as u8;
                                                    let r = (rgba[0].clamp(0.0, 1.0) * 255.0) as u8;
                                                    let a = (rgba[3].clamp(0.0, 1.0) * 255.0) as u8;
                                                    let mut pixels = vec![0u8; (bw * bh * 4) as usize];
                                                    for chunk in pixels.chunks_exact_mut(4) {
                                                        chunk[0] = b;
                                                        chunk[1] = g;
                                                        chunk[2] = r;
                                                        chunk[3] = a;
                                                    }
                                                    use windows::core::Interface;
                                                    if let Ok(resource) = canvas.resources.texture.cast::<windows::Win32::Graphics::Direct3D11::ID3D11Resource>() {
                                                        let d3d_box = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
                                                            left: x0,
                                                            top: y0,
                                                            front: 0,
                                                            right: x1,
                                                            bottom: y1,
                                                            back: 1,
                                                        };
                                                        unsafe {
                                                            ctx.UpdateSubresource(
                                                                &resource, 0, Some(&d3d_box),
                                                                pixels.as_ptr() as *const _,
                                                                bw * 4, bw * bh * 4,
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }

                                    unsafe {
                                        ctx.Flush();
                                        let _ = canvas.resources.surface.SetBuffer(&canvas.resources.buffer);
                                        let _ = canvas.resources.manager.Present();
                                    }

                                    if frame_id % 60 == 0 {
                                        println!("SubmitFrame: canvas={} frame={} cmds={}", canvas_id, frame_id, cmds.len());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Cleanup on disconnect
    if let Some((id, is_producer)) = client_id {
        let mut state = crate::ipc::server::SERVER_STATE.write();
        if is_producer {
            println!("Cleaning up Producer {}", id);
            state.remove_producer(id);
        } else {
            println!("Cleaning up Consumer {}", id);
            state.remove_consumer(id);
        }
    }

    Ok(())
}
