use bytes::BytesMut;
use core_server::ipc::protocol::ControlMessage;
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::ClientOptions;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

const PIPE_NAME: &str = r"\\.\pipe\overlay-core";

const CMD_CLEAR: u16 = 0x0101;
const CMD_FILL_RECT: u16 = 0x0102;
const CMD_DRAW_TEXT: u16 = 0x010B;
// Space-stack opcodes — task 3.2 of the `animation-and-viewport-fix` spec
// (core-server/src/ipc/cmd_decoder.rs). Used below to surround any draws
// that should be anchored to each monitor's client-area origin.
const CMD_PUSH_SPACE: u16 = 0x0109;
const CMD_POP_SPACE: u16 = 0x010A;
const SPACE_ID_MONITOR_LOCAL: u32 = 1;

fn write_f32(buf: &mut [u8], pos: &mut usize, v: f32) {
    buf[*pos..*pos + 4].copy_from_slice(&v.to_le_bytes());
    *pos += 4;
}

fn write_u16(buf: &mut [u8], pos: &mut usize, v: u16) {
    buf[*pos..*pos + 2].copy_from_slice(&v.to_le_bytes());
    *pos += 2;
}

fn write_u32(buf: &mut [u8], pos: &mut usize, v: u32) {
    buf[*pos..*pos + 4].copy_from_slice(&v.to_le_bytes());
    *pos += 4;
}

/// Emit a `CMD_PUSH_SPACE` with the given `space_id`.
///
/// Wire format (cmd_decoder.rs): `u16 opcode | u16 payload_len=4 | u32 space_id`.
/// Used to switch subsequent geometry commands to MonitorLocal (per-Monitor
/// client-area coordinates) until the matching `CMD_POP_SPACE`.
fn write_cmd_push_space(buf: &mut [u8], pos: &mut usize, space_id: u32) {
    write_u16(buf, pos, CMD_PUSH_SPACE);
    write_u16(buf, pos, 4);
    write_u32(buf, pos, space_id);
}

/// Emit a `CMD_POP_SPACE` (empty payload).
fn write_cmd_pop_space(buf: &mut [u8], pos: &mut usize) {
    write_u16(buf, pos, CMD_POP_SPACE);
    write_u16(buf, pos, 0);
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
    buf: &mut [u8],
    pos: &mut usize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
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

fn write_cmd_draw_text(
    buf: &mut [u8],
    pos: &mut usize,
    text: &str,
    x: f32,
    y: f32,
    font_size: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    let bytes = text.as_bytes();
    let text_len = bytes.len().min(u16::MAX as usize);
    let payload_len = 30 + text_len;
    write_u16(buf, pos, CMD_DRAW_TEXT);
    write_u16(buf, pos, payload_len as u16);
    write_f32(buf, pos, x);
    write_f32(buf, pos, y);
    write_f32(buf, pos, font_size);
    write_f32(buf, pos, r);
    write_f32(buf, pos, g);
    write_f32(buf, pos, b);
    write_f32(buf, pos, a);
    write_u16(buf, pos, text_len as u16);
    buf[*pos..*pos + text_len].copy_from_slice(&bytes[..text_len]);
    *pos += text_len;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let unlocked = args.iter().any(|a| a == "--unlocked" || a == "--no-vsync");

    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1) as u32;
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1) as u32;
    println!("[demo-app] 屏幕分辨率: {}x{}", screen_w, screen_h);
    if unlocked {
        println!("[demo-app] 模式: 无帧数限制 (Unlocked)");
    } else {
        println!("[demo-app] 模式: DWM VSync (锁定帧率)");
    }
    println!("[demo-app] 连接 {}...", PIPE_NAME);

    let mut client = loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(c) => break c,
            Err(e)
                if e.raw_os_error()
                    == Some(windows::Win32::Foundation::ERROR_PIPE_BUSY.0 as i32) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e.into()),
        }
    };
    println!("[demo-app] 已连接");

    let mut buf = BytesMut::new();

    // RegisterApp
    ControlMessage::RegisterApp {
        pid: std::process::id(),
    }
    .encode(&mut buf);
    client.write_all(&buf).await?;
    buf.clear();
    println!("[demo-app] 已注册 App");

    // 等 server 处理 RegisterApp 并创建共享内存
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // CreateCanvas（点对点）
    ControlMessage::CreateCanvas {
        logical_w: screen_w,
        logical_h: screen_h,
        render_w: screen_w,
        render_h: screen_h,
    }
    .encode(&mut buf);
    client.write_all(&buf).await?;
    buf.clear();
    println!("[demo-app] 已创建画布 {}x{}", screen_w, screen_h);

    // 打开共享内存
    let shmem_name = format!("overlay-core-cmds-{}", std::process::id());
    let shmem_name_w: Vec<u16> = shmem_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
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
    let shmem_bytes =
        unsafe { std::slice::from_raw_parts_mut(shmem_ptr.Value as *mut u8, 4 * 1024 * 1024) };
    println!("[demo-app] 已打开共享内存: {}", shmem_name);

    // 持续渲染循环
    println!("[demo-app] 开始渲染循环（DWM vsync，Ctrl+C 退出）...");
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
        }

        let cmd_offset: u32 = 24;
        let mut pos = cmd_offset as usize;

        // CLEAR：深色半透明背景（premultiplied: rgb *= alpha）
        let bg_a = 0.5_f32;
        write_cmd_clear(
            shmem_bytes,
            &mut pos,
            0.03 * bg_a,
            0.03 * bg_a,
            0.06 * bg_a,
            bg_a,
        );

        // 屏幕中心十字
        let cross_w = 4.0;
        let cross_len = cw.min(ch) * 0.15;
        // 水平线（绿）
        write_cmd_fill_rect(
            shmem_bytes,
            &mut pos,
            cw * 0.5 - cross_len * 0.5,
            ch * 0.5 - cross_w * 0.5,
            cross_len,
            cross_w,
            0.2,
            0.9,
            0.2,
            1.0,
        );
        // 竖直线（红）
        write_cmd_fill_rect(
            shmem_bytes,
            &mut pos,
            cw * 0.5 - cross_w * 0.5,
            ch * 0.5 - cross_len * 0.5,
            cross_w,
            cross_len,
            0.9,
            0.2,
            0.2,
            1.0,
        );

        // 四角色块
        let block = 80.0;
        let margin = 40.0;
        // 右上（yellow） — World space: 单张全局画布的右上角,所有 monitor
        // 共享同一张 surface,它们会看到此色块在同一画布位置的不同视角。
        write_cmd_fill_rect(
            shmem_bytes,
            &mut pos,
            cw - margin - block,
            margin,
            block,
            block,
            0.9,
            0.9,
            0.0,
            1.0,
        );
        // 左下（magenta） — World space
        write_cmd_fill_rect(
            shmem_bytes,
            &mut pos,
            margin,
            ch - margin - block,
            block,
            block,
            0.9,
            0.0,
            0.9,
            1.0,
        );
        // 右下（white） — World space
        write_cmd_fill_rect(
            shmem_bytes,
            &mut pos,
            cw - margin - block,
            ch - margin - block,
            block,
            block,
            0.9,
            0.9,
            0.9,
            1.0,
        );

        // 动态色块（左右来回移动） — World space
        let anim_x = cw * 0.5 + (t.sin() * cw * 0.3);
        let anim_y = ch * 0.3;
        write_cmd_fill_rect(
            shmem_bytes,
            &mut pos,
            anim_x - 30.0,
            anim_y - 30.0,
            60.0,
            60.0,
            0.9,
            0.5,
            0.1,
            1.0,
        );

        // ---- MonitorLocal 区间开始 ---------------------------------
        // 接下来的几个元素语义是"贴每个 monitor 客户区左上角"——
        // Core 会把它们 replay 到每个 monitor 自己的 per-Monitor surface
        // 上,使得每个窗口都独立在自己客户区 (10, 10) 附近看到 FPS 条/徽章
        // (缺陷 B 的修复要求).
        //
        // 如果你起两个 desktop-window-monitor 并拖到屏幕不同位置:
        //   * yellow/magenta/white 三个 World 块 仍然挂在同一张全局画布上,
        //     每个窗口按自己的 viewport 透视;
        //   * cyan 徽章 + FPS 条独立出现在两个窗口各自的 (margin, margin)
        //     / (10, 10) 客户区位置 —— 这是修复前做不到的.
        write_cmd_push_space(shmem_bytes, &mut pos, SPACE_ID_MONITOR_LOCAL);

        // CLEAR MonitorLocal：必须加上这一步，否则多缓冲机制下上一帧的字会残留在屏幕上！
        // 渲染背景完全透明，只保留我们的绘制内容。
        write_cmd_clear(shmem_bytes, &mut pos, 0.0, 0.0, 0.0, 0.0);

        // 左上 cyan 徽章(MonitorLocal): 每个 monitor 客户区左上 (margin, margin)
        write_cmd_fill_rect(
            shmem_bytes,
            &mut pos,
            margin,
            margin,
            block,
            block,
            0.0,
            0.9,
            0.9,
            1.0,
        );

        // FPS 数字（左上角, MonitorLocal）
        let fps_text = format!("{:.0} FPS", current_fps);
        let font_size = 20.0_f32;
        let char_w = font_size * 0.55;
        let pad_x = 6.0_f32;
        let line_h = font_size * 1.35;
        let bg_w = fps_text.len() as f32 * char_w + pad_x * 2.0;
        let bg_h = line_h + 4.0;
        let bg_a = 0.45_f32;
        write_cmd_fill_rect(
            shmem_bytes,
            &mut pos,
            10.0,
            10.0,
            bg_w,
            bg_h,
            0.05 * bg_a,
            0.05 * bg_a,
            0.05 * bg_a,
            bg_a,
        );
        write_cmd_draw_text(
            shmem_bytes,
            &mut pos,
            &fps_text,
            10.0 + pad_x,
            10.0 + (bg_h - line_h) / 2.0,
            font_size,
            0.2,
            0.9,
            0.2,
            1.0,
        );

        write_cmd_pop_space(shmem_bytes, &mut pos);
        // ---- MonitorLocal 区间结束 ---------------------------------

        let cmd_length = (pos - cmd_offset as usize) as u32;

        // SubmitFrame
        ControlMessage::SubmitFrame {
            canvas_id: 0,
            frame_id,
            offset: cmd_offset,
            length: cmd_length,
        }
        .encode(&mut buf);
        client.write_all(&buf).await?;
        buf.clear();

        if !unlocked {
            if let Err(e) = unsafe { DwmFlush() } {
                eprintln!("[demo-app] DwmFlush failed: {}", e);
                tokio::task::yield_now().await;
            }
        } else {
            // Even unlocked, we yield to allow the tokio runtime to process
            // other background events if necessary, preventing absolute starvation.
            tokio::task::yield_now().await;
        }
    }
}
