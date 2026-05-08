//! v1.0 spike：DComp 跨进程 surface 共享公共代码。
//!
//! 验证目标（按 docs/v1.0-server-bootstrap.md §4.1 / §9.2）：
//! 1. Producer 创建 D3D11/IDCompositionDesktopDevice，调
//!    `DCompositionCreateSurfaceHandle` 拿一个 NT HANDLE，作为 IDCompositionSurface 写底
//! 2. 通过命名管道 + DuplicateHandle 把 HANDLE 传给独立 consumer 进程
//! 3. Consumer 拿同一 HANDLE 调 `CreateSurfaceFromHandle` 在自己 DComp tree 里挂
//!    visual，肉眼看到 producer 写的红色块 → 方向 OK
//!
//! 这一步只验证 Win32 ↔ Win32。UWP（widget）作为 consumer 的子 spike 在 README 标注
//! 为下一步任务。

#![allow(clippy::missing_safety_doc)]

pub mod dcomp;
pub mod proto;

use windows::Win32::Foundation::HANDLE;

/// 跨进程使用的命名管道路径。Producer create，consumer connect。
///
/// 名字简单 + 不带 sandbox 兼容 SDDL；首版 spike 只在 Win32-Win32 跑。
/// UWP 子 spike 时再换成 SDDL 允许 ALL APPLICATION PACKAGES (S-1-15-2-1) 的版本。
pub const PIPE_PATH: &str = r"\\.\pipe\overlay-spike-dcomp";

/// HANDLE 在 64-bit Windows 上是 8 字节（在 32-bit 上是 4 字节，spike 只支持 x64）。
pub fn handle_to_u64(h: HANDLE) -> u64 {
    h.0 as u64
}

pub fn u64_to_handle(v: u64) -> HANDLE {
    HANDLE(v as *mut _)
}
