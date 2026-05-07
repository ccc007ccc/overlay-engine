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

// ---------- v0.7 phase 2 资源系统 ----------
/// Bitmap / Video / Capture handle 失效（已 destroy 或从未存在 / generation 不匹配）。
pub const RENDERER_ERR_RESOURCE_NOT_FOUND: RendererStatus = -8;
/// Slot table 满（默认 BITMAP_SLOT_CAPACITY = 1024）。
pub const RENDERER_ERR_RESOURCE_LIMIT: RendererStatus = -9;
/// 图片 / 视频 / 纹理解码失败。
pub const RENDERER_ERR_DECODE_FAIL: RendererStatus = -10;
/// 文件 IO 失败（不存在、权限不足、读写错）。
pub const RENDERER_ERR_IO: RendererStatus = -11;
/// 编码 / opcode 不支持（含 path opcode 0x06+ 保留区间）。
pub const RENDERER_ERR_UNSUPPORTED_FORMAT: RendererStatus = -12;
/// WGC 初始化失败 / 系统不支持（保留给 phase 4 capture 使用，phase 2 不构造）。
#[allow(dead_code)]
pub const RENDERER_ERR_CAPTURE_INIT: RendererStatus = -13;
/// `renderer_resize_canvas` 主动 ResizeBuffers / 重建 D2D bitmap render target 失败
/// （含 device-lost）。v0.7 lazy-resize 实现下不构造，保留给后续 phase 切到主动模式时使用。
#[allow(dead_code)]
pub const RENDERER_ERR_CANVAS_RESIZE_FAIL: RendererStatus = -14;

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

    /// v0.7 §2.6.3 — 显式 canvas 改尺寸（不动 swap chain；下次 begin_frame 自动重建）。
    pub(crate) fn resize_canvas(&self, new_w: u32, new_h: u32) -> RendererResult<()> {
        self.inner.lock().resize_canvas(new_w, new_h)
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

    // ===== v0.7 矢量图元（薄转发到 RendererState） =====

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_draw_line(
        &self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        stroke_width: f32,
        color: [f32; 4],
        dash_style: i32,
    ) -> RendererResult<()> {
        self.inner
            .lock()
            .cmd_draw_line(x0, y0, x1, y1, stroke_width, color, dash_style)
    }

    pub(crate) fn cmd_draw_polyline(
        &self,
        points: &[(f32, f32)],
        stroke_width: f32,
        color: [f32; 4],
        closed: bool,
    ) -> RendererResult<()> {
        self.inner
            .lock()
            .cmd_draw_polyline(points, stroke_width, color, closed)
    }

    pub(crate) fn cmd_stroke_rect(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        stroke_width: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.inner
            .lock()
            .cmd_stroke_rect(x, y, w, h, stroke_width, color)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_fill_rounded_rect(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius_x: f32,
        radius_y: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.inner
            .lock()
            .cmd_fill_rounded_rect(x, y, w, h, radius_x, radius_y, color)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_stroke_rounded_rect(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius_x: f32,
        radius_y: f32,
        stroke_width: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.inner
            .lock()
            .cmd_stroke_rounded_rect(x, y, w, h, radius_x, radius_y, stroke_width, color)
    }

    pub(crate) fn cmd_fill_ellipse(
        &self,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.inner.lock().cmd_fill_ellipse(cx, cy, rx, ry, color)
    }

    pub(crate) fn cmd_stroke_ellipse(
        &self,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        stroke_width: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.inner
            .lock()
            .cmd_stroke_ellipse(cx, cy, rx, ry, stroke_width, color)
    }

    pub(crate) fn cmd_push_clip_rect(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> RendererResult<()> {
        self.inner.lock().cmd_push_clip_rect(x, y, w, h)
    }

    pub(crate) fn cmd_pop_clip(&self) -> RendererResult<()> {
        self.inner.lock().cmd_pop_clip()
    }

    pub(crate) fn cmd_set_transform(&self, matrix: [f32; 6]) -> RendererResult<()> {
        self.inner.lock().cmd_set_transform(matrix)
    }

    pub(crate) fn cmd_reset_transform(&self) -> RendererResult<()> {
        self.inner.lock().cmd_reset_transform()
    }

    // ===== v0.7 phase 2 bitmap 资源 =====

    pub(crate) fn load_bitmap_from_memory(
        &self,
        bytes: &[u8],
    ) -> RendererResult<crate::renderer::resources::BitmapHandle> {
        self.inner.lock().load_bitmap_from_memory(bytes)
    }

    pub(crate) fn create_texture(
        &self,
        width: u32,
        height: u32,
        format: i32,
    ) -> RendererResult<crate::renderer::resources::BitmapHandle> {
        self.inner.lock().create_texture(width, height, format)
    }

    pub(crate) fn update_texture(
        &self,
        h: crate::renderer::resources::BitmapHandle,
        bytes: &[u8],
        stride: i32,
        format: i32,
    ) -> RendererResult<()> {
        self.inner.lock().update_texture(h, bytes, stride, format)
    }

    pub(crate) fn get_bitmap_size(
        &self,
        h: crate::renderer::resources::BitmapHandle,
    ) -> RendererResult<(u32, u32)> {
        self.inner.lock().get_bitmap_size(h)
    }

    pub(crate) fn destroy_bitmap(
        &self,
        h: crate::renderer::resources::BitmapHandle,
    ) -> RendererResult<()> {
        self.inner.lock().destroy_bitmap(h)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_draw_bitmap(
        &self,
        bitmap: crate::renderer::resources::BitmapHandle,
        src_x: f32,
        src_y: f32,
        src_w: f32,
        src_h: f32,
        dst_x: f32,
        dst_y: f32,
        dst_w: f32,
        dst_h: f32,
        opacity: f32,
        interp_mode: i32,
    ) -> RendererResult<()> {
        self.inner.lock().cmd_draw_bitmap(
            bitmap, src_x, src_y, src_w, src_h, dst_x, dst_y, dst_w, dst_h, opacity, interp_mode,
        )
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
