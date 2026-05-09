//! 内部错误类型 + 到 C ABI 状态码的映射
//!
//! 内部代码全部用 `RendererResult<T>`，FFI 边界统一通过 `to_status()`
//! 翻译为稳定的 `RendererStatus` 整数码 —— 调用方（C#）只看错误码 +
//! 通过日志回调拿到详细文本。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RendererError {
    #[error("invalid parameter: {0}")]
    InvalidParam(&'static str),

    #[error("D3D11 device init failed: {0}")]
    DeviceInit(#[source] windows::core::Error),

    #[error("offscreen surface init failed: {0}")]
    SwapChainInit(#[source] windows::core::Error),

    #[allow(dead_code)]
    #[error("render thread init failed: {0}")]
    ThreadInit(String),

    #[error("frame still held: operation not allowed between begin_frame and end_frame")]
    FrameStillHeld,

    #[allow(dead_code)]
    #[error("frame acquire/map failed: {0}")]
    FrameAcquire(#[source] windows::core::Error),

    #[error("resource handle not found or expired")]
    ResourceNotFound,

    #[error("resource slot table is full")]
    ResourceLimit,

    #[error("decode failed: {0}")]
    DecodeFail(#[source] windows::core::Error),

    #[error("io failed: {0}")]
    Io(#[source] std::io::Error),

    #[error("unsupported format: {0}")]
    UnsupportedFormat(&'static str),

    #[allow(dead_code)]
    #[error("canvas resize failed: {0}")]
    CanvasResizeFail(#[source] windows::core::Error),

    #[error("video open failed: {0}")]
    VideoOpenFail(#[source] windows::core::Error),

    #[error("video handle not found or expired")]
    VideoNotFound,

    #[error("video seek failed: {0}")]
    VideoSeekFail(#[source] windows::core::Error),

    #[error("video decode failed: {0}")]
    VideoDecodeFail(String),

    #[error("video stream format changed mid-decode")]
    VideoFormatChanged,
}

pub type RendererResult<T> = Result<T, RendererError>;
