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
    #[error("Invalid {field}: {value}")]
    InvalidEnum { field: &'static str, value: u8 },
}

pub const MAGIC: u32 = 0x4F56524C;
pub const VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 12;

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

        Ok(Self {
            opcode: buf.get_u16_le(),
            payload_len: buf.get_u32_le(),
        })
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u32_le(MAGIC);
        buf.put_u16_le(VERSION);
        buf.put_u16_le(self.opcode);
        buf.put_u32_le(self.payload_len);
    }
}

pub const OP_REGISTER_APP: u16 = 0x0001;
pub const OP_REGISTER_MONITOR: u16 = 0x0002;
pub const OP_CREATE_CANVAS: u16 = 0x0003;
pub const OP_ATTACH_MONITOR: u16 = 0x0004;
pub const OP_CANVAS_ATTACHED: u16 = 0x0005;
pub const OP_SUBMIT_FRAME: u16 = 0x0006;
pub const OP_MONITOR_LOCAL_SURFACE_ATTACHED: u16 = 0x0007;
pub const OP_APP_DETACHED: u16 = 0x0008;
pub const OP_LOAD_BITMAP: u16 = 0x0009;
pub const OP_LIST_MONITOR_TYPES: u16 = 0x000A;
pub const OP_MONITOR_TYPES: u16 = 0x000B;
pub const OP_START_MONITOR: u16 = 0x000C;
pub const OP_START_MONITOR_RESULT: u16 = 0x000D;
pub const OP_STOP_MONITOR: u16 = 0x000E;
pub const OP_STOP_MONITOR_RESULT: u16 = 0x000F;
pub const OP_REGISTER_MONITOR_V2: u16 = 0x0010;
pub const OP_CLOSE_MONITOR: u16 = 0x0011;

pub const DESKTOP_WINDOW_MODE_BORDERED: u32 = 1 << 0;
pub const DESKTOP_WINDOW_MODE_BORDERLESS: u32 = 1 << 1;
pub const DESKTOP_WINDOW_MODE_BORDERLESS_FULLSCREEN: u32 = 1 << 2;
pub const DESKTOP_WINDOW_FLAG_CLICK_THROUGH: u32 = 1 << 0;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppDetachReason {
    GracefulExit = 0,
    IoError = 1,
    Other = 2,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorKind {
    DesktopWindow = 1,
    GameBar = 2,
}

impl MonitorKind {
    pub fn from_wire(value: u8) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::DesktopWindow),
            2 => Ok(Self::GameBar),
            _ => Err(ProtocolError::InvalidEnum {
                field: "MonitorKind",
                value,
            }),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorStartPolicy {
    CoreOnDemand = 1,
    UserManual = 2,
}

impl MonitorStartPolicy {
    pub fn from_wire(value: u8) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::CoreOnDemand),
            2 => Ok(Self::UserManual),
            _ => Err(ProtocolError::InvalidEnum {
                field: "MonitorStartPolicy",
                value,
            }),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopWindowMode {
    Bordered = 1,
    Borderless = 2,
    BorderlessFullscreen = 3,
}

impl DesktopWindowMode {
    pub fn from_wire(value: u8) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::Bordered),
            2 => Ok(Self::Borderless),
            3 => Ok(Self::BorderlessFullscreen),
            _ => Err(ProtocolError::InvalidEnum {
                field: "DesktopWindowMode",
                value,
            }),
        }
    }

    pub fn bit(self) -> u32 {
        match self {
            Self::Bordered => DESKTOP_WINDOW_MODE_BORDERED,
            Self::Borderless => DESKTOP_WINDOW_MODE_BORDERLESS,
            Self::BorderlessFullscreen => DESKTOP_WINDOW_MODE_BORDERLESS_FULLSCREEN,
        }
    }

    pub fn cli_value(self) -> &'static str {
        match self {
            Self::Bordered => "bordered",
            Self::Borderless => "borderless",
            Self::BorderlessFullscreen => "borderless-fullscreen",
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorRequestStatus {
    Ok = 0,
    Unavailable = 1,
    ManualOpenRequired = 2,
    LimitExceeded = 3,
    InvalidCanvas = 4,
    SpawnFailed = 5,
    Timeout = 6,
    NotOwner = 7,
    NotFound = 8,
    NotCoreManaged = 9,
}

impl MonitorRequestStatus {
    pub fn from_wire(value: u8) -> Result<Self, ProtocolError> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::Unavailable),
            2 => Ok(Self::ManualOpenRequired),
            3 => Ok(Self::LimitExceeded),
            4 => Ok(Self::InvalidCanvas),
            5 => Ok(Self::SpawnFailed),
            6 => Ok(Self::Timeout),
            7 => Ok(Self::NotOwner),
            8 => Ok(Self::NotFound),
            9 => Ok(Self::NotCoreManaged),
            _ => Err(ProtocolError::InvalidEnum {
                field: "MonitorRequestStatus",
                value,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorTypeEntry {
    pub kind: MonitorKind,
    pub available: bool,
    pub start_policy: MonitorStartPolicy,
    pub core_startable: bool,
    pub core_managed: bool,
    pub max_instances: u32,
    pub window_modes: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlMessage {
    RegisterApp {
        pid: u32,
    },
    RegisterMonitor {
        pid: u32,
    },
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
    LoadBitmap {
        bitmap_id: u32,
        bytes: Vec<u8>,
    },
    ListMonitorTypes {
        request_id: u32,
    },
    MonitorTypes {
        request_id: u32,
        entries: Vec<MonitorTypeEntry>,
    },
    StartMonitor {
        request_id: u32,
        kind: MonitorKind,
        count: u32,
        target_canvas_id: u32,
        mode: DesktopWindowMode,
        flags: u32,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
    StartMonitorResult {
        request_id: u32,
        status: MonitorRequestStatus,
        monitor_ids: Vec<u32>,
    },
    StopMonitor {
        request_id: u32,
        monitor_id: u32,
    },
    StopMonitorResult {
        request_id: u32,
        status: MonitorRequestStatus,
    },
    RegisterMonitorV2 {
        pid: u32,
        kind: MonitorKind,
        owner_app_id: u32,
        request_id: u32,
        target_canvas_id: u32,
        mode: DesktopWindowMode,
        flags: u32,
        manual_lifecycle: bool,
    },
    CloseMonitor {
        monitor_id: u32,
    },
}

fn fixed_payload_len(opcode: u16) -> Option<usize> {
    match opcode {
        OP_REGISTER_APP | OP_REGISTER_MONITOR | OP_LIST_MONITOR_TYPES | OP_CLOSE_MONITOR => Some(4),
        OP_CREATE_CANVAS => Some(16),
        OP_ATTACH_MONITOR | OP_STOP_MONITOR => Some(8),
        OP_CANVAS_ATTACHED => Some(28),
        OP_SUBMIT_FRAME => Some(20),
        OP_MONITOR_LOCAL_SURFACE_ATTACHED => Some(24),
        OP_APP_DETACHED | OP_STOP_MONITOR_RESULT => Some(5),
        OP_START_MONITOR => Some(34),
        OP_REGISTER_MONITOR_V2 => Some(23),
        _ => None,
    }
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
            Self::LoadBitmap { .. } => OP_LOAD_BITMAP,
            Self::ListMonitorTypes { .. } => OP_LIST_MONITOR_TYPES,
            Self::MonitorTypes { .. } => OP_MONITOR_TYPES,
            Self::StartMonitor { .. } => OP_START_MONITOR,
            Self::StartMonitorResult { .. } => OP_START_MONITOR_RESULT,
            Self::StopMonitor { .. } => OP_STOP_MONITOR,
            Self::StopMonitorResult { .. } => OP_STOP_MONITOR_RESULT,
            Self::RegisterMonitorV2 { .. } => OP_REGISTER_MONITOR_V2,
            Self::CloseMonitor { .. } => OP_CLOSE_MONITOR,
        }
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        match self {
            Self::RegisterApp { pid } | Self::RegisterMonitor { pid } => {
                encode_header(self.opcode(), 4, buf);
                buf.put_u32_le(*pid);
            }
            Self::CreateCanvas {
                logical_w,
                logical_h,
                render_w,
                render_h,
            } => {
                encode_header(self.opcode(), 16, buf);
                buf.put_u32_le(*logical_w);
                buf.put_u32_le(*logical_h);
                buf.put_u32_le(*render_w);
                buf.put_u32_le(*render_h);
            }
            Self::AttachMonitor {
                canvas_id,
                monitor_id,
            } => {
                encode_header(self.opcode(), 8, buf);
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
                encode_header(self.opcode(), 28, buf);
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
                encode_header(self.opcode(), 20, buf);
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
                encode_header(self.opcode(), 24, buf);
                buf.put_u32_le(*canvas_id);
                buf.put_u32_le(*monitor_id);
                buf.put_u64_le(*surface_handle);
                buf.put_u32_le(*logical_w);
                buf.put_u32_le(*logical_h);
            }
            Self::AppDetached { app_id, reason } => {
                encode_header(self.opcode(), 5, buf);
                buf.put_u32_le(*app_id);
                buf.put_u8(*reason);
            }
            Self::LoadBitmap { bitmap_id, bytes } => {
                encode_header(self.opcode(), 8 + bytes.len() as u32, buf);
                buf.put_u32_le(*bitmap_id);
                buf.put_u32_le(bytes.len() as u32);
                buf.put_slice(bytes);
            }
            Self::ListMonitorTypes { request_id } => {
                encode_header(self.opcode(), 4, buf);
                buf.put_u32_le(*request_id);
            }
            Self::MonitorTypes {
                request_id,
                entries,
            } => {
                encode_header(self.opcode(), 8 + (entries.len() as u32) * 20, buf);
                buf.put_u32_le(*request_id);
                buf.put_u32_le(entries.len() as u32);
                for entry in entries {
                    buf.put_u8(entry.kind as u8);
                    buf.put_u8(entry.available as u8);
                    buf.put_u8(entry.start_policy as u8);
                    buf.put_u8(entry.core_startable as u8);
                    buf.put_u8(entry.core_managed as u8);
                    buf.put_slice(&[0, 0, 0]);
                    buf.put_u32_le(entry.max_instances);
                    buf.put_u32_le(entry.window_modes);
                    buf.put_u32_le(entry.flags);
                }
            }
            Self::StartMonitor {
                request_id,
                kind,
                count,
                target_canvas_id,
                mode,
                flags,
                x,
                y,
                w,
                h,
            } => {
                encode_header(self.opcode(), 34, buf);
                buf.put_u32_le(*request_id);
                buf.put_u8(*kind as u8);
                buf.put_u32_le(*count);
                buf.put_u32_le(*target_canvas_id);
                buf.put_u8(*mode as u8);
                buf.put_u32_le(*flags);
                buf.put_i32_le(*x);
                buf.put_i32_le(*y);
                buf.put_u32_le(*w);
                buf.put_u32_le(*h);
            }
            Self::StartMonitorResult {
                request_id,
                status,
                monitor_ids,
            } => {
                encode_header(self.opcode(), 9 + (monitor_ids.len() as u32) * 4, buf);
                buf.put_u32_le(*request_id);
                buf.put_u8(*status as u8);
                buf.put_u32_le(monitor_ids.len() as u32);
                for id in monitor_ids {
                    buf.put_u32_le(*id);
                }
            }
            Self::StopMonitor {
                request_id,
                monitor_id,
            } => {
                encode_header(self.opcode(), 8, buf);
                buf.put_u32_le(*request_id);
                buf.put_u32_le(*monitor_id);
            }
            Self::StopMonitorResult { request_id, status } => {
                encode_header(self.opcode(), 5, buf);
                buf.put_u32_le(*request_id);
                buf.put_u8(*status as u8);
            }
            Self::RegisterMonitorV2 {
                pid,
                kind,
                owner_app_id,
                request_id,
                target_canvas_id,
                mode,
                flags,
                manual_lifecycle,
            } => {
                encode_header(self.opcode(), 23, buf);
                buf.put_u32_le(*pid);
                buf.put_u8(*kind as u8);
                buf.put_u32_le(*owner_app_id);
                buf.put_u32_le(*request_id);
                buf.put_u32_le(*target_canvas_id);
                buf.put_u8(*mode as u8);
                buf.put_u32_le(*flags);
                buf.put_u8(*manual_lifecycle as u8);
            }
            Self::CloseMonitor { monitor_id } => {
                encode_header(self.opcode(), 4, buf);
                buf.put_u32_le(*monitor_id);
            }
        }
    }

    pub fn decode(
        opcode: u16,
        payload_len: u32,
        buf: &mut BytesMut,
    ) -> Result<Option<Self>, ProtocolError> {
        let payload_len = payload_len as usize;
        if let Some(expected) = fixed_payload_len(opcode) {
            if payload_len != expected {
                return Err(ProtocolError::PayloadLengthMismatch);
            }
        }
        if buf.remaining() < payload_len {
            return Err(ProtocolError::BufferTooSmall {
                expected: payload_len,
                actual: buf.remaining(),
            });
        }

        match opcode {
            OP_REGISTER_APP => Ok(Some(Self::RegisterApp {
                pid: buf.get_u32_le(),
            })),
            OP_REGISTER_MONITOR => Ok(Some(Self::RegisterMonitor {
                pid: buf.get_u32_le(),
            })),
            OP_CREATE_CANVAS => Ok(Some(Self::CreateCanvas {
                logical_w: buf.get_u32_le(),
                logical_h: buf.get_u32_le(),
                render_w: buf.get_u32_le(),
                render_h: buf.get_u32_le(),
            })),
            OP_ATTACH_MONITOR => Ok(Some(Self::AttachMonitor {
                canvas_id: buf.get_u32_le(),
                monitor_id: buf.get_u32_le(),
            })),
            OP_CANVAS_ATTACHED => Ok(Some(Self::CanvasAttached {
                canvas_id: buf.get_u32_le(),
                surface_handle: buf.get_u64_le(),
                logical_w: buf.get_u32_le(),
                logical_h: buf.get_u32_le(),
                render_w: buf.get_u32_le(),
                render_h: buf.get_u32_le(),
            })),
            OP_SUBMIT_FRAME => Ok(Some(Self::SubmitFrame {
                canvas_id: buf.get_u32_le(),
                frame_id: buf.get_u64_le(),
                offset: buf.get_u32_le(),
                length: buf.get_u32_le(),
            })),
            OP_MONITOR_LOCAL_SURFACE_ATTACHED => Ok(Some(Self::MonitorLocalSurfaceAttached {
                canvas_id: buf.get_u32_le(),
                monitor_id: buf.get_u32_le(),
                surface_handle: buf.get_u64_le(),
                logical_w: buf.get_u32_le(),
                logical_h: buf.get_u32_le(),
            })),
            OP_APP_DETACHED => Ok(Some(Self::AppDetached {
                app_id: buf.get_u32_le(),
                reason: buf.get_u8(),
            })),
            OP_LOAD_BITMAP => {
                if payload_len < 8 {
                    return Err(ProtocolError::PayloadLengthMismatch);
                }
                let bitmap_id = buf.get_u32_le();
                let byte_len = buf.get_u32_le() as usize;
                if byte_len != payload_len - 8 {
                    return Err(ProtocolError::PayloadLengthMismatch);
                }
                Ok(Some(Self::LoadBitmap {
                    bitmap_id,
                    bytes: buf.copy_to_bytes(byte_len).to_vec(),
                }))
            }
            OP_LIST_MONITOR_TYPES => Ok(Some(Self::ListMonitorTypes {
                request_id: buf.get_u32_le(),
            })),
            OP_MONITOR_TYPES => {
                if payload_len < 8 {
                    return Err(ProtocolError::PayloadLengthMismatch);
                }
                let request_id = buf.get_u32_le();
                let count = buf.get_u32_le() as usize;
                if payload_len != 8 + count * 20 {
                    return Err(ProtocolError::PayloadLengthMismatch);
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let kind = MonitorKind::from_wire(buf.get_u8())?;
                    let available = buf.get_u8() != 0;
                    let start_policy = MonitorStartPolicy::from_wire(buf.get_u8())?;
                    let core_startable = buf.get_u8() != 0;
                    let core_managed = buf.get_u8() != 0;
                    buf.advance(3);
                    entries.push(MonitorTypeEntry {
                        kind,
                        available,
                        start_policy,
                        core_startable,
                        core_managed,
                        max_instances: buf.get_u32_le(),
                        window_modes: buf.get_u32_le(),
                        flags: buf.get_u32_le(),
                    });
                }
                Ok(Some(Self::MonitorTypes {
                    request_id,
                    entries,
                }))
            }
            OP_START_MONITOR => Ok(Some(Self::StartMonitor {
                request_id: buf.get_u32_le(),
                kind: MonitorKind::from_wire(buf.get_u8())?,
                count: buf.get_u32_le(),
                target_canvas_id: buf.get_u32_le(),
                mode: DesktopWindowMode::from_wire(buf.get_u8())?,
                flags: buf.get_u32_le(),
                x: buf.get_i32_le(),
                y: buf.get_i32_le(),
                w: buf.get_u32_le(),
                h: buf.get_u32_le(),
            })),
            OP_START_MONITOR_RESULT => {
                if payload_len < 9 {
                    return Err(ProtocolError::PayloadLengthMismatch);
                }
                let request_id = buf.get_u32_le();
                let status = MonitorRequestStatus::from_wire(buf.get_u8())?;
                let count = buf.get_u32_le() as usize;
                if payload_len != 9 + count * 4 {
                    return Err(ProtocolError::PayloadLengthMismatch);
                }
                let mut monitor_ids = Vec::with_capacity(count);
                for _ in 0..count {
                    monitor_ids.push(buf.get_u32_le());
                }
                Ok(Some(Self::StartMonitorResult {
                    request_id,
                    status,
                    monitor_ids,
                }))
            }
            OP_STOP_MONITOR => Ok(Some(Self::StopMonitor {
                request_id: buf.get_u32_le(),
                monitor_id: buf.get_u32_le(),
            })),
            OP_STOP_MONITOR_RESULT => Ok(Some(Self::StopMonitorResult {
                request_id: buf.get_u32_le(),
                status: MonitorRequestStatus::from_wire(buf.get_u8())?,
            })),
            OP_REGISTER_MONITOR_V2 => Ok(Some(Self::RegisterMonitorV2 {
                pid: buf.get_u32_le(),
                kind: MonitorKind::from_wire(buf.get_u8())?,
                owner_app_id: buf.get_u32_le(),
                request_id: buf.get_u32_le(),
                target_canvas_id: buf.get_u32_le(),
                mode: DesktopWindowMode::from_wire(buf.get_u8())?,
                flags: buf.get_u32_le(),
                manual_lifecycle: buf.get_u8() != 0,
            })),
            OP_CLOSE_MONITOR => Ok(Some(Self::CloseMonitor {
                monitor_id: buf.get_u32_le(),
            })),
            _ => {
                buf.advance(payload_len);
                eprintln!(
                    "[protocol] unknown opcode {:#06x} — skipping {} payload bytes",
                    opcode, payload_len
                );
                Ok(None)
            }
        }
    }
}

fn encode_header(opcode: u16, payload_len: u32, buf: &mut BytesMut) {
    MessageHeader {
        opcode,
        payload_len,
    }
    .encode(buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known_payload_samples() -> Vec<(u16, Vec<u8>)> {
        let samples = [
            ControlMessage::RegisterApp { pid: 1 },
            ControlMessage::RegisterMonitor { pid: 2 },
            ControlMessage::CreateCanvas {
                logical_w: 800,
                logical_h: 600,
                render_w: 800,
                render_h: 600,
            },
            ControlMessage::AttachMonitor {
                canvas_id: 3,
                monitor_id: 4,
            },
            ControlMessage::CanvasAttached {
                canvas_id: 5,
                surface_handle: 0x1234,
                logical_w: 1024,
                logical_h: 768,
                render_w: 1024,
                render_h: 768,
            },
            ControlMessage::SubmitFrame {
                canvas_id: 6,
                frame_id: 7,
                offset: 24,
                length: 128,
            },
            ControlMessage::MonitorLocalSurfaceAttached {
                canvas_id: 8,
                monitor_id: 9,
                surface_handle: 0x5678,
                logical_w: 1920,
                logical_h: 1080,
            },
            ControlMessage::AppDetached {
                app_id: 10,
                reason: AppDetachReason::IoError as u8,
            },
            ControlMessage::ListMonitorTypes { request_id: 11 },
            ControlMessage::StartMonitor {
                request_id: 12,
                kind: MonitorKind::DesktopWindow,
                count: 2,
                target_canvas_id: 13,
                mode: DesktopWindowMode::Borderless,
                flags: DESKTOP_WINDOW_FLAG_CLICK_THROUGH,
                x: -1,
                y: 2,
                w: 3,
                h: 4,
            },
            ControlMessage::StopMonitor {
                request_id: 14,
                monitor_id: 15,
            },
            ControlMessage::StopMonitorResult {
                request_id: 16,
                status: MonitorRequestStatus::NotCoreManaged,
            },
            ControlMessage::RegisterMonitorV2 {
                pid: 17,
                kind: MonitorKind::GameBar,
                owner_app_id: 0,
                request_id: 0,
                target_canvas_id: 0,
                mode: DesktopWindowMode::Bordered,
                flags: 0,
                manual_lifecycle: true,
            },
            ControlMessage::CloseMonitor { monitor_id: 18 },
        ];

        samples
            .into_iter()
            .map(|msg| {
                let mut buf = BytesMut::new();
                msg.encode(&mut buf);
                let header = MessageHeader::decode(&mut buf).unwrap();
                (header.opcode, buf.to_vec())
            })
            .collect()
    }

    fn roundtrip(msg: ControlMessage) {
        let mut buf = BytesMut::new();
        msg.encode(&mut buf);
        let header = MessageHeader::decode(&mut buf).unwrap();
        let decoded = ControlMessage::decode(header.opcode, header.payload_len, &mut buf)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, msg);
        assert!(buf.is_empty());
    }

    #[test]
    fn known_opcodes_decode_with_exact_payload_lengths() {
        for (opcode, payload) in known_payload_samples() {
            let mut buf = BytesMut::from(&payload[..]);
            let decoded = ControlMessage::decode(opcode, payload.len() as u32, &mut buf)
                .unwrap_or_else(|e| panic!("opcode {opcode:#06x} failed exact decode: {e}"));
            assert!(decoded.is_some());
            assert!(buf.is_empty(), "opcode {opcode:#06x} left bytes behind");
        }
    }

    #[test]
    fn known_fixed_opcodes_reject_short_or_long_payload_lengths() {
        for (opcode, payload) in known_payload_samples() {
            if matches!(
                opcode,
                OP_LOAD_BITMAP | OP_MONITOR_TYPES | OP_START_MONITOR_RESULT
            ) {
                continue;
            }
            let mut short_buf = BytesMut::from(&payload[..]);
            let short = ControlMessage::decode(opcode, payload.len() as u32 - 1, &mut short_buf);
            assert!(matches!(short, Err(ProtocolError::PayloadLengthMismatch)));

            let mut long_buf = BytesMut::from(&payload[..]);
            let long = ControlMessage::decode(opcode, payload.len() as u32 + 1, &mut long_buf);
            assert!(matches!(long, Err(ProtocolError::PayloadLengthMismatch)));
        }
    }

    #[test]
    fn monitor_types_roundtrip() {
        roundtrip(ControlMessage::MonitorTypes {
            request_id: 21,
            entries: vec![
                MonitorTypeEntry {
                    kind: MonitorKind::DesktopWindow,
                    available: true,
                    start_policy: MonitorStartPolicy::CoreOnDemand,
                    core_startable: true,
                    core_managed: true,
                    max_instances: 16,
                    window_modes: DESKTOP_WINDOW_MODE_BORDERED | DESKTOP_WINDOW_MODE_BORDERLESS,
                    flags: DESKTOP_WINDOW_FLAG_CLICK_THROUGH,
                },
                MonitorTypeEntry {
                    kind: MonitorKind::GameBar,
                    available: true,
                    start_policy: MonitorStartPolicy::UserManual,
                    core_startable: false,
                    core_managed: false,
                    max_instances: 1,
                    window_modes: 0,
                    flags: 0,
                },
            ],
        });
    }

    #[test]
    fn start_monitor_result_roundtrip() {
        roundtrip(ControlMessage::StartMonitorResult {
            request_id: 22,
            status: MonitorRequestStatus::Ok,
            monitor_ids: vec![3, 4],
        });
    }

    #[test]
    fn unknown_opcode_skips_full_advertised_payload() {
        let payload = [1, 2, 3, 4, 5];
        let mut buf = BytesMut::from(&payload[..]);
        let decoded = ControlMessage::decode(0x9000, payload.len() as u32, &mut buf).unwrap();
        assert!(decoded.is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn unknown_opcode_rejects_truncated_payload() {
        let mut buf = BytesMut::from(&[1, 2][..]);
        let decoded = ControlMessage::decode(0x9000, 3, &mut buf);
        assert!(matches!(
            decoded,
            Err(ProtocolError::BufferTooSmall {
                expected: 3,
                actual: 2
            })
        ));
    }
}
