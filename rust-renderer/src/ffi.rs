//! C ABI 类型定义和错误码
//!
//! 这里的常量 / struct 必须与 C# 端 `RendererPInvoke.cs` 严格保持一致。
//! 任何修改都会破坏跨语言契约 —— 不要在没有同步更新 C# 端的情况下变更。
//!
//! ## v0.6 DComp ABI 改动
//! - 删除 `FrameMappedInfo` 和 mapped pointer 返回（不再 readback）
//! - `renderer_end_frame_pinned` → `renderer_end_frame`（无 out 参数）
//! - `renderer_release_frame_pinned` → 删（DComp 不需要 Unmap）
//! - 新增 `renderer_get_swapchain` 让 C# 拿 IDXGISwapChain raw IUnknown
//!
//! ## 保留
//! - 生命周期：create / resize / destroy
//! - 命令式 Painter：begin_frame(viewport) / clear / fill_rect / draw_text / end_frame
//! - 诊断：set_log_callback / get_perf_stats / last_error_string

use std::ffi::c_void;
use std::os::raw::c_char;

use crate::error::RendererResult;

/// 渲染器 API 状态码。
pub type RendererStatus = i32;

pub const RENDERER_OK: RendererStatus = 0;
pub const RENDERER_ERR_INVALID_PARAM: RendererStatus = -1;
pub const RENDERER_ERR_DEVICE_INIT: RendererStatus = -2;
/// 沿用历史 `SWAPCHAIN_INIT`（-3）：覆盖 swap chain / D2D bitmap 创建失败。
pub const RENDERER_ERR_SWAPCHAIN_INIT: RendererStatus = -3;
pub const RENDERER_ERR_THREAD_INIT: RendererStatus = -4;
pub const RENDERER_ERR_NOT_ATTACHED: RendererStatus = -5;
/// 命令式 ABI 状态机违例（重复 begin / 缺 begin）。
pub const RENDERER_ERR_FRAME_HELD: RendererStatus = -6;
/// 渲染失败（D2D EndDraw / Present 等返非零 HRESULT）。
pub const RENDERER_ERR_FRAME_ACQUIRE: RendererStatus = -7;

/// 日志回调函数指针。
///
/// - `level`: 0=trace, 1=debug, 2=info, 3=warn, 4=error
/// - `utf8_msg`: NUL 终止的 UTF-8 字符串（生命周期仅在回调内有效，
///   宿主必须立即拷贝或处理，回调返回后指针失效。
pub type LogCallbackFn = unsafe extern "system" fn(level: i32, utf8_msg: *const c_char);

/// Perf 滑动平均统计（最近 N 帧）。
///
/// `renderer_get_perf_stats` 返回这个 —— C# 端用来在 Flyout 显示渲染管线状况。
/// v0.6 起 `avg_readback_us` / `peak_readback_us` 字段含义改为 Present(0,0) 调用耗时
/// （字段名保留作 ABI 兼容；C# 端 UI 文本可以更新）。
/// ABI 与 `RendererPInvoke.PerfStats` 一致。
#[repr(C)]
pub struct PerfStats {
    /// 最近 N 帧的 render_us 平均值（begin_frame → end_frame 之间所有 cmd_* + EndDraw）
    pub avg_render_us: u64,
    /// 最近 N 帧的 present_us 平均值
    pub avg_readback_us: u64,
    /// 最近 N 帧的总耗时 (render + present) 平均值
    pub avg_total_us: u64,
    /// 历史最大 render_us（不滑动，单调）
    pub peak_render_us: u64,
    /// 历史最大 present_us
    pub peak_readback_us: u64,
    /// 已 end_frame 成功的总帧数
    pub total_frames: u64,
    /// 滑动窗口大小（信息用，C# 端不依赖）
    pub window_size: u32,
    /// 当前窗口里有效样本数（< window_size 时表示还没收满）
    pub valid_samples: u32,
}

/// 不透明渲染器句柄。
///
/// C# 端只持有 `IntPtr`，不能解引用，也不能复制。
/// 唯一合法操作：透传到其他 `renderer_*` 函数；最终用 `renderer_destroy` 释放。
#[repr(C)]
pub struct Renderer {
    inner: parking_lot::Mutex<crate::renderer::RendererState>,
}

impl Renderer {
    pub(crate) fn new(width: u32, height: u32) -> RendererResult<Self> {
        Ok(Self {
            inner: parking_lot::Mutex::new(crate::renderer::RendererState::new(width, height)?),
        })
    }

    pub(crate) fn resize(&self, width: u32, height: u32) -> RendererResult<()> {
        self.inner.lock().resize(width, height)
    }

    /// v0.6 DComp：返回 swap chain 的 IUnknown raw pointer（AddRef 给 C#）
    pub(crate) fn get_swapchain_iunknown(&self) -> *mut c_void {
        self.inner.lock().get_swapchain_iunknown()
    }

    // ===== 命令式 Painter API（forward 到 RendererState） =====
    pub(crate) fn begin_frame(
        &self,
        vp_x: f32,
        vp_y: f32,
        vp_w: f32,
        vp_h: f32,
    ) -> RendererResult<()> {
        self.inner.lock().begin_frame(vp_x, vp_y, vp_w, vp_h)
    }

    pub(crate) fn cmd_clear(&self, color: [f32; 4]) -> RendererResult<()> {
        self.inner.lock().cmd_clear(color)
    }

    pub(crate) fn cmd_fill_rect(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.inner.lock().cmd_fill_rect(x, y, w, h, color)
    }

    pub(crate) fn cmd_draw_text(
        &self,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.inner.lock().cmd_draw_text(text, x, y, font_size, color)
    }

    /// v0.6 DComp end_frame：内部 EndDraw + Present(0, 0)。无 out 参数。
    pub(crate) fn end_frame(&self) -> RendererResult<()> {
        self.inner.lock().end_frame().map(|_| ())
    }

    /// 兼容残留：v0.6 不需要 release。no-op。
    #[allow(dead_code)]
    pub(crate) fn release_pinned_frame(&self) {
        self.inner.lock().release_pinned_frame();
    }

    pub(crate) fn perf_stats(&self) -> PerfStats {
        self.inner.lock().perf_stats()
    }

    #[allow(dead_code)]
    pub(crate) fn size(&self) -> (u32, u32) {
        self.inner.lock().size()
    }
}

// 防意外删除 import 的占位
#[allow(dead_code)]
const _SUPPRESS_UNUSED_C_VOID: Option<*mut c_void> = None;
