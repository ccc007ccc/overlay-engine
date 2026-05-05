//! Overlay engine renderer cdylib（C ABI 入口）
//!
//! 此 crate 是纯渲染引擎库，暴露最小 C ABI，供任何能 DllImport 的语言（C# / Rust /
//! Python / C++ 等）调用。Game Bar Widget 只是众多输出宿主之一。
//!
//! 调用约定：所有导出函数使用 `extern "system"`，
//! 在 Windows x64 下等价于 MS x64 ABI（与 C# `CallingConvention.StdCall` 一致）。
//!
//! ## 阶段
//! - **R1-R3（已废弃）**：SwapChainPanel + ISwapChainPanelNative
//! - **V1-V3 Pinned（已废弃）**：CPU readback + WriteableBitmap → modal 拖动期间画面冻结
//! - **v0.5 viewport-aware（已废弃）**：viewport-sized RT 池，但仍用 wb 路径，modal 问题依旧
//! - **v0.6 DComp（当前）**：CreateSwapChainForComposition + DComp visual。Rust 端
//!   begin_frame → cmd_* → end_frame（内部 EndDraw + Present(0,0)）。C# 端拿 swap chain ptr
//!   通过 ICompositorInterop 包成 ICompositionSurface，挂到 ElementCompositionPreview 的
//!   child visual。DWM 内核合成器直接显示，**modal 不阻塞**。
//!
//! ## 调用流（典型）
//! ```text
//! init:    renderer_set_log_callback(cb)
//!          renderer_create(canvas_w, canvas_h, &handle)
//!          renderer_get_swapchain(handle, &swapchain_iunknown)
//!          // C# 用 swapchain_iunknown 包装成 ICompositionSurface 挂到 widget visual
//! 每帧:    renderer_begin_frame(handle, vp_x, vp_y, vp_w, vp_h)
//!          renderer_clear/fill_rect/draw_text(...) ...
//!          renderer_end_frame(handle)
//!          // 内部 Present，DComp 自动拉新内容；C# 不需要做任何 readback / 同步
//! resize:  renderer_resize(handle, canvas_w, canvas_h)
//! 关闭:    renderer_destroy(handle)
//! ```

#![allow(clippy::missing_safety_doc)]

mod error;
mod ffi;
mod log;
mod renderer;

use std::ffi::c_void;

pub use crate::ffi::{
    LogCallbackFn, PerfStats, Renderer, RendererStatus, RENDERER_ERR_DEVICE_INIT,
    RENDERER_ERR_FRAME_ACQUIRE, RENDERER_ERR_FRAME_HELD, RENDERER_ERR_INVALID_PARAM,
    RENDERER_ERR_NOT_ATTACHED, RENDERER_ERR_SWAPCHAIN_INIT, RENDERER_ERR_THREAD_INIT, RENDERER_OK,
};

/// 创建渲染器。
///
/// # 参数
/// - `pixel_width` / `pixel_height`: canvas 逻辑尺寸（业务命令坐标系参考；通常 = 显示器物理像素）
/// - `out_handle`: 成功时写入不透明渲染器句柄
#[no_mangle]
pub unsafe extern "system" fn renderer_create(
    pixel_width: i32,
    pixel_height: i32,
    out_handle: *mut *mut Renderer,
) -> RendererStatus {
    if out_handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    if pixel_width <= 0 || pixel_height <= 0 {
        return RENDERER_ERR_INVALID_PARAM;
    }

    crate::log::clear_last_error();

    match Renderer::new(pixel_width as u32, pixel_height as u32) {
        Ok(r) => {
            *out_handle = Box::into_raw(Box::new(r));
            RENDERER_OK
        }
        Err(e) => {
            crate::log::emit(2, &format!("renderer_create failed: {}", e));
            e.to_status()
        }
    }
}

/// canvas 逻辑尺寸变化（显示器分辨率变了）。不重建 swap chain（swap chain 由 begin_frame
/// 按 viewport 大小自动 ResizeBuffers）。
#[no_mangle]
pub unsafe extern "system" fn renderer_resize(
    handle: *mut Renderer,
    pixel_width: i32,
    pixel_height: i32,
) -> RendererStatus {
    if handle.is_null() || pixel_width <= 0 || pixel_height <= 0 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let r = &*handle;
    match r.resize(pixel_width as u32, pixel_height as u32) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_resize failed: {}", e));
            e.to_status()
        }
    }
}

/// 销毁渲染器。
#[no_mangle]
pub unsafe extern "system" fn renderer_destroy(handle: *mut Renderer) {
    if handle.is_null() {
        return;
    }
    drop(Box::from_raw(handle));
}

/// 注册全局日志回调。
#[no_mangle]
pub unsafe extern "system" fn renderer_set_log_callback(
    cb: Option<LogCallbackFn>,
) -> RendererStatus {
    crate::log::set_callback(cb);
    RENDERER_OK
}

/// v0.6 DComp：拿 swap chain 的 IUnknown raw pointer（已 AddRef）。
///
/// C# 端用 `Marshal.GetObjectForIUnknown` 转 `IDXGISwapChain1`，再通过
/// `ICompositorInterop::CreateCompositionSurfaceForSwapChain` 包成 `ICompositionSurface`，
/// 挂到 `SpriteVisual.Brush` → `ElementCompositionPreview.SetElementChildVisual(rootGrid)`。
///
/// 调用方用完 IUnknown 必须调 `Marshal.Release` 一次（成对 AddRef）。
/// 渲染器自身仍持有 swap chain 引用，destroy 时自动释放。
#[no_mangle]
pub unsafe extern "system" fn renderer_get_swapchain(
    handle: *mut Renderer,
    out_iunknown: *mut *mut c_void,
) -> RendererStatus {
    if handle.is_null() || out_iunknown.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let r = &*handle;
    let raw = r.get_swapchain_iunknown();
    if raw.is_null() {
        return RENDERER_ERR_SWAPCHAIN_INIT;
    }
    *out_iunknown = raw;
    RENDERER_OK
}

// =====================================================================
// 命令式 Painter ABI
// =====================================================================
//
// 业务侧（任意语言）一帧三步走：
//   1. renderer_begin_frame(h, vx, vy, vw, vh)   — 内部 SetTarget + BeginDraw + SetTransform
//   2. renderer_clear / fill_rect / draw_text  ...0..N — 推绘制命令（按 canvas-space 坐标）
//   3. renderer_end_frame(h)                    — EndDraw + Present(0, 0)

/// 开始一帧。配对调用 `renderer_end_frame`。
///
/// v0.5 起 viewport-aware：业务侧告诉 renderer "本帧只关心 canvas 中 (vx, vy, vw, vh) 这块"。
/// 业务命令坐标系仍是 canvas-space；Rust 内部用 viewport 大小的 swap chain，
/// `SetTransform(translate(-vx, -vy))` 自动平移命令；超出 viewport 部分被 D2D clip。
///
/// 不可重入：连续两次 `begin_frame` 不调 `end_frame` 返 `INVALID_PARAM`。
#[no_mangle]
pub unsafe extern "system" fn renderer_begin_frame(
    handle: *mut Renderer,
    viewport_x: f32,
    viewport_y: f32,
    viewport_w: f32,
    viewport_h: f32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let r = &*handle;
    match r.begin_frame(viewport_x, viewport_y, viewport_w, viewport_h) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_begin_frame: {}", e));
            e.to_status()
        }
    }
}

/// 清屏到指定颜色。premultiplied alpha：rgb 必须 ≤ a。
#[no_mangle]
pub unsafe extern "system" fn renderer_clear(
    handle: *mut Renderer,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_clear([r, g, b, a]) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_clear: {}", e));
            e.to_status()
        }
    }
}

/// 实心矩形。坐标 = canvas-space pixel，premultiplied alpha 颜色。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_fill_rect(
    handle: *mut Renderer,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_fill_rect(x, y, w, h, [r, g, b, a]) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_fill_rect: {}", e));
            e.to_status()
        }
    }
}

/// 单行文本（Segoe UI / NORMAL）。坐标 = canvas-space。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_draw_text(
    handle: *mut Renderer,
    utf8: *const u8,
    utf8_len: i32,
    x: f32,
    y: f32,
    font_size: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> RendererStatus {
    if handle.is_null() || utf8_len < 0 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    let text: &str = if utf8_len == 0 {
        ""
    } else {
        if utf8.is_null() {
            return RENDERER_ERR_INVALID_PARAM;
        }
        let slice = std::slice::from_raw_parts(utf8, utf8_len as usize);
        match std::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => {
                crate::log::emit(4, "renderer_draw_text: invalid UTF-8 input");
                return RENDERER_ERR_INVALID_PARAM;
            }
        }
    };
    match renderer.cmd_draw_text(text, x, y, font_size, [r, g, b, a]) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_draw_text: {}", e));
            e.to_status()
        }
    }
}

/// v0.6 DComp 提交一帧 → EndDraw + Present(0, 0)。不返 mapped pointer。
///
/// DComp 自动拉 swap chain 新内容做合成。
/// 必须先 `begin_frame`，否则返 `INVALID_PARAM`。
#[no_mangle]
pub unsafe extern "system" fn renderer_end_frame(handle: *mut Renderer) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let r = &*handle;
    match r.end_frame() {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            let lvl = if matches!(e, crate::error::RendererError::FrameStillHeld) {
                3
            } else {
                4
            };
            crate::log::emit(lvl, &format!("renderer_end_frame: {}", e));
            e.to_status()
        }
    }
}

// =====================================================================
// v0.7 矢量图元 ABI（Phase 1）
// =====================================================================
//
// 全部在 begin_frame / end_frame 之间调用，否则返 INVALID_PARAM。
// 颜色 premultiplied alpha [0, 1]，坐标 canvas-space 像素。
// 实现细节见 painter.rs（DrawCmd enum + execute 派发，决策 spec 10.5）。

/// 直线。`stroke_width` 是 canvas-space 像素。
/// `dash_style`: 0=solid, 1=dash, 2=dot, 3=dash_dot；越界视为 solid。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_draw_line(
    handle: *mut Renderer,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    stroke_width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
    dash_style: i32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_draw_line(x0, y0, x1, y1, stroke_width, [r, g, b, a], dash_style) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_draw_line: {}", e));
            e.to_status()
        }
    }
}

/// 折线 / 闭合多边形。`points` 是连续 `[x0,y0,x1,y1,...]` 数组，`point_count` = 点数（不是 float 数）。
/// `closed != 0` 时首尾自动相接。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_draw_polyline(
    handle: *mut Renderer,
    points: *const f32,
    point_count: i32,
    stroke_width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
    closed: i32,
) -> RendererStatus {
    if handle.is_null() || point_count < 0 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    if point_count < 2 {
        // 0 / 1 个点画不出线段，但不视为错误（业务方可能传空数组），no-op
        return RENDERER_OK;
    }
    if points.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    let n = point_count as usize;
    let raw = std::slice::from_raw_parts(points, n * 2);
    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(n);
    for i in 0..n {
        pts.push((raw[i * 2], raw[i * 2 + 1]));
    }
    match renderer.cmd_draw_polyline(&pts, stroke_width, [r, g, b, a], closed != 0) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_draw_polyline: {}", e));
            e.to_status()
        }
    }
}

/// 矩形描边。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_stroke_rect(
    handle: *mut Renderer,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    stroke_width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_stroke_rect(x, y, w, h, stroke_width, [r, g, b, a]) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_stroke_rect: {}", e));
            e.to_status()
        }
    }
}

/// 圆角矩形填充。`radius_x` ≠ `radius_y` 时是椭圆角。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_fill_rounded_rect(
    handle: *mut Renderer,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius_x: f32,
    radius_y: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_fill_rounded_rect(x, y, w, h, radius_x, radius_y, [r, g, b, a]) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_fill_rounded_rect: {}", e));
            e.to_status()
        }
    }
}

/// 圆角矩形描边。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_stroke_rounded_rect(
    handle: *mut Renderer,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius_x: f32,
    radius_y: f32,
    stroke_width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_stroke_rounded_rect(
        x,
        y,
        w,
        h,
        radius_x,
        radius_y,
        stroke_width,
        [r, g, b, a],
    ) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_stroke_rounded_rect: {}", e));
            e.to_status()
        }
    }
}

/// 椭圆填充（含正圆，rx == ry 时）。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_fill_ellipse(
    handle: *mut Renderer,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_fill_ellipse(cx, cy, rx, ry, [r, g, b, a]) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_fill_ellipse: {}", e));
            e.to_status()
        }
    }
}

/// 椭圆描边。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_stroke_ellipse(
    handle: *mut Renderer,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    stroke_width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_stroke_ellipse(cx, cy, rx, ry, stroke_width, [r, g, b, a]) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_stroke_ellipse: {}", e));
            e.to_status()
        }
    }
}

/// 推矩形 clip。配对 `renderer_pop_clip` 使用，栈结构。
/// 当前实现走 `PushAxisAlignedClip` (ALIASED)，clip 边缘走整像素。
#[no_mangle]
pub unsafe extern "system" fn renderer_push_clip_rect(
    handle: *mut Renderer,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_push_clip_rect(x, y, w, h) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_push_clip_rect: {}", e));
            e.to_status()
        }
    }
}

/// 弹 clip 栈顶。栈空时由 D2D 处理（通常是无操作或日志警告）。
#[no_mangle]
pub unsafe extern "system" fn renderer_pop_clip(handle: *mut Renderer) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_pop_clip() {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_pop_clip: {}", e));
            e.to_status()
        }
    }
}

/// 设置 2D 仿射变换。`matrix` 是 6 个 float 的指针：`[m11, m12, m21, m22, dx, dy]`。
/// 等同 D2D Matrix3x2。`set_transform` 后所有命令叠加该变换；`reset_transform` 恢复成
/// viewport 平移（不是 identity —— begin_frame 内部已 SetTransform 了 viewport 平移）。
#[no_mangle]
pub unsafe extern "system" fn renderer_set_transform(
    handle: *mut Renderer,
    matrix: *const f32,
) -> RendererStatus {
    if handle.is_null() || matrix.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let raw = std::slice::from_raw_parts(matrix, 6);
    let m: [f32; 6] = [raw[0], raw[1], raw[2], raw[3], raw[4], raw[5]];
    let renderer = &*handle;
    match renderer.cmd_set_transform(m) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_set_transform: {}", e));
            e.to_status()
        }
    }
}

/// 重置 transform 为 viewport 平移（v0.6 默认状态）。
#[no_mangle]
pub unsafe extern "system" fn renderer_reset_transform(
    handle: *mut Renderer,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_reset_transform() {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_reset_transform: {}", e));
            e.to_status()
        }
    }
}

// =====================================================================
// 诊断 ABI
// =====================================================================

/// 拉取最近 N 帧（默认 N=60）的 perf 滑动统计。
#[no_mangle]
pub unsafe extern "system" fn renderer_get_perf_stats(
    handle: *mut Renderer,
    out_stats: *mut PerfStats,
) -> RendererStatus {
    if handle.is_null() || out_stats.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let r = &*handle;
    let stats = r.perf_stats();
    std::ptr::write(out_stats, stats);
    RENDERER_OK
}

/// 拷贝最近一条 ERROR 级别的日志到 buf。
#[no_mangle]
pub unsafe extern "system" fn renderer_last_error_string(
    buf: *mut u8,
    buf_len: usize,
) -> usize {
    let s = match crate::log::last_error_string() {
        Some(s) => s,
        None => {
            if !buf.is_null() && buf_len > 0 {
                *buf = 0;
            }
            return 0;
        }
    };
    let bytes = s.as_bytes();
    let needed = bytes.len();
    if buf.is_null() || buf_len == 0 {
        return needed;
    }
    let copy_len = std::cmp::min(bytes.len(), buf_len.saturating_sub(1));
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, copy_len);
    *buf.add(copy_len) = 0;
    needed
}

// 防意外删除 import 的占位
#[allow(dead_code)]
const _SUPPRESS_UNUSED_C_VOID: Option<*mut c_void> = None;

// ---------- 单元测试 ----------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_renderer(w: i32, h: i32) -> *mut Renderer {
        let mut handle: *mut Renderer = std::ptr::null_mut();
        let status = unsafe { renderer_create(w, h, &mut handle as *mut _) };
        assert_eq!(status, RENDERER_OK, "create should succeed on machine with D3D11 GPU");
        assert!(!handle.is_null());
        handle
    }

    /// v0.6 DComp 业务侧典型用法的 helper：begin → 一组命令 → end。
    fn run_cmd_frame(h: *mut Renderer, w: i32, h_: i32) {
        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, w as f32, h_ as f32) };
        assert_eq!(st, RENDERER_OK);
        let st = unsafe { renderer_clear(h, 0.0, 0.0, 0.05, 0.30) };
        assert_eq!(st, RENDERER_OK);
        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);
    }

    // ---------- 生命周期 ----------

    #[test]
    fn create_and_destroy_640_480() {
        let h = make_renderer(640, 480);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn resize_to_1280_720() {
        let h = make_renderer(640, 480);
        let status = unsafe { renderer_resize(h, 1280, 720) };
        assert_eq!(status, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn resize_to_zero_is_rejected() {
        let h = make_renderer(640, 480);
        let status = unsafe { renderer_resize(h, 0, 100) };
        assert_eq!(status, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn c_abi_null_handle_returns_invalid_param() {
        let mut handle: *mut Renderer = std::ptr::null_mut();
        let status = unsafe { renderer_create(-1, 100, &mut handle as *mut _) };
        assert_eq!(status, RENDERER_ERR_INVALID_PARAM);
        assert!(handle.is_null());
    }

    #[test]
    fn destroy_null_is_noop() {
        unsafe { renderer_destroy(std::ptr::null_mut()) };
    }

    #[test]
    fn set_log_callback_with_none_succeeds() {
        let status = unsafe { renderer_set_log_callback(None) };
        assert_eq!(status, RENDERER_OK);
    }

    #[test]
    fn get_swapchain_returns_non_null() {
        let h = make_renderer(640, 480);
        let mut iunk: *mut c_void = std::ptr::null_mut();
        let st = unsafe { renderer_get_swapchain(h, &mut iunk) };
        assert_eq!(st, RENDERER_OK);
        assert!(!iunk.is_null());
        // C# 拿到后会 Marshal.Release —— 这里测试也手动 Release 模拟
        unsafe {
            use windows::core::{IUnknown, Interface};
            let unk: IUnknown = IUnknown::from_raw(iunk);
            drop(unk);
        }
        unsafe { renderer_destroy(h) };
    }

    // ---------- 命令式 Painter ABI ----------

    #[test]
    fn cmd_mode_roundtrip_clear_and_text() {
        let h = make_renderer(640, 480);

        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0) };
        assert_eq!(st, RENDERER_OK);

        let st = unsafe { renderer_clear(h, 0.0, 0.0, 0.05, 0.30) };
        assert_eq!(st, RENDERER_OK);

        let st = unsafe { renderer_fill_rect(h, 10.0, 10.0, 100.0, 50.0, 0.2, 0.2, 0.6, 0.8) };
        assert_eq!(st, RENDERER_OK);

        let text = "Hello, Overlay! 渲染中";
        let st = unsafe {
            renderer_draw_text(
                h,
                text.as_ptr(),
                text.len() as i32,
                20.0,
                30.0,
                28.0,
                1.0,
                1.0,
                1.0,
                0.9,
            )
        };
        assert_eq!(st, RENDERER_OK);

        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);

        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn cmd_clear_before_begin_returns_invalid_param() {
        let h = make_renderer(640, 480);
        let st = unsafe { renderer_clear(h, 0.0, 0.0, 0.0, 0.0) };
        assert_eq!(st, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn end_frame_without_begin_returns_invalid_param() {
        let h = make_renderer(640, 480);
        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn double_begin_returns_invalid_param() {
        let h = make_renderer(640, 480);
        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0) };
        assert_eq!(st, RENDERER_OK);
        let st2 = unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0) };
        assert_eq!(st2, RENDERER_ERR_INVALID_PARAM);

        unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn cmd_mode_resize_in_middle_recovers() {
        let h = make_renderer(640, 480);
        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0) };
        assert_eq!(st, RENDERER_OK);
        let st2 = unsafe { renderer_clear(h, 0.0, 0.0, 0.0, 1.0) };
        assert_eq!(st2, RENDERER_OK);

        let r = unsafe { renderer_resize(h, 1280, 720) };
        assert_eq!(r, RENDERER_OK);

        // resize 后应能重新 begin_frame
        let st3 = unsafe { renderer_begin_frame(h, 0.0, 0.0, 1280.0, 720.0) };
        assert_eq!(st3, RENDERER_OK);
        let st4 = unsafe { renderer_end_frame(h) };
        assert_eq!(st4, RENDERER_OK);

        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn frame_index_monotonic_across_cmd_frames() {
        let h = make_renderer(640, 480);
        for _ in 1u64..=5 {
            run_cmd_frame(h, 640, 480);
        }
        let mut stats = std::mem::MaybeUninit::<PerfStats>::uninit();
        unsafe { renderer_get_perf_stats(h, stats.as_mut_ptr()) };
        let stats = unsafe { stats.assume_init() };
        assert_eq!(stats.total_frames, 5);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn perf_stats_after_some_cmd_frames() {
        let h = make_renderer(640, 480);
        for _ in 0..10 {
            run_cmd_frame(h, 640, 480);
        }
        let mut stats = std::mem::MaybeUninit::<PerfStats>::uninit();
        let status = unsafe { renderer_get_perf_stats(h, stats.as_mut_ptr()) };
        assert_eq!(status, RENDERER_OK);
        let stats = unsafe { stats.assume_init() };
        assert_eq!(stats.total_frames, 10);
        assert_eq!(stats.valid_samples, 10);
        assert_eq!(stats.window_size, 60);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn double_end_returns_invalid_param() {
        let h = make_renderer(640, 480);
        unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0) };
        unsafe { renderer_clear(h, 0.0, 0.0, 0.0, 1.0) };

        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);
        let st2 = unsafe { renderer_end_frame(h) };
        assert_eq!(st2, RENDERER_ERR_INVALID_PARAM);

        unsafe { renderer_destroy(h) };
    }

    // ---------- v0.5 viewport-aware（v0.6 swap chain 仍按 viewport size 重建） ----------

    #[test]
    fn viewport_smaller_than_canvas_runs() {
        let h = make_renderer(1280, 720);
        let st = unsafe { renderer_begin_frame(h, 200.0, 150.0, 800.0, 600.0) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_clear(h, 0.0, 0.0, 0.05, 0.30) };
        unsafe { renderer_fill_rect(h, 600.0, 350.0, 80.0, 80.0, 0.2, 0.4, 0.8, 0.9) };
        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn viewport_resize_across_frames_rebuilds_swap_chain() {
        let h = make_renderer(2560, 1440);
        let st = unsafe { renderer_begin_frame(h, 100.0, 100.0, 800.0, 600.0) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_clear(h, 0.0, 0.0, 0.0, 0.0) };
        unsafe { renderer_end_frame(h) };

        // 第二帧：viewport 变成 1024x768 → swap chain ResizeBuffers
        let st = unsafe { renderer_begin_frame(h, 50.0, 50.0, 1024.0, 768.0) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_clear(h, 0.0, 0.0, 0.0, 0.0) };
        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn viewport_partially_outside_canvas_ok() {
        let h = make_renderer(1920, 1080);
        let st = unsafe { renderer_begin_frame(h, -100.0, -50.0, 800.0, 600.0) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_clear(h, 0.0, 0.0, 0.0, 0.0) };
        unsafe { renderer_fill_rect(h, 0.0, 0.0, 200.0, 200.0, 0.5, 0.5, 0.5, 0.8) };
        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    // ---------- v0.7 矢量图元 ABI ----------
    //
    // 这层是 ABI 状态机校验：调用顺序、null 句柄、状态码。
    // 像素级 golden 比对延后到 Phase 2 加 readback API 后再做（v0.6 DComp 路径
    // 没有 CPU readback，单测无法直接看像素 —— 这是已知限制，spec 第 6 节风险表）。

    #[test]
    fn cmd_v07_full_roundtrip_in_one_frame() {
        // 一帧内连续调所有 11 个新命令 + 3 个老命令，全部应返 OK
        let h = make_renderer(800, 600);
        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, 800.0, 600.0) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_clear(h, 0.0, 0.0, 0.05, 1.0) };

        // 矢量图元
        let s = unsafe { renderer_draw_line(h, 10.0, 10.0, 100.0, 100.0, 2.0, 1.0, 0.5, 0.0, 1.0, 0) };
        assert_eq!(s, RENDERER_OK);

        let pts: [f32; 8] = [10.0, 200.0, 50.0, 240.0, 90.0, 200.0, 130.0, 260.0];
        let s = unsafe {
            renderer_draw_polyline(h, pts.as_ptr(), 4, 1.5, 0.2, 0.8, 0.4, 1.0, 0)
        };
        assert_eq!(s, RENDERER_OK);

        let s = unsafe { renderer_stroke_rect(h, 200.0, 50.0, 100.0, 60.0, 1.0, 1.0, 1.0, 1.0, 1.0) };
        assert_eq!(s, RENDERER_OK);

        let s = unsafe {
            renderer_fill_rounded_rect(h, 320.0, 50.0, 100.0, 60.0, 8.0, 8.0, 0.4, 0.4, 0.8, 0.9)
        };
        assert_eq!(s, RENDERER_OK);

        let s = unsafe {
            renderer_stroke_rounded_rect(
                h, 440.0, 50.0, 100.0, 60.0, 12.0, 12.0, 2.0, 0.9, 0.9, 0.3, 1.0,
            )
        };
        assert_eq!(s, RENDERER_OK);

        let s = unsafe { renderer_fill_ellipse(h, 600.0, 80.0, 40.0, 30.0, 0.8, 0.2, 0.2, 0.9) };
        assert_eq!(s, RENDERER_OK);

        let s = unsafe {
            renderer_stroke_ellipse(h, 700.0, 80.0, 40.0, 30.0, 1.5, 0.2, 0.8, 0.2, 1.0)
        };
        assert_eq!(s, RENDERER_OK);

        // 状态命令：clip 栈
        let s = unsafe { renderer_push_clip_rect(h, 50.0, 300.0, 400.0, 200.0) };
        assert_eq!(s, RENDERER_OK);
        unsafe { renderer_fill_rect(h, 0.0, 0.0, 800.0, 600.0, 0.0, 0.5, 0.5, 0.5) };
        let s = unsafe { renderer_pop_clip(h) };
        assert_eq!(s, RENDERER_OK);

        // 状态命令：transform
        let m: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 100.0, 100.0]; // pure translate
        let s = unsafe { renderer_set_transform(h, m.as_ptr()) };
        assert_eq!(s, RENDERER_OK);
        unsafe { renderer_fill_rect(h, 0.0, 0.0, 50.0, 50.0, 1.0, 1.0, 0.0, 1.0) };
        let s = unsafe { renderer_reset_transform(h) };
        assert_eq!(s, RENDERER_OK);

        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn cmd_v07_outside_begin_returns_invalid_param() {
        // 状态机：begin_frame 之外调任何 v0.7 命令都返 INVALID_PARAM
        let h = make_renderer(640, 480);

        let s = unsafe { renderer_draw_line(h, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe { renderer_stroke_rect(h, 0.0, 0.0, 10.0, 10.0, 1.0, 1.0, 1.0, 1.0, 1.0) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe {
            renderer_fill_rounded_rect(h, 0.0, 0.0, 10.0, 10.0, 2.0, 2.0, 1.0, 1.0, 1.0, 1.0)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe { renderer_fill_ellipse(h, 50.0, 50.0, 10.0, 10.0, 1.0, 1.0, 1.0, 1.0) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe { renderer_push_clip_rect(h, 0.0, 0.0, 10.0, 10.0) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe { renderer_pop_clip(h) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let m: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        let s = unsafe { renderer_set_transform(h, m.as_ptr()) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe { renderer_reset_transform(h) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn cmd_v07_null_handle_returns_invalid_param() {
        // null 句柄是协议错误，所有 ABI 都应快速失败
        let m: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

        let s = unsafe {
            renderer_draw_line(
                std::ptr::null_mut(),
                0.0,
                0.0,
                1.0,
                1.0,
                1.0,
                1.0,
                1.0,
                1.0,
                1.0,
                0,
            )
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe {
            renderer_set_transform(std::ptr::null_mut(), m.as_ptr())
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe { renderer_pop_clip(std::ptr::null_mut()) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
    }

    #[test]
    fn cmd_polyline_invalid_inputs() {
        // null pointer + 负 count + 单点都要安全处理
        let h = make_renderer(640, 480);
        unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0) };

        // null 数组
        let s = unsafe {
            renderer_draw_polyline(h, std::ptr::null(), 3, 1.0, 1.0, 1.0, 1.0, 1.0, 0)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        // 负点数
        let pts: [f32; 4] = [0.0, 0.0, 1.0, 1.0];
        let s = unsafe {
            renderer_draw_polyline(h, pts.as_ptr(), -1, 1.0, 1.0, 1.0, 1.0, 1.0, 0)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        // 单点：no-op，但返 OK（业务方可能传空数组场景）
        let s = unsafe {
            renderer_draw_polyline(h, pts.as_ptr(), 1, 1.0, 1.0, 1.0, 1.0, 1.0, 0)
        };
        assert_eq!(s, RENDERER_OK);

        // 0 点：no-op，OK
        let s = unsafe {
            renderer_draw_polyline(h, std::ptr::null(), 0, 1.0, 1.0, 1.0, 1.0, 1.0, 0)
        };
        assert_eq!(s, RENDERER_OK);

        unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn cmd_v07_clip_stack_push_pop_balance() {
        // 嵌套 clip：push push fill pop pop —— 验证栈可工作
        let h = make_renderer(800, 600);
        unsafe { renderer_begin_frame(h, 0.0, 0.0, 800.0, 600.0) };

        let s = unsafe { renderer_push_clip_rect(h, 50.0, 50.0, 400.0, 400.0) };
        assert_eq!(s, RENDERER_OK);
        let s = unsafe { renderer_push_clip_rect(h, 100.0, 100.0, 200.0, 200.0) };
        assert_eq!(s, RENDERER_OK);

        unsafe { renderer_fill_rect(h, 0.0, 0.0, 800.0, 600.0, 1.0, 0.0, 0.0, 1.0) };

        let s = unsafe { renderer_pop_clip(h) };
        assert_eq!(s, RENDERER_OK);
        let s = unsafe { renderer_pop_clip(h) };
        assert_eq!(s, RENDERER_OK);

        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn cmd_v07_transform_chained_operations() {
        // set → fill → reset → fill：验证 transform 作用域正确，reset 后回到 viewport-translate
        let h = make_renderer(800, 600);
        unsafe { renderer_begin_frame(h, 100.0, 100.0, 600.0, 400.0) };

        // 只平移 50,50
        let m: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 50.0, 50.0];
        let s = unsafe { renderer_set_transform(h, m.as_ptr()) };
        assert_eq!(s, RENDERER_OK);
        unsafe { renderer_fill_rect(h, 0.0, 0.0, 100.0, 100.0, 0.5, 0.5, 0.5, 1.0) };

        // reset → 恢复 viewport translate
        let s = unsafe { renderer_reset_transform(h) };
        assert_eq!(s, RENDERER_OK);
        unsafe { renderer_fill_rect(h, 200.0, 200.0, 50.0, 50.0, 1.0, 0.0, 0.0, 1.0) };

        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }
}
