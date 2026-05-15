use bytes::BytesMut;
use core_server::ipc::protocol::{
    ControlMessage, DesktopWindowMode, MessageHeader, MonitorKind, MonitorRequestStatus,
    MonitorTypeEntry, DESKTOP_WINDOW_FLAG_CLICK_THROUGH, HEADER_SIZE,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

const PIPE_NAME: &str = r"\\.\pipe\overlay-core";

const CMD_CLEAR: u16 = 0x0101;
const CMD_FILL_RECT: u16 = 0x0102;
const CMD_STROKE_RECT: u16 = 0x0103;
const CMD_FILL_ROUNDED_RECT: u16 = 0x0104;
const CMD_STROKE_ROUNDED_RECT: u16 = 0x0105;
const CMD_FILL_ELLIPSE: u16 = 0x0106;
const CMD_STROKE_ELLIPSE: u16 = 0x0107;
const CMD_DRAW_LINE: u16 = 0x0108;
const CMD_PUSH_SPACE: u16 = 0x0109;
const CMD_POP_SPACE: u16 = 0x010A;
const CMD_DRAW_TEXT: u16 = 0x010B;
const CMD_DRAW_BITMAP: u16 = 0x010C;
const SPACE_ID_MONITOR_LOCAL: u32 = 1;

const TEXTURE_ORB: u32 = 1;
const TEXTURE_GRID: u32 = 2;
const TEXTURE_STRIPES: u32 = 3;

static ORB_PNG: &[u8] = include_bytes!("../../assets/demo-textures/orb.png");
static GRID_PNG: &[u8] = include_bytes!("../../assets/demo-textures/grid.png");
static STRIPES_PNG: &[u8] = include_bytes!("../../assets/demo-textures/stripes.png");

#[derive(Debug, Clone, Copy)]
struct DemoOptions {
    unlocked: bool,
    desktop_monitors: u32,
    window_mode: DesktopWindowMode,
    click_through: bool,
}

impl Default for DemoOptions {
    fn default() -> Self {
        Self {
            unlocked: false,
            desktop_monitors: 0,
            window_mode: DesktopWindowMode::Bordered,
            click_through: false,
        }
    }
}

fn parse_demo_options() -> anyhow::Result<Option<DemoOptions>> {
    let mut options = DemoOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--unlocked" | "--no-vsync" => options.unlocked = true,
            "--desktop-monitors" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--desktop-monitors requires a count"))?;
                options.desktop_monitors = value.parse()?;
            }
            "--window-mode" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--window-mode requires a value"))?;
                options.window_mode = parse_window_mode(&value)?;
            }
            "--click-through" => options.click_through = true,
            "-h" | "--help" => {
                print_usage();
                return Ok(None);
            }
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }
    Ok(Some(options))
}

fn parse_window_mode(value: &str) -> anyhow::Result<DesktopWindowMode> {
    match value {
        "bordered" => Ok(DesktopWindowMode::Bordered),
        "borderless" => Ok(DesktopWindowMode::Borderless),
        "borderless-fullscreen" | "fullscreen" => Ok(DesktopWindowMode::BorderlessFullscreen),
        _ => anyhow::bail!("unknown window mode: {value}"),
    }
}

fn window_mode_label(mode: DesktopWindowMode) -> &'static str {
    match mode {
        DesktopWindowMode::Bordered => "bordered",
        DesktopWindowMode::Borderless => "borderless",
        DesktopWindowMode::BorderlessFullscreen => "borderless-fullscreen",
    }
}

fn print_usage() {
    println!("Usage: demo-app [--unlocked] [--desktop-monitors N] [--window-mode bordered|borderless|fullscreen] [--click-through]");
}

async fn send_control_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: ControlMessage,
    buf: &mut BytesMut,
) -> anyhow::Result<()> {
    msg.encode(buf);
    writer.write_all(buf).await?;
    buf.clear();
    Ok(())
}

async fn read_control_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> anyhow::Result<Option<ControlMessage>> {
    let mut header_buf = [0u8; HEADER_SIZE];
    reader.read_exact(&mut header_buf).await?;
    let mut header_bytes = BytesMut::from(&header_buf[..]);
    let header = MessageHeader::decode(&mut header_bytes)?;

    let mut payload_buf = vec![0u8; header.payload_len as usize];
    if !payload_buf.is_empty() {
        reader.read_exact(&mut payload_buf).await?;
    }
    let mut payload = BytesMut::from(&payload_buf[..]);
    Ok(ControlMessage::decode(
        header.opcode,
        header.payload_len,
        &mut payload,
    )?)
}

async fn wait_monitor_types<R: AsyncRead + Unpin>(
    reader: &mut R,
    request_id: u32,
) -> anyhow::Result<Vec<MonitorTypeEntry>> {
    loop {
        if let Some(ControlMessage::MonitorTypes {
            request_id: id,
            entries,
        }) = read_control_message(reader).await?
        {
            if id == request_id {
                return Ok(entries);
            }
        }
    }
}

async fn wait_start_monitor_result<R: AsyncRead + Unpin>(
    reader: &mut R,
    request_id: u32,
) -> anyhow::Result<(MonitorRequestStatus, Vec<u32>)> {
    loop {
        if let Some(ControlMessage::StartMonitorResult {
            request_id: id,
            status,
            monitor_ids,
        }) = read_control_message(reader).await?
        {
            if id == request_id {
                return Ok((status, monitor_ids));
            }
        }
    }
}

fn print_monitor_catalog(entries: &[MonitorTypeEntry]) {
    for entry in entries {
        println!(
            "[demo-app] Monitor {:?}: available={} policy={:?} max={} core_startable={} core_managed={} modes=0x{:x} flags=0x{:x}",
            entry.kind,
            entry.available,
            entry.start_policy,
            entry.max_instances,
            entry.core_startable,
            entry.core_managed,
            entry.window_modes,
            entry.flags
        );
    }
}

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

fn write_i32(buf: &mut [u8], pos: &mut usize, v: i32) {
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

fn write_cmd_stroke_rect(
    buf: &mut [u8],
    pos: &mut usize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    stroke_width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    write_u16(buf, pos, CMD_STROKE_RECT);
    write_u16(buf, pos, 36);
    write_f32(buf, pos, x);
    write_f32(buf, pos, y);
    write_f32(buf, pos, w);
    write_f32(buf, pos, h);
    write_f32(buf, pos, stroke_width);
    write_f32(buf, pos, r);
    write_f32(buf, pos, g);
    write_f32(buf, pos, b);
    write_f32(buf, pos, a);
}

fn write_cmd_fill_rounded_rect(
    buf: &mut [u8],
    pos: &mut usize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius_x: f32,
    radius_y: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    write_u16(buf, pos, CMD_FILL_ROUNDED_RECT);
    write_u16(buf, pos, 40);
    write_f32(buf, pos, x);
    write_f32(buf, pos, y);
    write_f32(buf, pos, w);
    write_f32(buf, pos, h);
    write_f32(buf, pos, radius_x);
    write_f32(buf, pos, radius_y);
    write_f32(buf, pos, r);
    write_f32(buf, pos, g);
    write_f32(buf, pos, b);
    write_f32(buf, pos, a);
}

fn write_cmd_stroke_rounded_rect(
    buf: &mut [u8],
    pos: &mut usize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius_x: f32,
    radius_y: f32,
    stroke_width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    write_u16(buf, pos, CMD_STROKE_ROUNDED_RECT);
    write_u16(buf, pos, 44);
    write_f32(buf, pos, x);
    write_f32(buf, pos, y);
    write_f32(buf, pos, w);
    write_f32(buf, pos, h);
    write_f32(buf, pos, radius_x);
    write_f32(buf, pos, radius_y);
    write_f32(buf, pos, stroke_width);
    write_f32(buf, pos, r);
    write_f32(buf, pos, g);
    write_f32(buf, pos, b);
    write_f32(buf, pos, a);
}

fn write_cmd_fill_ellipse(
    buf: &mut [u8],
    pos: &mut usize,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    write_u16(buf, pos, CMD_FILL_ELLIPSE);
    write_u16(buf, pos, 32);
    write_f32(buf, pos, cx);
    write_f32(buf, pos, cy);
    write_f32(buf, pos, rx);
    write_f32(buf, pos, ry);
    write_f32(buf, pos, r);
    write_f32(buf, pos, g);
    write_f32(buf, pos, b);
    write_f32(buf, pos, a);
}

fn write_cmd_stroke_ellipse(
    buf: &mut [u8],
    pos: &mut usize,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    stroke_width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    write_u16(buf, pos, CMD_STROKE_ELLIPSE);
    write_u16(buf, pos, 36);
    write_f32(buf, pos, cx);
    write_f32(buf, pos, cy);
    write_f32(buf, pos, rx);
    write_f32(buf, pos, ry);
    write_f32(buf, pos, stroke_width);
    write_f32(buf, pos, r);
    write_f32(buf, pos, g);
    write_f32(buf, pos, b);
    write_f32(buf, pos, a);
}

fn write_cmd_draw_line(
    buf: &mut [u8],
    pos: &mut usize,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    stroke_width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    write_u16(buf, pos, CMD_DRAW_LINE);
    write_u16(buf, pos, 40);
    write_f32(buf, pos, x0);
    write_f32(buf, pos, y0);
    write_f32(buf, pos, x1);
    write_f32(buf, pos, y1);
    write_f32(buf, pos, stroke_width);
    write_f32(buf, pos, r);
    write_f32(buf, pos, g);
    write_f32(buf, pos, b);
    write_f32(buf, pos, a);
    write_i32(buf, pos, 0);
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

fn write_cmd_draw_bitmap(
    buf: &mut [u8],
    pos: &mut usize,
    bitmap_id: u32,
    src: (f32, f32, f32, f32),
    dst: (f32, f32, f32, f32),
    opacity: f32,
    interp_mode: i32,
) {
    write_u16(buf, pos, CMD_DRAW_BITMAP);
    write_u16(buf, pos, 44);
    write_u32(buf, pos, bitmap_id);
    write_f32(buf, pos, src.0);
    write_f32(buf, pos, src.1);
    write_f32(buf, pos, src.2);
    write_f32(buf, pos, src.3);
    write_f32(buf, pos, dst.0);
    write_f32(buf, pos, dst.1);
    write_f32(buf, pos, dst.2);
    write_f32(buf, pos, dst.3);
    write_f32(buf, pos, opacity);
    write_i32(buf, pos, interp_mode);
}

fn wave01(v: f32) -> f32 {
    (v.sin() * 0.5 + 0.5).clamp(0.0, 1.0)
}

fn write_complex_animation_scene(buf: &mut [u8], pos: &mut usize, cw: f32, ch: f32, t: f32) {
    let min_dim = cw.min(ch);
    let center_x = cw * 0.5;
    let center_y = ch * 0.5;
    let tau = std::f32::consts::TAU;

    for i in 0..24 {
        let fi = i as f32;
        let angle = t * 0.55 + fi * tau / 24.0;
        let x0 = center_x + angle.cos() * min_dim * 0.08;
        let y0 = center_y + angle.sin() * min_dim * 0.05;
        let x1 = center_x + angle.cos() * min_dim * 0.35;
        let y1 = center_y + angle.sin() * min_dim * 0.24;
        let a = 0.10 + wave01(t * 1.7 + fi * 0.31) * 0.16;
        write_cmd_draw_line(buf, pos, x0, y0, x1, y1, 1.0, 0.15, 0.50, 1.00, a);
    }

    let panel_w = (cw * 0.42).clamp(420.0, 860.0);
    let panel_h = (ch * 0.34).clamp(260.0, 520.0);
    let panel_x = center_x - panel_w * 0.5 + (t * 0.35).sin() * cw * 0.035;
    let panel_y = center_y - panel_h * 0.5 + (t * 0.27).cos() * ch * 0.035;
    write_cmd_fill_rounded_rect(
        buf,
        pos,
        panel_x + 12.0,
        panel_y + 16.0,
        panel_w,
        panel_h,
        30.0,
        30.0,
        0.0,
        0.0,
        0.0,
        0.22,
    );
    write_cmd_fill_rounded_rect(
        buf, pos, panel_x, panel_y, panel_w, panel_h, 28.0, 28.0, 0.03, 0.06, 0.12, 0.68,
    );
    write_cmd_stroke_rounded_rect(
        buf, pos, panel_x, panel_y, panel_w, panel_h, 28.0, 28.0, 2.0, 0.20, 0.70, 1.00, 0.62,
    );

    write_cmd_draw_bitmap(
        buf,
        pos,
        TEXTURE_GRID,
        (0.0, 0.0, 0.0, 0.0),
        (
            panel_x + 18.0,
            panel_y + 18.0,
            panel_w - 36.0,
            panel_h - 36.0,
        ),
        0.34,
        1,
    );
    let stripe_src_x = (t * 22.0).rem_euclid(32.0);
    write_cmd_draw_bitmap(
        buf,
        pos,
        TEXTURE_STRIPES,
        (stripe_src_x, 0.0, 96.0, 128.0),
        (
            panel_x + panel_w * 0.57,
            panel_y + 26.0,
            panel_w * 0.34,
            panel_h - 52.0,
        ),
        0.24 + wave01(t * 1.4) * 0.20,
        1,
    );

    let cols = 18usize;
    let rows = 10usize;
    let cell_w = panel_w / cols as f32;
    let cell_h = panel_h / rows as f32;
    for row in 0..rows {
        for col in 0..cols {
            let u = col as f32 / (cols - 1) as f32;
            let v = row as f32 / (rows - 1) as f32;
            let phase = t * 3.0 + col as f32 * 0.65 + row as f32 * 0.42;
            let level = wave01(phase);
            let a = 0.16 + level * 0.42;
            write_cmd_fill_rect(
                buf,
                pos,
                panel_x + col as f32 * cell_w + 2.0,
                panel_y + row as f32 * cell_h + 2.0,
                cell_w - 4.0,
                cell_h - 4.0,
                0.08 + u * 0.38,
                0.22 + level * 0.58,
                0.95 - u * 0.32 + v * 0.10,
                a,
            );
        }
    }

    let lens_x = panel_x + panel_w * (0.5 + (t * 0.63).sin() * 0.30);
    let lens_y = panel_y + panel_h * (0.5 + (t * 0.81).cos() * 0.24);
    write_cmd_fill_ellipse(buf, pos, lens_x, lens_y, 62.0, 62.0, 1.0, 1.0, 1.0, 0.16);
    write_cmd_stroke_ellipse(
        buf, pos, lens_x, lens_y, 70.0, 70.0, 3.0, 0.55, 0.95, 1.0, 0.72,
    );
    let orb_size = 104.0 + wave01(t * 2.1) * 58.0;
    write_cmd_draw_bitmap(
        buf,
        pos,
        TEXTURE_ORB,
        (0.0, 0.0, 0.0, 0.0),
        (
            lens_x - orb_size * 0.5,
            lens_y - orb_size * 0.5,
            orb_size,
            orb_size,
        ),
        0.72 + wave01(t * 1.7) * 0.22,
        1,
    );

    for i in 0..16 {
        let fi = i as f32;
        let angle = t * 0.9 + fi * tau / 16.0;
        let pulse = wave01(t * 2.2 + fi * 0.73);
        let cx = center_x + angle.cos() * (min_dim * 0.30 + pulse * 24.0);
        let cy = center_y + angle.sin() * (min_dim * 0.20 + pulse * 18.0);
        let radius = 8.0 + pulse * 18.0;
        write_cmd_fill_ellipse(
            buf,
            pos,
            cx,
            cy,
            radius,
            radius,
            0.95 - pulse * 0.30,
            0.35 + pulse * 0.55,
            0.15 + fi / 20.0,
            0.78,
        );
    }

    let diamond_r = min_dim * 0.09;
    let mut points = [(0.0_f32, 0.0_f32); 4];
    for (i, point) in points.iter_mut().enumerate() {
        let angle = t * 1.15 + i as f32 * tau / 4.0;
        *point = (
            center_x + angle.cos() * diamond_r,
            center_y + angle.sin() * diamond_r,
        );
    }
    for i in 0..4 {
        let (x0, y0) = points[i];
        let (x1, y1) = points[(i + 1) % 4];
        write_cmd_draw_line(buf, pos, x0, y0, x1, y1, 4.0, 1.0, 0.72, 0.18, 0.92);
    }
    write_cmd_stroke_rect(
        buf,
        pos,
        center_x - diamond_r,
        center_y - diamond_r,
        diamond_r * 2.0,
        diamond_r * 2.0,
        2.0,
        1.0,
        1.0,
        1.0,
        0.38,
    );

    let bar_count = 32usize;
    let bar_area_w = (cw * 0.72).clamp(480.0, 1200.0);
    let bar_w = bar_area_w / bar_count as f32 * 0.56;
    let gap = bar_area_w / bar_count as f32 * 0.44;
    let base_x = center_x - bar_area_w * 0.5;
    let base_y = (ch * 0.82).min(ch - 90.0).max(center_y + min_dim * 0.20);
    for i in 0..bar_count {
        let fi = i as f32;
        let level = wave01(t * 2.6 + fi * 0.38) * wave01(t * 0.7 - fi * 0.13);
        let h = 20.0 + level * min_dim * 0.12;
        let x = base_x + fi * (bar_w + gap);
        write_cmd_fill_rounded_rect(
            buf,
            pos,
            x,
            base_y - h,
            bar_w,
            h,
            bar_w * 0.45,
            bar_w * 0.45,
            0.20 + level * 0.55,
            0.85,
            0.35 + fi / bar_count as f32 * 0.50,
            0.70,
        );
    }

    write_cmd_draw_text(
        buf,
        pos,
        "Complex animation: geometry/text/PNG textures via IPC",
        panel_x,
        panel_y - 34.0,
        18.0,
        0.85,
        0.92,
        1.0,
        0.92,
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Some(options) = parse_demo_options()? else {
        return Ok(());
    };

    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1) as u32;
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1) as u32;
    println!("[demo-app] 屏幕分辨率: {}x{}", screen_w, screen_h);
    if options.unlocked {
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

    send_control_message(
        &mut client,
        ControlMessage::RegisterApp {
            pid: std::process::id(),
        },
        &mut buf,
    )
    .await?;
    println!("[demo-app] 已注册 App");

    // 等 server 处理 RegisterApp 并创建共享内存
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    for (bitmap_id, name, bytes) in [
        (TEXTURE_ORB, "orb", ORB_PNG),
        (TEXTURE_GRID, "grid", GRID_PNG),
        (TEXTURE_STRIPES, "stripes", STRIPES_PNG),
    ] {
        send_control_message(
            &mut client,
            ControlMessage::LoadBitmap {
                bitmap_id,
                bytes: bytes.to_vec(),
            },
            &mut buf,
        )
        .await?;
        println!("[demo-app] 已上传贴图: {} (id={})", name, bitmap_id);
    }

    send_control_message(
        &mut client,
        ControlMessage::CreateCanvas {
            logical_w: screen_w,
            logical_h: screen_h,
            render_w: screen_w,
            render_h: screen_h,
        },
        &mut buf,
    )
    .await?;
    println!("[demo-app] 已创建画布 {}x{}", screen_w, screen_h);

    let list_request_id = 1;
    send_control_message(
        &mut client,
        ControlMessage::ListMonitorTypes {
            request_id: list_request_id,
        },
        &mut buf,
    )
    .await?;
    let monitor_entries = wait_monitor_types(&mut client, list_request_id).await?;
    print_monitor_catalog(&monitor_entries);

    if options.desktop_monitors > 0 {
        let start_request_id = 2;
        let flags = if options.click_through {
            DESKTOP_WINDOW_FLAG_CLICK_THROUGH
        } else {
            0
        };
        println!(
            "[demo-app] 请求启动 {} 个 Desktop monitor，mode={} click_through={}",
            options.desktop_monitors,
            window_mode_label(options.window_mode),
            options.click_through
        );
        send_control_message(
            &mut client,
            ControlMessage::StartMonitor {
                request_id: start_request_id,
                kind: MonitorKind::DesktopWindow,
                count: options.desktop_monitors,
                target_canvas_id: 0,
                mode: options.window_mode,
                flags,
                x: 100,
                y: 100,
                w: 720,
                h: 420,
            },
            &mut buf,
        )
        .await?;
        let (status, monitor_ids) =
            wait_start_monitor_result(&mut client, start_request_id).await?;
        if status != MonitorRequestStatus::Ok {
            anyhow::bail!("StartMonitor failed: {status:?}");
        }
        println!("[demo-app] Desktop monitor 已启动: {:?}", monitor_ids);
    } else {
        println!("[demo-app] 未请求 Desktop monitor；可手动打开 Game Bar widget 或使用 --desktop-monitors N");
    }

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
        unsafe { std::slice::from_raw_parts_mut(shmem_ptr.Value as *mut u8, 16 * 1024 * 1024) };
    println!("[demo-app] 已打开共享内存: {}", shmem_name);

    // 持续渲染循环
    println!("[demo-app] 开始渲染循环（DWM vsync，Ctrl+C 退出）...");
    let mut frame_id: u64 = 0;
    let cw = screen_w as f32;
    let ch = screen_h as f32;

    let mut last_fps_time = std::time::Instant::now();
    let mut fps_frame_count: u64 = 0;
    let mut current_fps: f32 = 0.0;
    let start_time = std::time::Instant::now();

    // Use a simple ring-buffer strategy for the offset to prevent data races
    // when running --unlocked.
    let mut current_offset: u32 = 24;
    let max_offset: u32 = 14 * 1024 * 1024;
    let frame_max_size: u32 = 64 * 1024;

    loop {
        frame_id += 1;
        fps_frame_count += 1;
        // Use real time for animation so it looks smooth regardless of framerate
        let t = start_time.elapsed().as_secs_f32() * 2.0;

        let now = std::time::Instant::now();
        let elapsed = now.duration_since(last_fps_time).as_secs_f32();
        if elapsed >= 1.0 {
            current_fps = fps_frame_count as f32 / elapsed;
            fps_frame_count = 0;
            last_fps_time = now;
        }

        let cmd_offset = current_offset;
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

        write_complex_animation_scene(shmem_bytes, &mut pos, cw, ch, t);

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

        let icon_size = 48.0 + wave01(t * 2.4) * 18.0;
        write_cmd_draw_bitmap(
            shmem_bytes,
            &mut pos,
            TEXTURE_ORB,
            (0.0, 0.0, 0.0, 0.0),
            (margin + 16.0, margin + 16.0, icon_size, icon_size),
            0.88,
            1,
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
        if cmd_length > frame_max_size {
            eprintln!("[demo-app] frame payload too large!");
        }

        // Advance ring buffer offset
        current_offset += frame_max_size;
        if current_offset >= max_offset {
            current_offset = 24;
        }

        send_control_message(
            &mut client,
            ControlMessage::SubmitFrame {
                canvas_id: 0,
                frame_id,
                offset: cmd_offset,
                length: cmd_length,
            },
            &mut buf,
        )
        .await?;

        if !options.unlocked {
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
