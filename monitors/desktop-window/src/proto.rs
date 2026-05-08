//! 命名管道协议（极简）。
//!
//! 一帧两步：consumer 连上 → 写 4 字节 LE u32 (consumer pid) → producer DuplicateHandle
//! → 回写 8 字节 LE u64（duplicate 后的 HANDLE 值）。Producer 侧后续不再写，consumer
//! 拿到 handle 后断开管道（producer accept 下一个连接）。
//!
//! 不带 magic/version：spike 代码，不用考虑兼容；正式 v1.0 IPC 协议另起。

use std::io::{Read, Write};
use windows::core::Result;

pub fn read_pid<R: Read>(r: &mut R) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

pub fn write_pid<W: Write>(w: &mut W, pid: u32) -> std::io::Result<()> {
    w.write_all(&pid.to_le_bytes())
}

pub fn read_handle_u64<R: Read>(r: &mut R) -> std::io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

pub fn write_handle_u64<W: Write>(w: &mut W, value: u64) -> std::io::Result<()> {
    w.write_all(&value.to_le_bytes())
}

/// 把 windows::core::Error 转成 io::Error，方便 ? 跨用。
pub fn map_win<T>(r: Result<T>) -> std::io::Result<T> {
    r.map_err(|e| std::io::Error::other(format!("win error: {e}")))
}
