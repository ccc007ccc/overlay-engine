use bytes::BytesMut;
use core_server::ipc::protocol::ControlMessage;
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::ClientOptions;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

const PIPE_NAME: &str = r"\\.\pipe\overlay-core";

const CMD_CLEAR: u16 = 0x0101;
const CMD_FILL_RECT: u16 = 0x0102;

fn write_f32(buf: &mut [u8], pos: &mut usize, v: f32) {
    buf[*pos..*pos + 4].copy_from_slice(&v.to_le_bytes());
    *pos += 4;
}

fn write_u16(buf: &mut [u8], pos: &mut usize, v: u16) {
    buf[*pos..*pos + 2].copy_from_slice(&v.to_le_bytes());
    *pos += 2;
}

fn write_cmd_clear(buf: &mut [u8], pos: &mut usize, r: f32, g: f32, b: f32, a: f32) {
    write_u16(buf, pos, CMD_CLEAR);
    write_u16(buf, pos, 16);
    write_f32(buf, pos, r);
    write_f32(buf, pos, g);
    write_f32(buf, pos, b);
    write_f32(buf, pos, a);
}

fn write_cmd_fill_rect(
    buf: &mut [u8], pos: &mut usize,
    x: f32, y: f32, w: f32, h: f32,
    r: f32, g: f32, b: f32, a: f32,
) {
    write_u16(buf, pos, CMD_FILL_RECT);
    write_u16(buf, pos, 32);
    write_f32(buf, pos, x);
    write_f32(buf, pos, y);
    write_f32(buf, pos, w);
    write_f32(buf, pos, h);
    write_f32(buf, pos, r);
    write_f32(buf, pos, g);
    write_f32(buf, pos, b);
    write_f32(buf, pos, a);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1) as u32;
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1) as u32;
    println!("[test-producer] 屏幕分辨率: {}x{}", screen_w, screen_h);
    println!("[test-producer] 连接 {}...", PIPE_NAME);

    let mut client = loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(windows::Win32::Foundation::ERROR_PIPE_BUSY.0 as i32) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e.into()),
        }
    };
    println!("[test-producer] 已连接");

    let mut buf = BytesMut::new();

    // RegisterProducer
    ControlMessage::RegisterProducer { pid: std::process::id() }.encode(&mut buf);
    client.write_all(&buf).await?;
    buf.clear();
    println!("[test-producer] 已注册 Producer");

    // 等 server 处理 RegisterProducer 并创建共享内存
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // CreateCanvas（点对点）
    ControlMessage::CreateCanvas {
        logical_w: screen_w,
        logical_h: screen_h,
        render_w: screen_w,
        render_h: screen_h,
    }.encode(&mut buf);
    client.write_all(&buf).await?;
    buf.clear();
    println!("[test-producer] 已创建画布 {}x{}", screen_w, screen_h);

    // 打开共享内存
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
            0, 0, 0,
        )
    };
    if shmem_ptr.Value.is_null() {
        return Err(anyhow::anyhow!("MapViewOfFile failed"));
    }
    let shmem_bytes = unsafe { std::slice::from_raw_parts_mut(shmem_ptr.Value as *mut u8, 4 * 1024 * 1024) };
    println!("[test-producer] 已打开共享内存: {}", shmem_name);

    println!("[test-producer] 等待 consumer 连接（5秒后开始 attach）...");
    println!("[test-producer] 请现在启动 desktop-window-consumer 或打开 Game Bar widget");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // AttachConsumer（尝试 attach consumer 1-4）
    for cid in 1..=4 {
        ControlMessage::AttachConsumer { canvas_id: 1, consumer_id: cid }.encode(&mut buf);
        client.write_all(&buf).await?;
        buf.clear();
    }
    println!("[test-producer] 已发送 AttachConsumer (1-4)");

    // 持续渲染循环
    println!("[test-producer] 开始渲染循环（Ctrl+C 退出）...");
    let mut frame_id: u64 = 0;
    let cw = screen_w as f32;
    let ch = screen_h as f32;

    let mut last_fps_time = std::time::Instant::now();
    let mut fps_frame_count: u64 = 0;
    let mut current_fps: f32 = 0.0;

    loop {
        frame_id += 1;
        fps_frame_count += 1;
        let t = frame_id as f32 * 0.02;

        let now = std::time::Instant::now();
        let elapsed = now.duration_since(last_fps_time).as_secs_f32();
        if elapsed >= 1.0 {
            current_fps = fps_frame_count as f32 / elapsed;
            fps_frame_count = 0;
            last_fps_time = now;
            println!("[test-producer] FPS: {:.1}", current_fps);
        }

        let cmd_offset: u32 = 24;
        let mut pos = cmd_offset as usize;

        // CLEAR：深色半透明背景（premultiplied: rgb *= alpha）
        let bg_a = 0.5_f32;
        write_cmd_clear(shmem_bytes, &mut pos, 0.03 * bg_a, 0.03 * bg_a, 0.06 * bg_a, bg_a);

        // 屏幕中心十字
        let cross_w = 4.0;
        let cross_len = cw.min(ch) * 0.15;
        // 水平线（绿）
        write_cmd_fill_rect(shmem_bytes, &mut pos,
            cw * 0.5 - cross_len * 0.5, ch * 0.5 - cross_w * 0.5,
            cross_len, cross_w,
            0.2, 0.9, 0.2, 1.0);
        // 竖直线（红）
        write_cmd_fill_rect(shmem_bytes, &mut pos,
            cw * 0.5 - cross_w * 0.5, ch * 0.5 - cross_len * 0.5,
            cross_w, cross_len,
            0.9, 0.2, 0.2, 1.0);

        // 四角色块
        let block = 80.0;
        let margin = 40.0;
        // 左上（cyan）
        write_cmd_fill_rect(shmem_bytes, &mut pos,
            margin, margin, block, block, 0.0, 0.9, 0.9, 1.0);
        // 右上（yellow）
        write_cmd_fill_rect(shmem_bytes, &mut pos,
            cw - margin - block, margin, block, block, 0.9, 0.9, 0.0, 1.0);
        // 左下（magenta）
        write_cmd_fill_rect(shmem_bytes, &mut pos,
            margin, ch - margin - block, block, block, 0.9, 0.0, 0.9, 1.0);
        // 右下（white）
        write_cmd_fill_rect(shmem_bytes, &mut pos,
            cw - margin - block, ch - margin - block, block, block, 0.9, 0.9, 0.9, 1.0);

        // 动态色块（左右来回移动）
        let anim_x = cw * 0.5 + (t.sin() * cw * 0.3);
        let anim_y = ch * 0.3;
        write_cmd_fill_rect(shmem_bytes, &mut pos,
            anim_x - 30.0, anim_y - 30.0, 60.0, 60.0,
            0.9, 0.5, 0.1, 1.0);

        // FPS 条形指示（左上角）：宽度 = fps / 60 * 200px，颜色随 fps 变
        let fps_bar_w = (current_fps / 60.0).clamp(0.0, 1.0) * 200.0;
        let (fr, fg, fb) = if current_fps >= 50.0 {
            (0.2, 0.9, 0.2) // 绿
        } else if current_fps >= 25.0 {
            (0.9, 0.9, 0.2) // 黄
        } else {
            (0.9, 0.2, 0.2) // 红
        };
        // FPS 背景条
        write_cmd_fill_rect(shmem_bytes, &mut pos,
            10.0, 10.0, 210.0, 20.0,
            0.1, 0.1, 0.1, 0.8);
        // FPS 前景条
        write_cmd_fill_rect(shmem_bytes, &mut pos,
            15.0, 13.0, fps_bar_w, 14.0,
            fr, fg, fb, 1.0);

        let cmd_length = (pos - cmd_offset as usize) as u32;

        // SubmitFrame
        ControlMessage::SubmitFrame {
            canvas_id: 1,
            frame_id,
            offset: cmd_offset,
            length: cmd_length,
        }.encode(&mut buf);
        client.write_all(&buf).await?;
        buf.clear();

        if frame_id % 60 == 0 {
            println!("[test-producer] frame {} FPS={:.1} (cmds={} bytes)", frame_id, current_fps, cmd_length);
        }

        tokio::time::sleep(std::time::Duration::from_millis(16)).await;
    }
}
