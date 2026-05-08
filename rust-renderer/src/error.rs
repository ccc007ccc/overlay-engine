//! 内部错误类型 + 到 C ABI 状态码的映射
//!
//! 内部代码全部用 `RendererResult<T>`，FFI 边界统一通过 `to_status()`
//! 翻译为稳定的 `RendererStatus` 整数码 —— 调用方（C#）只看错误码 +
//! 通过日志回调拿到详细文本。

use crate::ffi::{
    RendererStatus, RENDERER_ERR_CANVAS_RESIZE_FAIL, RENDERER_ERR_DECODE_FAIL,
    RENDERER_ERR_DEVICE_INIT, RENDERER_ERR_FRAME_ACQUIRE, RENDERER_ERR_FRAME_HELD,
    RENDERER_ERR_INVALID_PARAM, RENDERER_ERR_IO, RENDERER_ERR_RESOURCE_LIMIT,
    RENDERER_ERR_RESOURCE_NOT_FOUND, RENDERER_ERR_SWAPCHAIN_INIT, RENDERER_ERR_THREAD_INIT,
    RENDERER_ERR_UNSUPPORTED_FORMAT, RENDERER_ERR_VIDEO_DECODE_FAIL,
    RENDERER_ERR_VIDEO_FORMAT_CHANGED, RENDERER_ERR_VIDEO_NOT_FOUND, RENDERER_ERR_VIDEO_OPEN_FAIL,
    RENDERER_ERR_VIDEO_SEEK_FAIL,
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

    /// 命令式状态机违例：业务在 `begin_frame` / `end_frame` 之间调用了不允许同帧
    /// 进行的操作（典型场景：v0.7 `resize_canvas` 要求帧外调用，spec §2.6.3）。
    /// 历史名称沿用 V1 pull-driven 时期（acquire_frame / release_frame 协议错误）；
    /// 错误码 `RENDERER_ERR_FRAME_HELD`(-6) 不变。
    #[error("frame still held: operation not allowed between begin_frame and end_frame")]
    FrameStillHeld,

    /// 渲染或 Map staging 失败。V2 路径下 acquire 不会构造它（acquire 内部只可能
    /// 因 `FrameStillHeld` 失败，渲染调用本身不返 Err）。保留变体让 ABI 错误码与 enum
    /// 一一对应，避免错误码常量与 enum 之间出现裂缝。
    #[allow(dead_code)]
    #[error("frame acquire/map failed: {0}")]
    FrameAcquire(#[source] windows::core::Error),

    // ---------- v0.7 phase 2 资源系统 ----------
    /// Bitmap / Video / Capture handle 找不到（已 destroy 或从未存在）。
    /// 包含 ABA 失败：handle 的 generation 与 slot 当前 generation 不匹配。
    #[error("resource handle not found or expired")]
    ResourceNotFound,

    /// Slot table 满（默认 BITMAP_SLOT_CAPACITY = 1024）。
    #[error("resource slot table is full")]
    ResourceLimit,

    /// 图片 / 视频 / 纹理解码失败。WIC 返非零 HRESULT，或字节流不识别。
    #[error("decode failed: {0}")]
    DecodeFail(#[source] windows::core::Error),

    /// 文件 IO 失败（路径不存在、权限不足、读写错误）。
    #[error("io failed: {0}")]
    Io(#[source] std::io::Error),

    /// 编码格式不支持（path opcode 0x06+ 等保留区间，或未来 NV12 但当前 BGRA8 only 等）。
    #[error("unsupported format: {0}")]
    UnsupportedFormat(&'static str),

    /// `renderer_resize` 时 ResizeBuffers / 重建 D2D bitmap render target 失败（含 device-lost）。
    /// v0.7 spec §2.6 占位 —— 当前 begin_frame 内部仍走自动 resize 路径，不主动构造此变体；
    /// 留给后续 phase 做 resize ABI 行为升级时使用。
    #[allow(dead_code)]
    #[error("canvas resize failed: {0}")]
    CanvasResizeFail(#[source] windows::core::Error),

    // ---------- v0.7 phase 3 video ----------
    /// MF Source Reader 打开 / 配置失败：文件不存在、codec 不支持、DRM 拒绝。
    #[error("video open failed: {0}")]
    VideoOpenFail(#[source] windows::core::Error),

    /// 业务用了已 close / 未分配 / generation 不匹配的 VideoHandle。
    #[error("video handle not found or expired")]
    VideoNotFound,

    /// SetCurrentPosition 失败（越界 / HRESULT 错）。
    #[error("video seek failed: {0}")]
    VideoSeekFail(#[source] windows::core::Error),

    /// ReadSample / Lock / Buffer 校验失败。
    #[error("video decode failed: {0}")]
    VideoDecodeFail(String),

    /// 解码中流类型变化（codec 切了输出格式），业务需要重新 open。
    #[error("video stream format changed mid-decode")]
    VideoFormatChanged,
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
            Self::ResourceNotFound => RENDERER_ERR_RESOURCE_NOT_FOUND,
            Self::ResourceLimit => RENDERER_ERR_RESOURCE_LIMIT,
            Self::DecodeFail(_) => RENDERER_ERR_DECODE_FAIL,
            Self::Io(_) => RENDERER_ERR_IO,
            Self::UnsupportedFormat(_) => RENDERER_ERR_UNSUPPORTED_FORMAT,
            Self::CanvasResizeFail(_) => RENDERER_ERR_CANVAS_RESIZE_FAIL,
            Self::VideoOpenFail(_) => RENDERER_ERR_VIDEO_OPEN_FAIL,
            Self::VideoNotFound => RENDERER_ERR_VIDEO_NOT_FOUND,
            Self::VideoSeekFail(_) => RENDERER_ERR_VIDEO_SEEK_FAIL,
            Self::VideoDecodeFail(_) => RENDERER_ERR_VIDEO_DECODE_FAIL,
            Self::VideoFormatChanged => RENDERER_ERR_VIDEO_FORMAT_CHANGED,
        }
    }
}

pub(crate) type RendererResult<T> = Result<T, RendererError>;
