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
pub const OP_REGISTER_PRODUCER: u16 = 0x0001;
pub const OP_REGISTER_CONSUMER: u16 = 0x0002;
pub const OP_CREATE_CANVAS: u16 = 0x0003;
pub const OP_ATTACH_CONSUMER: u16 = 0x0004;
pub const OP_CANVAS_ATTACHED: u16 = 0x0005;
pub const OP_SUBMIT_FRAME: u16 = 0x0006;

#[derive(Debug, Clone)]
pub enum ControlMessage {
    RegisterProducer { pid: u32 },
    RegisterConsumer { pid: u32 },
    CreateCanvas {
        logical_w: u32,
        logical_h: u32,
        render_w: u32,
        render_h: u32,
    },
    AttachConsumer {
        canvas_id: u32,
        consumer_id: u32,
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
}

impl ControlMessage {
    pub fn opcode(&self) -> u16 {
        match self {
            Self::RegisterProducer { .. } => OP_REGISTER_PRODUCER,
            Self::RegisterConsumer { .. } => OP_REGISTER_CONSUMER,
            Self::CreateCanvas { .. } => OP_CREATE_CANVAS,
            Self::AttachConsumer { .. } => OP_ATTACH_CONSUMER,
            Self::CanvasAttached { .. } => OP_CANVAS_ATTACHED,
            Self::SubmitFrame { .. } => OP_SUBMIT_FRAME,
        }
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        match self {
            Self::RegisterProducer { pid } | Self::RegisterConsumer { pid } => {
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
            Self::AttachConsumer {
                canvas_id,
                consumer_id,
            } => {
                let header = MessageHeader {
                    opcode: self.opcode(),
                    payload_len: 8,
                };
                header.encode(buf);
                buf.put_u32_le(*canvas_id);
                buf.put_u32_le(*consumer_id);
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
        }
    }

    pub fn decode(opcode: u16, buf: &mut BytesMut) -> Result<Self, ProtocolError> {
        match opcode {
            OP_REGISTER_PRODUCER => {
                if buf.remaining() < 4 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 4,
                        actual: buf.remaining(),
                    });
                }
                Ok(Self::RegisterProducer {
                    pid: buf.get_u32_le(),
                })
            }
            OP_REGISTER_CONSUMER => {
                if buf.remaining() < 4 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 4,
                        actual: buf.remaining(),
                    });
                }
                Ok(Self::RegisterConsumer {
                    pid: buf.get_u32_le(),
                })
            }
            OP_CREATE_CANVAS => {
                if buf.remaining() < 16 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 16,
                        actual: buf.remaining(),
                    });
                }
                Ok(Self::CreateCanvas {
                    logical_w: buf.get_u32_le(),
                    logical_h: buf.get_u32_le(),
                    render_w: buf.get_u32_le(),
                    render_h: buf.get_u32_le(),
                })
            }
            OP_ATTACH_CONSUMER => {
                if buf.remaining() < 8 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 8,
                        actual: buf.remaining(),
                    });
                }
                Ok(Self::AttachConsumer {
                    canvas_id: buf.get_u32_le(),
                    consumer_id: buf.get_u32_le(),
                })
            }
            OP_CANVAS_ATTACHED => {
                if buf.remaining() < 28 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 28,
                        actual: buf.remaining(),
                    });
                }
                Ok(Self::CanvasAttached {
                    canvas_id: buf.get_u32_le(),
                    surface_handle: buf.get_u64_le(),
                    logical_w: buf.get_u32_le(),
                    logical_h: buf.get_u32_le(),
                    render_w: buf.get_u32_le(),
                    render_h: buf.get_u32_le(),
                })
            }
            OP_SUBMIT_FRAME => {
                if buf.remaining() < 20 {
                    return Err(ProtocolError::BufferTooSmall {
                        expected: 20,
                        actual: buf.remaining(),
                    });
                }
                Ok(Self::SubmitFrame {
                    canvas_id: buf.get_u32_le(),
                    frame_id: buf.get_u64_le(),
                    offset: buf.get_u32_le(),
                    length: buf.get_u32_le(),
                })
            }
            _ => Err(ProtocolError::UnknownOpcode(opcode)),
        }
    }
}
