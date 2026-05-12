use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("Buffer too small: expected {expected}, got {actual}")]
    BufferTooSmall { expected: usize, actual: usize },
    #[error("Invalid magic number: {0:#010x}")]
    InvalidMagic(u32),
    #[error("Unsupported version: {0}")]
    UnsupportedVersion(u16),
    #[error("Unknown opcode: {0:#06x}")]
    UnknownOpcode(u16),
    #[error("Payload length mismatch")]
    PayloadLengthMismatch,
}

pub const MAGIC: u32 = 0x4F56524C; // 'OVRL'
pub const VERSION: u16 = 1;

pub const HEADER_SIZE: usize = 12; // 4 (magic) + 2 (version) + 2 (opcode) + 4 (payload_len)

#[derive(Debug, Clone, PartialEq)]
pub struct MessageHeader {
    pub opcode: u16,
    pub payload_len: u32,
}

impl MessageHeader {
    pub fn decode(buf: &mut BytesMut) -> Result<Self, ProtocolError> {
        if buf.remaining() < HEADER_SIZE {
            return Err(ProtocolError::BufferTooSmall {
                expected: HEADER_SIZE,
                actual: buf.remaining(),
            });
        }

        let magic = buf.get_u32_le();
        if magic != MAGIC {
            return Err(ProtocolError::InvalidMagic(magic));
        }

        let version = buf.get_u16_le();
        if version != VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }

        let opcode = buf.get_u16_le();
        let payload_len = buf.get_u32_le();

        Ok(Self {
            opcode,
            payload_len,
        })
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u32_le(MAGIC);
        buf.put_u16_le(VERSION);
        buf.put_u16_le(self.opcode);
        buf.put_u32_le(self.payload_len);
    }
}

// Opcodes
pub const OP_REGISTER_APP: u16 = 0x0001;
pub const OP_REGISTER_MONITOR: u16 = 0x0002;
pub const OP_CREATE_CANVAS: u16 = 0x0003;
pub const OP_ATTACH_MONITOR: u16 = 0x0004;
pub const OP_CANVAS_ATTACHED: u16 = 0x0005;
pub const OP_SUBMIT_FRAME: u16 = 0x0006;
/// `MonitorLocalSurfaceAttached` — task 3.3 of the
/// `animation-and-viewport-fix` spec.
///
/// Sent by Core immediately **after** `CanvasAttached`. Carries the NT
/// handle of the per-Monitor MonitorLocal DComp surface so the Monitor
/// can mount a second visual for MonitorLocal-space pixels without
/// disturbing the existing World visual.
///
/// The on-the-wire layout of the pre-existing 6 variants
/// (`0x0001..=0x0006`) is **unchanged** — Preservation 3.1. Older
/// Monitors that do not recognize this opcode skip it via the
/// `decode` unknown-opcode downgrade (warn, not error).
pub const OP_MONITOR_LOCAL_SURFACE_ATTACHED: u16 = 0x0007;
pub const OP_APP_DETACHED: u16 = 0x0008;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppDetachReason {
    GracefulExit = 0,
    IoError = 1,
    Other = 2,
}


#[derive(Debug, Clone)]
pub enum ControlMessage {
    RegisterApp { pid: u32 },
    RegisterMonitor { pid: u32 },
    CreateCanvas {
        logical_w: u32,
        logical_h: u32,
        render_w: u32,
        render_h: u32,
    },
    AttachMonitor {
        canvas_id: u32,
        monitor_id: u32,
    },
    CanvasAttached {
        canvas_id: u32,
        surface_handle: u64,
        logical_w: u32,
        logical_h: u32,
        render_w: u32,
        render_h: u32,
    },
    SubmitFrame {
        canvas_id: u32,
        frame_id: u64,
        offset: u32,
        length: u32,
    },
    /// Per-Monitor MonitorLocal surface handoff. Added by task 3.3 under
    /// scheme α (see design.md §Fix Implementation → Change 5): the
    /// `CanvasAttached` layout is preserved bit-for-bit, and this new
    /// message is emitted immediately after it for every monitor that
    /// has a `PerMonitorResources` surface.
    ///
    /// Older monitors that do not recognize `OP_MONITOR_LOCAL_SURFACE_ATTACHED`
    /// skip it via the decoder's unknown-opcode downgrade path (warn,
    /// not error). This preserves Preservation 3.2 / 3.3 (desktop-window
    /// and Game Bar widget attach paths still work).
    MonitorLocalSurfaceAttached {
        canvas_id: u32,
        monitor_id: u32,
        surface_handle: u64,
        logical_w: u32,
        logical_h: u32,
    },
    AppDetached {
        app_id: u32,
        reason: u8,
    },
}

impl ControlMessage {
    pub fn opcode(&self) -> u16 {
        match self {
            Self::RegisterApp { .. } => OP_REGISTER_APP,
            Self::RegisterMonitor { .. } => OP_REGISTER_MONITOR,
            Self::CreateCanvas { .. } => OP_CREATE_CANVAS,
            Self::AttachMonitor { .. } => OP_ATTACH_MONITOR,
            Self::CanvasAttached { .. } => OP_CANVAS_ATTACHED,
            Self::SubmitFrame { .. } => OP_SUBMIT_FRAME,
            Self::MonitorLocalSurfaceAttached { .. } => OP_MONITOR_LOCAL_SURFACE_ATTACHED,
            Self::AppDetached { .. } => OP_APP_DETACHED,
        }
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        match self {
            Self::RegisterApp { pid } | Self::RegisterMonitor { pid } => {
                let header = MessageHeader {
                    opcode: self.opcode(),
                    payload_len: 4,
                };
                header.encode(buf);
                buf.put_u32_le(*pid);
            }
            Self::CreateCanvas {
                logical_w,
                logical_h,
                render_w,
                render_h,
            } => {
                let header = MessageHeader {
                    opcode: self.opcode(),
                    payload_len: 16,
                };
                header.encode(buf);
                buf.put_u32_le(*logical_w);
                buf.put_u32_le(*logical_h);
                buf.put_u32_le(*render_w);
                buf.put_u32_le(*render_h);
            }
            Self::AttachMonitor {
                canvas_id,
                monitor_id,
            } => {
                let header = MessageHeader {
                    opcode: self.opcode(),
                    payload_len: 8,
                };
                header.encode(buf);
                buf.put_u32_le(*canvas_id);
                buf.put_u32_le(*monitor_id);
            }
            Self::CanvasAttached {
                canvas_id,
                surface_handle,
                logical_w,
                logical_h,
                render_w,
                render_h,
            } => {
                let header = MessageHeader {
                    opcode: self.opcode(),
                    payload_len: 28,
                };
                header.encode(buf);
                buf.put_u32_le(*canvas_id);
                buf.put_u64_le(*surface_handle);
                buf.put_u32_le(*logical_w);
                buf.put_u32_le(*logical_h);
                buf.put_u32_le(*render_w);
                buf.put_u32_le(*render_h);
            }
            Self::SubmitFrame {
                canvas_id,
                frame_id,
                offset,
                length,
            } => {
                let header = MessageHeader {
                    opcode: self.opcode(),
                    payload_len: 20,
                };
                header.encode(buf);
                buf.put_u32_le(*canvas_id);
                buf.put_u64_le(*frame_id);
                buf.put_u32_le(*offset);
                buf.put_u32_le(*length);
            }
            Self::MonitorLocalSurfaceAttached {
                canvas_id,
                monitor_id,
                surface_handle,
                logical_w,
                logical_h,
            } => {
                // Payload layout (24 bytes):
                //   u32 canvas_id
                //   u32 monitor_id
                //   u64 surface_handle
                //   u32 logical_w
                //   u32 logical_h
                let header = MessageHeader {
                    opcode: self.opcode(),
                    payload_len: 24,
                };
                header.encode(buf);
                buf.put_u32_le(*canvas_id);
                buf.put_u32_le(*monitor_id);
                buf.put_u64_le(*surface_handle);
                buf.put_u32_le(*logical_w);
                buf.put_u32_le(*logical_h);
            }
            Self::AppDetached { app_id, reason } => {
                let header = MessageHeader {
                    opcode: self.opcode(),
                    payload_len: 5,
                };
                header.encode(buf);
                buf.put_u32_le(*app_id);
                buf.put_u8(*reason);
            }
        }
    }

    /// Decode a control-plane message from `buf`, given the already-parsed
    /// `opcode` and `payload_len` from a `MessageHeader`.
    ///
    /// Returns:
    /// * `Ok(Some(msg))` — known opcode, decoded successfully.
    /// * `Ok(None)` — **unknown opcode**. The `payload_len` bytes have been
    ///   skipped from `buf` and a warning is logged. Task 3.3 of the
    ///   `animation-and-viewport-fix` spec downgraded unknown opcodes from
    ///   error to warning so that older monitors (which do not recognize
    ///   `OP_MONITOR_LOCAL_SURFACE_ATTACHED` / future opcodes) can ignore
    ///   the message instead of tearing down the IPC connection. This
    ///   preserves Preservation 3.2 / 3.3.
    /// * `Err(ProtocolError)` — malformed frame (payload shorter than
    ///   advertised, known opcode with wrong byte count, etc.) — these
    ///   remain fatal.
    pub fn decode(
        opcode: u16,
        payload_len: u32,
        buf: &mut BytesMut,
    ) -> Result<Option<Self>, ProtocolError> {
        match opcode {
            OP_REGISTER_APP => {
                if buf.remaining() < 4 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 4,
                        actual: buf.remaining(),
                    });
                }
                Ok(Some(Self::RegisterApp {
                    pid: buf.get_u32_le(),
                }))
            }
            OP_REGISTER_MONITOR => {
                if buf.remaining() < 4 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 4,
                        actual: buf.remaining(),
                    });
                }
                Ok(Some(Self::RegisterMonitor {
                    pid: buf.get_u32_le(),
                }))
            }
            OP_CREATE_CANVAS => {
                if buf.remaining() < 16 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 16,
                        actual: buf.remaining(),
                    });
                }
                Ok(Some(Self::CreateCanvas {
                    logical_w: buf.get_u32_le(),
                    logical_h: buf.get_u32_le(),
                    render_w: buf.get_u32_le(),
                    render_h: buf.get_u32_le(),
                }))
            }
            OP_ATTACH_MONITOR => {
                if buf.remaining() < 8 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 8,
                        actual: buf.remaining(),
                    });
                }
                Ok(Some(Self::AttachMonitor {
                    canvas_id: buf.get_u32_le(),
                    monitor_id: buf.get_u32_le(),
                }))
            }
            OP_CANVAS_ATTACHED => {
                if buf.remaining() < 28 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 28,
                        actual: buf.remaining(),
                    });
                }
                Ok(Some(Self::CanvasAttached {
                    canvas_id: buf.get_u32_le(),
                    surface_handle: buf.get_u64_le(),
                    logical_w: buf.get_u32_le(),
                    logical_h: buf.get_u32_le(),
                    render_w: buf.get_u32_le(),
                    render_h: buf.get_u32_le(),
                }))
            }
            OP_SUBMIT_FRAME => {
                if buf.remaining() < 20 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 20,
                        actual: buf.remaining(),
                    });
                }
                Ok(Some(Self::SubmitFrame {
                    canvas_id: buf.get_u32_le(),
                    frame_id: buf.get_u64_le(),
                    offset: buf.get_u32_le(),
                    length: buf.get_u32_le(),
                }))
            }
            OP_MONITOR_LOCAL_SURFACE_ATTACHED => {
                if buf.remaining() < 24 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 24,
                        actual: buf.remaining(),
                    });
                }
                Ok(Some(Self::MonitorLocalSurfaceAttached {
                    canvas_id: buf.get_u32_le(),
                    monitor_id: buf.get_u32_le(),
                    surface_handle: buf.get_u64_le(),
                    logical_w: buf.get_u32_le(),
                    logical_h: buf.get_u32_le(),
                }))
            }
            OP_APP_DETACHED => {
                if buf.remaining() < 5 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 5,
                        actual: buf.remaining(),
                    });
                }
                Ok(Some(Self::AppDetached {
                    app_id: buf.get_u32_le(),
                    reason: buf.get_u8(),
                }))
            }
            _ => {
                // Task 3.3: downgrade unknown opcodes from error to warning.
                // We must consume the advertised payload so the reader stays
                // frame-aligned (next call to `MessageHeader::decode` begins
                // on the next message boundary).
                let skip = (payload_len as usize).min(buf.remaining());
                buf.advance(skip);
                eprintln!(
                    "[protocol] unknown opcode {:#06x} — skipping {} payload \
                     bytes (task 3.3 backward-compat downgrade)",
                    opcode, skip
                );
                Ok(None)
            }
        }
    }
}
