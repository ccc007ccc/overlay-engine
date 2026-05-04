//! 内部错误类型 + 到 C ABI 状态码的映射
//!
//! 内部代码全部用 `RendererResult<T>`，FFI 边界统一通过 `to_status()`
//! 翻译为稳定的 `RendererStatus` 整数码 —— 调用方（C#）只看错误码 +
//! 通过日志回调拿到详细文本。

use crate::ffi::{
    RendererStatus, RENDERER_ERR_DEVICE_INIT, RENDERER_ERR_FRAME_ACQUIRE,
    RENDERER_ERR_FRAME_HELD, RENDERER_ERR_INVALID_PARAM, RENDERER_ERR_SWAPCHAIN_INIT,
    RENDERER_ERR_THREAD_INIT,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RendererError {
    #[error("invalid parameter: {0}")]
    InvalidParam(&'static str),

    #[error("D3D11 device init failed: {0}")]
    DeviceInit(#[source] windows::core::Error),

    /// 沿用历史 `SWAPCHAIN_INIT` 错误码语义 —— V1 改 offscreen 路径后，
    /// 这个变体覆盖 render target / RTV / staging texture 创建失败。
    /// 历史上的 ABI 错误码（-3）保持不变，方便 C# 端不用同步改。
    #[error("offscreen surface init failed: {0}")]
    SwapChainInit(#[source] windows::core::Error),

    /// 保留给未来 V2 升级到独立渲染线程时使用；V1 pull-driven 不构造它。
    /// 不删变体是为了让 ABI 错误码 `RENDERER_ERR_THREAD_INIT` 与 enum 一一对应，
    /// 避免错误码常量与 enum 之间出现裂缝。
    #[allow(dead_code)]
    #[error("render thread init failed: {0}")]
    ThreadInit(String),

    /// 调 `acquire_frame` 时上一帧还没 `release` —— 协议错误。
    #[error("frame still held by previous acquire (must release_frame first)")]
    FrameStillHeld,

    /// 渲染或 Map staging 失败。V2 路径下 acquire 不会构造它（acquire 内部只可能
    /// 因 `FrameStillHeld` 失败，渲染调用本身不返 Err）。保留变体让 ABI 错误码与 enum
    /// 一一对应，避免错误码常量与 enum 之间出现裂缝。
    #[allow(dead_code)]
    #[error("frame acquire/map failed: {0}")]
    FrameAcquire(#[source] windows::core::Error),
}

impl RendererError {
    /// 把内部错误投影到稳定的 C ABI 状态码。
    pub(crate) fn to_status(&self) -> RendererStatus {
        match self {
            Self::InvalidParam(_) => RENDERER_ERR_INVALID_PARAM,
            Self::DeviceInit(_) => RENDERER_ERR_DEVICE_INIT,
            Self::SwapChainInit(_) => RENDERER_ERR_SWAPCHAIN_INIT,
            Self::ThreadInit(_) => RENDERER_ERR_THREAD_INIT,
            Self::FrameStillHeld => RENDERER_ERR_FRAME_HELD,
            Self::FrameAcquire(_) => RENDERER_ERR_FRAME_ACQUIRE,
        }
    }
}

pub(crate) type RendererResult<T> = Result<T, RendererError>;
