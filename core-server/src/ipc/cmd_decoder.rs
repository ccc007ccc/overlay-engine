use bytes::{Buf, BytesMut};

use crate::renderer::painter::DrawCmd;

const CMD_HEADER_SIZE: usize = 4; // opcode (u16) + payload_len (u16)

const CMD_CLEAR: u16 = 0x0101;
const CMD_FILL_RECT: u16 = 0x0102;
const CMD_STROKE_RECT: u16 = 0x0103;
const CMD_FILL_ROUNDED_RECT: u16 = 0x0104;
const CMD_STROKE_ROUNDED_RECT: u16 = 0x0105;
const CMD_FILL_ELLIPSE: u16 = 0x0106;
const CMD_STROKE_ELLIPSE: u16 = 0x0107;
const CMD_DRAW_LINE: u16 = 0x0108;

fn read_rgba(buf: &mut BytesMut) -> [f32; 4] {
    [buf.get_f32_le(), buf.get_f32_le(), buf.get_f32_le(), buf.get_f32_le()]
}

pub fn decode_commands(data: &[u8]) -> Vec<RenderCommand> {
    let mut buf = BytesMut::from(data);
    let mut commands = Vec::new();

    while buf.remaining() >= CMD_HEADER_SIZE {
        let opcode = buf.get_u16_le();
        let _payload_len = buf.get_u16_le();

        let cmd = match opcode {
            CMD_CLEAR => {
                if buf.remaining() < 16 { break; }
                RenderCommand::Clear(read_rgba(&mut buf))
            }
            CMD_FILL_RECT => {
                if buf.remaining() < 32 { break; }
                let x = buf.get_f32_le();
                let y = buf.get_f32_le();
                let w = buf.get_f32_le();
                let h = buf.get_f32_le();
                let rgba = read_rgba(&mut buf);
                RenderCommand::Draw(DrawCmd::FillRect { x, y, w, h, rgba })
            }
            CMD_STROKE_RECT => {
                if buf.remaining() < 36 { break; }
                let x = buf.get_f32_le();
                let y = buf.get_f32_le();
                let w = buf.get_f32_le();
                let h = buf.get_f32_le();
                let stroke_width = buf.get_f32_le();
                let rgba = read_rgba(&mut buf);
                RenderCommand::Draw(DrawCmd::StrokeRect { x, y, w, h, stroke_width, rgba })
            }
            CMD_FILL_ROUNDED_RECT => {
                if buf.remaining() < 40 { break; }
                let x = buf.get_f32_le();
                let y = buf.get_f32_le();
                let w = buf.get_f32_le();
                let h = buf.get_f32_le();
                let radius_x = buf.get_f32_le();
                let radius_y = buf.get_f32_le();
                let rgba = read_rgba(&mut buf);
                RenderCommand::Draw(DrawCmd::FillRoundedRect { x, y, w, h, radius_x, radius_y, rgba })
            }
            CMD_STROKE_ROUNDED_RECT => {
                if buf.remaining() < 44 { break; }
                let x = buf.get_f32_le();
                let y = buf.get_f32_le();
                let w = buf.get_f32_le();
                let h = buf.get_f32_le();
                let radius_x = buf.get_f32_le();
                let radius_y = buf.get_f32_le();
                let stroke_width = buf.get_f32_le();
                let rgba = read_rgba(&mut buf);
                RenderCommand::Draw(DrawCmd::StrokeRoundedRect { x, y, w, h, radius_x, radius_y, stroke_width, rgba })
            }
            CMD_FILL_ELLIPSE => {
                if buf.remaining() < 32 { break; }
                let cx = buf.get_f32_le();
                let cy = buf.get_f32_le();
                let rx = buf.get_f32_le();
                let ry = buf.get_f32_le();
                let rgba = read_rgba(&mut buf);
                RenderCommand::Draw(DrawCmd::FillEllipse { cx, cy, rx, ry, rgba })
            }
            CMD_STROKE_ELLIPSE => {
                if buf.remaining() < 36 { break; }
                let cx = buf.get_f32_le();
                let cy = buf.get_f32_le();
                let rx = buf.get_f32_le();
                let ry = buf.get_f32_le();
                let stroke_width = buf.get_f32_le();
                let rgba = read_rgba(&mut buf);
                RenderCommand::Draw(DrawCmd::StrokeEllipse { cx, cy, rx, ry, stroke_width, rgba })
            }
            CMD_DRAW_LINE => {
                if buf.remaining() < 40 { break; }
                let x0 = buf.get_f32_le();
                let y0 = buf.get_f32_le();
                let x1 = buf.get_f32_le();
                let y1 = buf.get_f32_le();
                let stroke_width = buf.get_f32_le();
                let rgba = read_rgba(&mut buf);
                let dash_style = buf.get_i32_le();
                RenderCommand::Draw(DrawCmd::DrawLine { x0, y0, x1, y1, stroke_width, rgba, dash_style })
            }
            _ => {
                eprintln!("[cmd_decoder] unknown opcode: {:#06x}", opcode);
                break;
            }
        };
        commands.push(cmd);
    }
    commands
}

#[derive(Debug, Clone)]
pub enum RenderCommand {
    Clear([f32; 4]),
    Draw(DrawCmd),
}
