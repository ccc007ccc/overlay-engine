use bytes::{Buf, BufMut, BytesMut};

pub const SINGLETON_PIPE_NAME: &str = r"\\.\pipe\overlay-desktop-window-monitor-singleton";
pub const SINGLETON_OP_OPEN_WINDOW: u16 = 0x0101;
pub const SINGLETON_OP_ACK: u16 = 0x0201;
pub const SINGLETON_OP_NACK: u16 = 0x0202;
const SINGLETON_HEADER_SIZE: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingletonFrameError {
    BufferTooSmall { expected: usize, actual: usize },
    UnknownOpcode(u16),
    PayloadLengthMismatch { expected: u32, actual: u32 },
    Utf8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SingletonRequest {
    OpenWindow { target_canvas_id: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingletonResponse {
    Ack { pid: u32, new_monitor_id: u32 },
    Nack { reason: u16, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorWindowSnapshot {
    pub monitor_id: u32,
    pub target_canvas_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingletonState {
    pub monitor_process_pid: u32,
    pub registered_windows: Vec<MonitorWindowSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsPipeState {
    NoPipe,
    PipeExistsAcceptsInWindow,
    PipeExistsStale,
    Race,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BecomeOutcome {
    MonitorProcess,
    Launcher,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryBecomeErr {
    Race,
}

pub fn try_become_singleton(osr: OsPipeState) -> Result<BecomeOutcome, TryBecomeErr> {
    match osr {
        OsPipeState::NoPipe | OsPipeState::PipeExistsStale => Ok(BecomeOutcome::MonitorProcess),
        OsPipeState::PipeExistsAcceptsInWindow => Ok(BecomeOutcome::Launcher),
        OsPipeState::Race => Err(TryBecomeErr::Race),
    }
}

pub fn handle_singleton_request(
    req: SingletonRequest,
    state: &mut SingletonState,
) -> SingletonResponse {
    match req {
        SingletonRequest::OpenWindow { target_canvas_id } => {
            let new_monitor_id = state
                .registered_windows
                .iter()
                .map(|w| w.monitor_id)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
            state.registered_windows.push(MonitorWindowSnapshot {
                monitor_id: new_monitor_id,
                target_canvas_id,
            });
            SingletonResponse::Ack {
                pid: state.monitor_process_pid,
                new_monitor_id,
            }
        }
    }
}

pub fn launcher_log_line(pid: u32) -> String {
    format!("forwarded open-window request to existing monitor-process (pid {pid}), exiting")
}

pub fn encode_request(req: SingletonRequest, buf: &mut BytesMut) {
    match req {
        SingletonRequest::OpenWindow { target_canvas_id } => {
            buf.put_u16_le(SINGLETON_OP_OPEN_WINDOW);
            buf.put_u32_le(4);
            buf.put_u32_le(target_canvas_id);
        }
    }
}

pub fn decode_request(buf: &mut BytesMut) -> Result<SingletonRequest, SingletonFrameError> {
    let (opcode, len) = decode_header(buf)?;
    match opcode {
        SINGLETON_OP_OPEN_WINDOW => {
            require_len(len, 4)?;
            require_remaining(buf, 4)?;
            Ok(SingletonRequest::OpenWindow {
                target_canvas_id: buf.get_u32_le(),
            })
        }
        _ => Err(SingletonFrameError::UnknownOpcode(opcode)),
    }
}

pub fn encode_response(resp: &SingletonResponse, buf: &mut BytesMut) {
    match resp {
        SingletonResponse::Ack { pid, new_monitor_id } => {
            buf.put_u16_le(SINGLETON_OP_ACK);
            buf.put_u32_le(8);
            buf.put_u32_le(*pid);
            buf.put_u32_le(*new_monitor_id);
        }
        SingletonResponse::Nack { reason, message } => {
            let bytes = message.as_bytes();
            buf.put_u16_le(SINGLETON_OP_NACK);
            buf.put_u32_le(2 + bytes.len() as u32);
            buf.put_u16_le(*reason);
            buf.extend_from_slice(bytes);
        }
    }
}

pub fn decode_response(buf: &mut BytesMut) -> Result<SingletonResponse, SingletonFrameError> {
    let (opcode, len) = decode_header(buf)?;
    match opcode {
        SINGLETON_OP_ACK => {
            require_len(len, 8)?;
            require_remaining(buf, 8)?;
            Ok(SingletonResponse::Ack {
                pid: buf.get_u32_le(),
                new_monitor_id: buf.get_u32_le(),
            })
        }
        SINGLETON_OP_NACK => {
            if len < 2 {
                return Err(SingletonFrameError::PayloadLengthMismatch { expected: 2, actual: len });
            }
            require_remaining(buf, len as usize)?;
            let reason = buf.get_u16_le();
            let msg_bytes = buf.split_to((len - 2) as usize);
            let message = String::from_utf8(msg_bytes.to_vec()).map_err(|_| SingletonFrameError::Utf8)?;
            Ok(SingletonResponse::Nack { reason, message })
        }
        _ => Err(SingletonFrameError::UnknownOpcode(opcode)),
    }
}

fn decode_header(buf: &mut BytesMut) -> Result<(u16, u32), SingletonFrameError> {
    require_remaining(buf, SINGLETON_HEADER_SIZE)?;
    Ok((buf.get_u16_le(), buf.get_u32_le()))
}

fn require_len(actual: u32, expected: u32) -> Result<(), SingletonFrameError> {
    if actual == expected {
        Ok(())
    } else {
        Err(SingletonFrameError::PayloadLengthMismatch { expected, actual })
    }
}

fn require_remaining(buf: &BytesMut, expected: usize) -> Result<(), SingletonFrameError> {
    if buf.remaining() >= expected {
        Ok(())
    } else {
        Err(SingletonFrameError::BufferTooSmall { expected, actual: buf.remaining() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn try_become_singleton_decision_table() {
        assert_eq!(try_become_singleton(OsPipeState::NoPipe), Ok(BecomeOutcome::MonitorProcess));
        assert_eq!(try_become_singleton(OsPipeState::PipeExistsAcceptsInWindow), Ok(BecomeOutcome::Launcher));
        assert_eq!(try_become_singleton(OsPipeState::PipeExistsStale), Ok(BecomeOutcome::MonitorProcess));
        assert_eq!(try_become_singleton(OsPipeState::Race), Err(TryBecomeErr::Race));
    }

    #[test]
    fn launcher_prints_exact_forwarded_log_line() {
        assert_eq!(
            launcher_log_line(42),
            "forwarded open-window request to existing monitor-process (pid 42), exiting"
        );
    }

    #[test]
    fn request_and_response_roundtrip() {
        let req = SingletonRequest::OpenWindow { target_canvas_id: 7 };
        let mut buf = BytesMut::new();
        encode_request(req, &mut buf);
        assert_eq!(decode_request(&mut buf).unwrap(), req);

        let resp = SingletonResponse::Ack { pid: 11, new_monitor_id: 3 };
        let mut buf = BytesMut::new();
        encode_response(&resp, &mut buf);
        assert_eq!(decode_response(&mut buf).unwrap(), resp);
    }

    proptest! {
        #[test]
        fn singleton_open_window_returns_ack_with_correct_pid(
            pid in any::<u32>(),
            target_canvas_id in any::<u32>(),
            existing in prop::collection::vec(any::<u32>(), 0..16)
        ) {
            let mut state = SingletonState {
                monitor_process_pid: pid,
                registered_windows: existing.iter().enumerate().map(|(idx, target)| MonitorWindowSnapshot {
                    monitor_id: idx as u32 + 1,
                    target_canvas_id: *target,
                }).collect(),
            };
            let old_len = state.registered_windows.len();
            let response = handle_singleton_request(
                SingletonRequest::OpenWindow { target_canvas_id },
                &mut state,
            );
            prop_assert_eq!(state.registered_windows.len(), old_len + 1);
            match response {
                SingletonResponse::Ack { pid: ack_pid, new_monitor_id } => {
                    prop_assert_eq!(ack_pid, pid);
                    prop_assert_eq!(state.registered_windows.last().unwrap().monitor_id, new_monitor_id);
                    prop_assert_eq!(state.registered_windows.last().unwrap().target_canvas_id, target_canvas_id);
                }
                _ => prop_assert!(false),
            }
        }
    }
}
