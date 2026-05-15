//! Command ringbuffer decoder.
//!
//! ## Opcode map
//!
//! The command-ringbuffer layer uses the `0x0100..` opcode range (distinct
//! from the control-plane `0x0001..` range in `protocol.rs`). Two opcode
//! sub-ranges are defined today:
//!
//! * **Geometry opcodes** `0x0101..=0x0108` — the 8 existing draw opcodes
//!   (`CLEAR`, `FILL_RECT`, `STROKE_RECT`, `FILL_ROUNDED_RECT`,
//!   `STROKE_ROUNDED_RECT`, `FILL_ELLIPSE`, `STROKE_ELLIPSE`, `DRAW_LINE`).
//!   Their on-the-wire byte layout is frozen — Preservation Requirement 3.6
//!   in `.kiro/specs/animation-and-viewport-fix/bugfix.md` says existing
//!   apps that never emit `PUSH_SPACE` must see identical decode
//!   behaviour after the fix.
//!
//! * **Text opcode** `0x010B` — `DRAW_TEXT`, payload
//!   `f32 x | f32 y | f32 font_size | f32 rgba[4] | u16 text_len | utf8[text_len]`.
//!   Text is a first-class command and participates in the same World /
//!   MonitorLocal space-stack routing as geometry.
//!
//! Misuse is tolerated, not fatal:
//! * `CMD_PUSH_SPACE` with an unknown `space_id` → warn log, skip (no
//!   `RenderCommand` emitted; subsequent commands in the frame are unaffected).
//! * `CMD_POP_SPACE` on an empty stack, `CMD_PUSH_SPACE`/`CMD_POP_SPACE`
//!   imbalance at frame end → the dispatcher warns and skips the offending
//!   op; already-dispatched commands in the frame are not invalidated.

use bytes::{Buf, BytesMut};

use crate::renderer::painter::DrawCmd;

const CMD_HEADER_SIZE: usize = 4; // opcode (u16) + payload_len (u16)

// --- Geometry opcodes (Preservation 3.6: byte layout frozen) ---
const CMD_CLEAR: u16 = 0x0101;
const CMD_FILL_RECT: u16 = 0x0102;
const CMD_STROKE_RECT: u16 = 0x0103;
const CMD_FILL_ROUNDED_RECT: u16 = 0x0104;
const CMD_STROKE_ROUNDED_RECT: u16 = 0x0105;
const CMD_FILL_ELLIPSE: u16 = 0x0106;
const CMD_STROKE_ELLIPSE: u16 = 0x0107;
const CMD_DRAW_LINE: u16 = 0x0108;

// --- Space-stack opcodes (task 3.2 / design.md §Fix Implementation Change 6) ---
/// Push a coordinate space on the per-`SubmitFrame` space stack.
/// Payload: `u32 space_id` (`0 = World`, `1 = MonitorLocal`).
pub const CMD_PUSH_SPACE: u16 = 0x0109;
/// Pop the top of the per-`SubmitFrame` space stack. Empty payload.
pub const CMD_POP_SPACE: u16 = 0x010A;

// --- Text / bitmap opcodes ---
const CMD_DRAW_TEXT: u16 = 0x010B;
const CMD_DRAW_BITMAP: u16 = 0x010C;

/// Minimum DRAW_TEXT payload without UTF-8 bytes:
/// f32 x/y/font_size + rgba[4] + u16 text_len.
const CMD_DRAW_TEXT_FIXED_PAYLOAD: usize = 30;

/// Numeric `space_id` values wire-encoded inside `CMD_PUSH_SPACE` payloads.
/// design.md §Fix Implementation → Change 6 fixes these values:
/// `0 = World`, `1 = MonitorLocal`. Adding a new space is a wire-protocol
/// change and must bump the allocator here deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceId {
    /// Global shared canvas coordinates. Default for any command that is
    /// not inside an explicit `PUSH_SPACE(MonitorLocal)..POP_SPACE` region.
    World = 0,
    /// Per-Monitor client-area coordinates; replayed independently onto
    /// each attached Monitor's `PerMonitorResources` surface.
    MonitorLocal = 1,
}

impl SpaceId {
    /// Decode a wire `u32 space_id`. Returns `None` for unknown values so
    /// the caller can warn and skip the offending `CMD_PUSH_SPACE` (per
    /// task 3.2 "on misuse: emit a warning log and skip the offending
    /// command").
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::World),
            1 => Some(Self::MonitorLocal),
            _ => None,
        }
    }
}

fn read_rgba(buf: &mut BytesMut) -> [f32; 4] {
    [
        buf.get_f32_le(),
        buf.get_f32_le(),
        buf.get_f32_le(),
        buf.get_f32_le(),
    ]
}

fn fixed_payload_len(opcode: u16) -> Option<usize> {
    match opcode {
        CMD_CLEAR => Some(16),
        CMD_FILL_RECT | CMD_FILL_ELLIPSE => Some(32),
        CMD_STROKE_RECT | CMD_STROKE_ELLIPSE => Some(36),
        CMD_FILL_ROUNDED_RECT | CMD_DRAW_LINE => Some(40),
        CMD_STROKE_ROUNDED_RECT | CMD_DRAW_BITMAP => Some(44),
        CMD_PUSH_SPACE => Some(4),
        CMD_POP_SPACE => Some(0),
        _ => None,
    }
}

pub fn decode_commands(data: &[u8]) -> Vec<RenderCommand> {
    let mut buf = BytesMut::from(data);
    let mut commands = Vec::new();

    while buf.remaining() >= CMD_HEADER_SIZE {
        let opcode = buf.get_u16_le();
        let payload_len = buf.get_u16_le() as usize;

        if buf.remaining() < payload_len {
            eprintln!(
                "[cmd_decoder] opcode {:#06x} truncated payload: expected {}, got {}",
                opcode,
                payload_len,
                buf.remaining()
            );
            break;
        }

        let mut payload = buf.split_to(payload_len);
        if let Some(expected) = fixed_payload_len(opcode) {
            if payload_len != expected {
                eprintln!(
                    "[cmd_decoder] opcode {:#06x} payload_len mismatch: expected {}, got {}; skipping",
                    opcode,
                    expected,
                    payload_len
                );
                continue;
            }
        }

        match opcode {
            CMD_CLEAR => {
                commands.push(RenderCommand::Clear(read_rgba(&mut payload)));
            }
            CMD_FILL_RECT => {
                let x = payload.get_f32_le();
                let y = payload.get_f32_le();
                let w = payload.get_f32_le();
                let h = payload.get_f32_le();
                let rgba = read_rgba(&mut payload);
                commands.push(RenderCommand::Draw(DrawCmd::FillRect { x, y, w, h, rgba }));
            }
            CMD_STROKE_RECT => {
                let x = payload.get_f32_le();
                let y = payload.get_f32_le();
                let w = payload.get_f32_le();
                let h = payload.get_f32_le();
                let stroke_width = payload.get_f32_le();
                let rgba = read_rgba(&mut payload);
                commands.push(RenderCommand::Draw(DrawCmd::StrokeRect {
                    x,
                    y,
                    w,
                    h,
                    stroke_width,
                    rgba,
                }));
            }
            CMD_FILL_ROUNDED_RECT => {
                let x = payload.get_f32_le();
                let y = payload.get_f32_le();
                let w = payload.get_f32_le();
                let h = payload.get_f32_le();
                let radius_x = payload.get_f32_le();
                let radius_y = payload.get_f32_le();
                let rgba = read_rgba(&mut payload);
                commands.push(RenderCommand::Draw(DrawCmd::FillRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius_x,
                    radius_y,
                    rgba,
                }));
            }
            CMD_STROKE_ROUNDED_RECT => {
                let x = payload.get_f32_le();
                let y = payload.get_f32_le();
                let w = payload.get_f32_le();
                let h = payload.get_f32_le();
                let radius_x = payload.get_f32_le();
                let radius_y = payload.get_f32_le();
                let stroke_width = payload.get_f32_le();
                let rgba = read_rgba(&mut payload);
                commands.push(RenderCommand::Draw(DrawCmd::StrokeRoundedRect {
                    x,
                    y,
                    w,
                    h,
                    radius_x,
                    radius_y,
                    stroke_width,
                    rgba,
                }));
            }
            CMD_FILL_ELLIPSE => {
                let cx = payload.get_f32_le();
                let cy = payload.get_f32_le();
                let rx = payload.get_f32_le();
                let ry = payload.get_f32_le();
                let rgba = read_rgba(&mut payload);
                commands.push(RenderCommand::Draw(DrawCmd::FillEllipse {
                    cx,
                    cy,
                    rx,
                    ry,
                    rgba,
                }));
            }
            CMD_STROKE_ELLIPSE => {
                let cx = payload.get_f32_le();
                let cy = payload.get_f32_le();
                let rx = payload.get_f32_le();
                let ry = payload.get_f32_le();
                let stroke_width = payload.get_f32_le();
                let rgba = read_rgba(&mut payload);
                commands.push(RenderCommand::Draw(DrawCmd::StrokeEllipse {
                    cx,
                    cy,
                    rx,
                    ry,
                    stroke_width,
                    rgba,
                }));
            }
            CMD_DRAW_LINE => {
                let x0 = payload.get_f32_le();
                let y0 = payload.get_f32_le();
                let x1 = payload.get_f32_le();
                let y1 = payload.get_f32_le();
                let stroke_width = payload.get_f32_le();
                let rgba = read_rgba(&mut payload);
                let dash_style = payload.get_i32_le();
                commands.push(RenderCommand::Draw(DrawCmd::DrawLine {
                    x0,
                    y0,
                    x1,
                    y1,
                    stroke_width,
                    rgba,
                    dash_style,
                }));
            }
            CMD_DRAW_BITMAP => {
                let bitmap_id = payload.get_u32_le();
                let src_x = payload.get_f32_le();
                let src_y = payload.get_f32_le();
                let src_w = payload.get_f32_le();
                let src_h = payload.get_f32_le();
                let dst_x = payload.get_f32_le();
                let dst_y = payload.get_f32_le();
                let dst_w = payload.get_f32_le();
                let dst_h = payload.get_f32_le();
                let opacity = payload.get_f32_le();
                let interp_mode = payload.get_i32_le();
                commands.push(RenderCommand::DrawBitmap(BitmapDrawCommand {
                    bitmap_id,
                    src_x,
                    src_y,
                    src_w,
                    src_h,
                    dst_x,
                    dst_y,
                    dst_w,
                    dst_h,
                    opacity,
                    interp_mode,
                }));
            }
            CMD_DRAW_TEXT => {
                if payload_len < CMD_DRAW_TEXT_FIXED_PAYLOAD {
                    eprintln!(
                        "[cmd_decoder] CMD_DRAW_TEXT payload too short: {} < {}; skipping",
                        payload_len, CMD_DRAW_TEXT_FIXED_PAYLOAD
                    );
                    continue;
                }

                let x = payload.get_f32_le();
                let y = payload.get_f32_le();
                let font_size = payload.get_f32_le();
                let rgba = read_rgba(&mut payload);
                let text_len = payload.get_u16_le() as usize;
                let expected_payload_len = CMD_DRAW_TEXT_FIXED_PAYLOAD + text_len;
                if expected_payload_len != payload_len {
                    eprintln!(
                        "[cmd_decoder] CMD_DRAW_TEXT payload_len mismatch: expected {}, got {}; skipping",
                        expected_payload_len,
                        payload_len
                    );
                    continue;
                }

                let text_bytes = payload.copy_to_bytes(text_len);
                match std::str::from_utf8(&text_bytes) {
                    Ok(text) => commands.push(RenderCommand::Draw(DrawCmd::DrawText {
                        text: text.to_owned(),
                        x,
                        y,
                        font_size,
                        rgba,
                    })),
                    Err(e) => {
                        eprintln!("[cmd_decoder] CMD_DRAW_TEXT invalid UTF-8: {}", e);
                    }
                }
            }
            CMD_PUSH_SPACE => {
                let space_id_raw = payload.get_u32_le();
                match SpaceId::from_u32(space_id_raw) {
                    Some(space) => commands.push(RenderCommand::PushSpace(space)),
                    None => {
                        eprintln!(
                            "[cmd_decoder] CMD_PUSH_SPACE with unknown space_id={}; \
                             skipping (per task 3.2 misuse policy)",
                            space_id_raw
                        );
                    }
                }
            }
            CMD_POP_SPACE => {
                commands.push(RenderCommand::PopSpace);
            }
            _ => {
                eprintln!(
                    "[cmd_decoder] unknown opcode: {:#06x}; skipping {} payload bytes",
                    opcode, payload_len
                );
            }
        }
    }
    commands
}

#[derive(Debug, Clone)]
pub struct BitmapDrawCommand {
    pub bitmap_id: u32,
    pub src_x: f32,
    pub src_y: f32,
    pub src_w: f32,
    pub src_h: f32,
    pub dst_x: f32,
    pub dst_y: f32,
    pub dst_w: f32,
    pub dst_h: f32,
    pub opacity: f32,
    pub interp_mode: i32,
}

#[derive(Debug, Clone)]
pub enum RenderCommand {
    /// `CMD_CLEAR` — clear the current render target to an RGBA color.
    Clear([f32; 4]),
    /// Any of the 8 geometry `DrawCmd` variants produced by the
    /// `0x0101..=0x0108` opcodes.
    Draw(DrawCmd),
    DrawBitmap(BitmapDrawCommand),
    /// `CMD_PUSH_SPACE` — push a coordinate space on the per-`SubmitFrame`
    /// space stack. The dispatcher (`server_task.rs`, task 3.4) maintains
    /// the stack; subsequent geometry commands render into the target
    /// selected by the top-of-stack space.
    PushSpace(SpaceId),
    /// `CMD_POP_SPACE` — pop the top of the per-`SubmitFrame` space stack.
    /// Underflow is handled by the dispatcher (warn + skip).
    PopSpace,
}

#[cfg(test)]
mod tests {
    //! Unit tests for the decoder. These cover:
    //!
    //! * **Preservation 3.6** — all 8 existing geometry opcodes decode
    //!   unchanged when no PUSH_SPACE is present. (The test suite under
    //!   `core-server/tests/preservation.rs` covers this more
    //!   exhaustively; these co-located tests are a fast local guard.)
    //! * **Task 3.2 new opcodes** — `CMD_PUSH_SPACE` (both valid
    //!   `space_id` and unknown `space_id` misuse) and `CMD_POP_SPACE`.
    //! * **Mixed streams** — PUSH/POP interleaved with geometry commands.

    use super::*;

    fn push_u16(buf: &mut Vec<u8>, v: u16) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn push_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn push_f32(buf: &mut Vec<u8>, v: f32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn emit_fill_rect(buf: &mut Vec<u8>, x: f32, y: f32, w: f32, h: f32, rgba: [f32; 4]) {
        push_u16(buf, CMD_FILL_RECT);
        push_u16(buf, 32);
        push_f32(buf, x);
        push_f32(buf, y);
        push_f32(buf, w);
        push_f32(buf, h);
        for c in &rgba {
            push_f32(buf, *c);
        }
    }

    fn emit_clear(buf: &mut Vec<u8>, rgba: [f32; 4]) {
        push_u16(buf, CMD_CLEAR);
        push_u16(buf, 16);
        for c in &rgba {
            push_f32(buf, *c);
        }
    }

    fn emit_push_space(buf: &mut Vec<u8>, space_id: u32) {
        push_u16(buf, CMD_PUSH_SPACE);
        push_u16(buf, 4);
        push_u32(buf, space_id);
    }

    fn emit_pop_space(buf: &mut Vec<u8>) {
        push_u16(buf, CMD_POP_SPACE);
        push_u16(buf, 0);
    }

    fn emit_draw_text(
        buf: &mut Vec<u8>,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        rgba: [f32; 4],
    ) {
        let text_bytes = text.as_bytes();
        push_u16(buf, CMD_DRAW_TEXT);
        push_u16(buf, (CMD_DRAW_TEXT_FIXED_PAYLOAD + text_bytes.len()) as u16);
        push_f32(buf, x);
        push_f32(buf, y);
        push_f32(buf, font_size);
        for c in &rgba {
            push_f32(buf, *c);
        }
        push_u16(buf, text_bytes.len() as u16);
        buf.extend_from_slice(text_bytes);
    }

    fn emit_draw_bitmap(buf: &mut Vec<u8>) {
        push_u16(buf, CMD_DRAW_BITMAP);
        push_u16(buf, 44);
        push_u32(buf, 7);
        for v in [1.0, 2.0, 32.0, 48.0, 10.0, 20.0, 64.0, 96.0, 0.75] {
            push_f32(buf, v);
        }
        buf.extend_from_slice(&1i32.to_le_bytes());
    }

    #[test]
    fn draw_text_decodes_to_drawcmd_text() {
        let mut buf = Vec::new();
        emit_draw_text(&mut buf, "144 FPS", 16.0, 11.0, 20.0, [0.2, 0.9, 0.2, 1.0]);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            RenderCommand::Draw(DrawCmd::DrawText {
                text,
                x,
                y,
                font_size,
                rgba,
            }) => {
                assert_eq!(text, "144 FPS");
                assert_eq!((*x, *y, *font_size), (16.0, 11.0, 20.0));
                assert_eq!(*rgba, [0.2, 0.9, 0.2, 1.0]);
            }
            other => panic!("expected DrawText, got {:?}", other),
        }
    }

    #[test]
    fn draw_bitmap_decodes_to_bitmap_command() {
        let mut buf = Vec::new();
        emit_draw_bitmap(&mut buf);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            RenderCommand::DrawBitmap(draw) => {
                assert_eq!(draw.bitmap_id, 7);
                assert_eq!(
                    (draw.src_x, draw.src_y, draw.src_w, draw.src_h),
                    (1.0, 2.0, 32.0, 48.0)
                );
                assert_eq!(
                    (draw.dst_x, draw.dst_y, draw.dst_w, draw.dst_h),
                    (10.0, 20.0, 64.0, 96.0)
                );
                assert_eq!(draw.opacity, 0.75);
                assert_eq!(draw.interp_mode, 1);
            }
            other => panic!("expected DrawBitmap, got {:?}", other),
        }
    }

    #[test]
    fn push_space_world_decodes_to_pushspace_world() {
        let mut buf = Vec::new();
        emit_push_space(&mut buf, 0);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            RenderCommand::PushSpace(SpaceId::World) => {}
            other => panic!("expected PushSpace(World), got {:?}", other),
        }
    }

    #[test]
    fn push_space_monitor_local_decodes_to_pushspace_monitor_local() {
        let mut buf = Vec::new();
        emit_push_space(&mut buf, 1);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            RenderCommand::PushSpace(SpaceId::MonitorLocal) => {}
            other => panic!("expected PushSpace(MonitorLocal), got {:?}", other),
        }
    }

    #[test]
    fn pop_space_decodes_to_popspace() {
        let mut buf = Vec::new();
        emit_pop_space(&mut buf);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            RenderCommand::PopSpace => {}
            other => panic!("expected PopSpace, got {:?}", other),
        }
    }

    #[test]
    fn push_space_with_unknown_space_id_is_skipped_not_fatal() {
        // Misuse: `space_id = 42` is not in {0, 1}. Per task 3.2 the
        // decoder must warn and skip the offending command; subsequent
        // geometry commands must still decode.
        let mut buf = Vec::new();
        emit_push_space(&mut buf, 42);
        emit_fill_rect(&mut buf, 10.0, 10.0, 20.0, 4.0, [0.0, 1.0, 0.0, 1.0]);

        let cmds = decode_commands(&buf);
        // PUSH is skipped (no RenderCommand emitted), FILL_RECT is preserved.
        assert_eq!(
            cmds.len(),
            1,
            "unknown space_id should be skipped, not fatal"
        );
        match &cmds[0] {
            RenderCommand::Draw(DrawCmd::FillRect { x, y, w, h, rgba }) => {
                assert_eq!((*x, *y, *w, *h), (10.0, 10.0, 20.0, 4.0));
                assert_eq!(*rgba, [0.0, 1.0, 0.0, 1.0]);
            }
            other => panic!("expected FillRect, got {:?}", other),
        }
    }

    #[test]
    fn mixed_push_fill_pop_stream_decodes_in_order() {
        // The canonical sub-property 1b counterexample stream:
        //   PUSH_SPACE(MonitorLocal) / FILL_RECT(10,10,20,4,green) / POP_SPACE
        // After task 3.2, this decodes to 3 RenderCommands in order —
        // confirming the decoder half of the fix.
        let mut buf = Vec::new();
        emit_push_space(&mut buf, 1);
        emit_fill_rect(&mut buf, 10.0, 10.0, 20.0, 4.0, [0.0, 1.0, 0.0, 1.0]);
        emit_pop_space(&mut buf);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 3);
        assert!(matches!(
            cmds[0],
            RenderCommand::PushSpace(SpaceId::MonitorLocal)
        ));
        assert!(matches!(
            cmds[1],
            RenderCommand::Draw(DrawCmd::FillRect { .. })
        ));
        assert!(matches!(cmds[2], RenderCommand::PopSpace));
    }

    #[test]
    fn stream_without_push_or_pop_decodes_exactly_as_before() {
        // Preservation 3.6: streams that never emit PUSH_SPACE must decode
        // identically to the pre-fix implementation. We serialize a
        // FILL_RECT + a CLEAR and assert the decoder returns exactly those
        // two RenderCommand variants in order with no extra output.
        let mut buf = Vec::new();
        emit_fill_rect(&mut buf, 0.0, 0.0, 5.0, 5.0, [1.0, 0.0, 0.0, 1.0]);
        push_u16(&mut buf, CMD_CLEAR);
        push_u16(&mut buf, 16);
        for c in &[0.5f32, 0.5, 0.5, 1.0] {
            push_f32(&mut buf, *c);
        }

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 2);
        assert!(matches!(
            cmds[0],
            RenderCommand::Draw(DrawCmd::FillRect { .. })
        ));
        assert!(matches!(cmds[1], RenderCommand::Clear(_)));
    }

    #[test]
    fn malformed_fixed_payload_short_skips_and_preserves_following_command() {
        let mut buf = Vec::new();
        push_u16(&mut buf, CMD_FILL_RECT);
        push_u16(&mut buf, 28);
        buf.extend_from_slice(&[0u8; 28]);
        emit_clear(&mut buf, [0.1, 0.2, 0.3, 0.4]);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], RenderCommand::Clear(_)));
    }

    #[test]
    fn malformed_fixed_payload_long_skips_and_preserves_following_command() {
        let mut buf = Vec::new();
        push_u16(&mut buf, CMD_FILL_RECT);
        push_u16(&mut buf, 36);
        buf.extend_from_slice(&[0u8; 36]);
        emit_clear(&mut buf, [0.1, 0.2, 0.3, 0.4]);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], RenderCommand::Clear(_)));
    }

    #[test]
    fn push_and_pop_reject_wrong_payload_lengths_without_desync() {
        let mut buf = Vec::new();
        push_u16(&mut buf, CMD_PUSH_SPACE);
        push_u16(&mut buf, 0);
        push_u16(&mut buf, CMD_POP_SPACE);
        push_u16(&mut buf, 4);
        buf.extend_from_slice(&[0u8; 4]);
        emit_clear(&mut buf, [0.1, 0.2, 0.3, 0.4]);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], RenderCommand::Clear(_)));
    }

    #[test]
    fn draw_text_length_mismatch_skips_and_preserves_following_command() {
        let mut buf = Vec::new();
        push_u16(&mut buf, CMD_DRAW_TEXT);
        push_u16(&mut buf, 31);
        push_f32(&mut buf, 1.0);
        push_f32(&mut buf, 2.0);
        push_f32(&mut buf, 12.0);
        for c in &[1.0, 1.0, 1.0, 1.0] {
            push_f32(&mut buf, *c);
        }
        push_u16(&mut buf, 2);
        buf.push(b'a');
        emit_clear(&mut buf, [0.1, 0.2, 0.3, 0.4]);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], RenderCommand::Clear(_)));
    }

    #[test]
    fn unknown_opcode_skips_payload_and_preserves_following_command() {
        let mut buf = Vec::new();
        push_u16(&mut buf, 0x9999);
        push_u16(&mut buf, 3);
        buf.extend_from_slice(&[1, 2, 3]);
        emit_clear(&mut buf, [0.1, 0.2, 0.3, 0.4]);

        let cmds = decode_commands(&buf);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], RenderCommand::Clear(_)));
    }

    #[test]
    fn space_id_from_u32_round_trip() {
        assert_eq!(SpaceId::from_u32(0), Some(SpaceId::World));
        assert_eq!(SpaceId::from_u32(1), Some(SpaceId::MonitorLocal));
        assert_eq!(SpaceId::from_u32(2), None);
        assert_eq!(SpaceId::from_u32(u32::MAX), None);
    }
}
