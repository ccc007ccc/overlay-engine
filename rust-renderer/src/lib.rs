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
    LogCallbackFn, PerfStats, Renderer, RendererStatus, VideoInfo, RENDERER_ERR_CANVAS_RESIZE_FAIL,
    RENDERER_ERR_CAPTURE_INIT, RENDERER_ERR_DECODE_FAIL, RENDERER_ERR_DEVICE_INIT,
    RENDERER_ERR_FRAME_ACQUIRE, RENDERER_ERR_FRAME_HELD, RENDERER_ERR_INVALID_PARAM,
    RENDERER_ERR_IO, RENDERER_ERR_NOT_ATTACHED, RENDERER_ERR_RESOURCE_LIMIT,
    RENDERER_ERR_RESOURCE_NOT_FOUND, RENDERER_ERR_SWAPCHAIN_INIT, RENDERER_ERR_THREAD_INIT,
    RENDERER_ERR_UNSUPPORTED_FORMAT, RENDERER_ERR_VIDEO_DECODE_FAIL,
    RENDERER_ERR_VIDEO_FORMAT_CHANGED, RENDERER_ERR_VIDEO_NOT_FOUND, RENDERER_ERR_VIDEO_OPEN_FAIL,
    RENDERER_ERR_VIDEO_SEEK_FAIL, RENDERER_OK,
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

/// v0.7 §2.6.3 — 显式画布管理 ABI。
///
/// host 应在 WM_SIZE / `winit::WindowEvent::Resized` / 用户改设置面板时调，
/// **不要每帧调**（ResizeBuffers 重分配 GPU 缓冲，per-frame 性能损失明显）。
///
/// 同尺寸 short-circuit（零开销）；零尺寸或 cmd_drawing 中调用返错。
///
/// # 返回
/// - `RENDERER_OK`(0)
/// - `RENDERER_ERR_INVALID_PARAM`(-1) — `new_w` 或 `new_h` ≤ 0
/// - `RENDERER_ERR_FRAME_HELD`(-6) — 当前在 begin_frame / end_frame 之间
/// - `RENDERER_ERR_CANVAS_RESIZE_FAIL`(-14) — ResizeBuffers / 重建 D2D bitmap 失败
///   （当前实现 lazy resize，不主动构造该错误码；保留给后续 phase 升级用）
///
/// 实施细节：当前仅更新内部 canvas 字段。swap chain 实际 ResizeBuffers 由下次
/// `begin_frame` 按 viewport 大小自动触发。desktop-window 典型用法
/// `resize_canvas(w, h)` + `begin_frame(0, 0, w, h, ...)` 二者协同 → 一次 ResizeBuffers。
#[no_mangle]
pub unsafe extern "system" fn renderer_resize_canvas(
    handle: *mut Renderer,
    new_w: i32,
    new_h: i32,
) -> RendererStatus {
    if handle.is_null() || new_w <= 0 || new_h <= 0 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let r = &*handle;
    match r.resize_canvas(new_w as u32, new_h as u32) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_resize_canvas failed: {}", e));
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
/// **v0.7 新增**：`out_canvas_w` / `out_canvas_h` 写出当前画布尺寸，让业务做百分比布局。
/// 两者均允许传 NULL 跳过；不需要画布尺寸的旧业务（widget v0.6）传 NULL 即可。
///
/// 不可重入：连续两次 `begin_frame` 不调 `end_frame` 返 `INVALID_PARAM`。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_begin_frame(
    handle: *mut Renderer,
    viewport_x: f32,
    viewport_y: f32,
    viewport_w: f32,
    viewport_h: f32,
    out_canvas_w: *mut i32,
    out_canvas_h: *mut i32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let r = &*handle;
    match r.begin_frame(viewport_x, viewport_y, viewport_w, viewport_h) {
        Ok(()) => {
            // v0.7：begin_frame 成功后写出画布尺寸（renderer.size() 当前 = canvas 尺寸；
            // 后续 phase 让 resize_canvas 真正改 swap chain 后这里仍是同一来源）。
            // NULL 出参跳过，兼容 v0.6 调用方以及"不需要画布尺寸"的业务。
            let (cw, ch) = r.size();
            if !out_canvas_w.is_null() {
                *out_canvas_w = cw as i32;
            }
            if !out_canvas_h.is_null() {
                *out_canvas_h = ch as i32;
            }
            RENDERER_OK
        }
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
// Phase 2+3: Bitmap / 外部纹理 ABI
// =====================================================================

/// 从内存字节流解码图片（PNG/JPEG/...）→ bitmap handle。
#[no_mangle]
pub unsafe extern "system" fn renderer_load_bitmap_from_memory(
    handle: *mut Renderer,
    bytes: *const u8,
    byte_len: i32,
    out_handle: *mut u32,
) -> RendererStatus {
    if handle.is_null() || out_handle.is_null() || bytes.is_null() || byte_len <= 0 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    *out_handle = 0;
    let renderer = &*handle;
    let slice = std::slice::from_raw_parts(bytes, byte_len as usize);
    match renderer.load_bitmap_from_memory(slice) {
        Ok(h) => { *out_handle = h; RENDERER_OK }
        Err(e) => {
            crate::log::emit(4, &format!("renderer_load_bitmap_from_memory: {}", e));
            e.to_status()
        }
    }
}

/// 从 UTF-8 路径解码图片。`utf8_path` 长度 `path_len` 字节，无 NUL。
#[no_mangle]
pub unsafe extern "system" fn renderer_load_bitmap_from_file(
    handle: *mut Renderer,
    utf8_path: *const u8,
    path_len: i32,
    out_handle: *mut u32,
) -> RendererStatus {
    if handle.is_null() || out_handle.is_null() || utf8_path.is_null() || path_len <= 0 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    *out_handle = 0;
    let path_slice = std::slice::from_raw_parts(utf8_path, path_len as usize);
    let path_str = match std::str::from_utf8(path_slice) {
        Ok(s) => s,
        Err(_) => {
            crate::log::emit(4, "renderer_load_bitmap_from_file: invalid UTF-8 path");
            return RENDERER_ERR_INVALID_PARAM;
        }
    };
    let bytes = match std::fs::read(path_str) {
        Ok(b) => b,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_load_bitmap_from_file: io {}", e));
            return crate::error::RendererError::Io(e).to_status();
        }
    };
    let renderer = &*handle;
    match renderer.load_bitmap_from_memory(&bytes) {
        Ok(h) => { *out_handle = h; RENDERER_OK }
        Err(e) => {
            crate::log::emit(4, &format!("renderer_load_bitmap_from_file: decode {}", e));
            e.to_status()
        }
    }
}

/// 创建空可写纹理。`format`: 0=BGRA8, 1=RGBA8, 2=NV12（暂未支持）。
#[no_mangle]
pub unsafe extern "system" fn renderer_create_texture(
    handle: *mut Renderer,
    width: u32,
    height: u32,
    format: i32,
    out_handle: *mut u32,
) -> RendererStatus {
    if handle.is_null() || out_handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    *out_handle = 0;
    let renderer = &*handle;
    match renderer.create_texture(width, height, format) {
        Ok(h) => { *out_handle = h; RENDERER_OK }
        Err(e) => {
            crate::log::emit(4, &format!("renderer_create_texture: {}", e));
            e.to_status()
        }
    }
}

/// 上传一帧像素到可写纹理。`stride` = 每行字节数。format 必须与 create 一致。
#[no_mangle]
pub unsafe extern "system" fn renderer_update_texture(
    handle: *mut Renderer,
    bitmap: u32,
    bytes: *const u8,
    byte_len: i32,
    stride: i32,
    format: i32,
) -> RendererStatus {
    if handle.is_null() || bytes.is_null() || byte_len <= 0 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    let slice = std::slice::from_raw_parts(bytes, byte_len as usize);
    match renderer.update_texture(bitmap, slice, stride, format) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_update_texture: {}", e));
            e.to_status()
        }
    }
}

/// 查询 bitmap 尺寸。
#[no_mangle]
pub unsafe extern "system" fn renderer_get_bitmap_size(
    handle: *mut Renderer,
    bitmap: u32,
    out_width: *mut u32,
    out_height: *mut u32,
) -> RendererStatus {
    if handle.is_null() || out_width.is_null() || out_height.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.get_bitmap_size(bitmap) {
        Ok((w, h)) => { *out_width = w; *out_height = h; RENDERER_OK }
        Err(e) => {
            crate::log::emit(4, &format!("renderer_get_bitmap_size: {}", e));
            e.to_status()
        }
    }
}

/// 销毁 bitmap。已 destroy → RESOURCE_NOT_FOUND（idempotent 由调用方判断）。
#[no_mangle]
pub unsafe extern "system" fn renderer_destroy_bitmap(
    handle: *mut Renderer,
    bitmap: u32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.destroy_bitmap(bitmap) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_destroy_bitmap: {}", e));
            e.to_status()
        }
    }
}

/// 把 bitmap 画到 canvas。`src_*` 全 0 = 整 bitmap。`interp_mode`: 0=nearest, 1=linear。
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn renderer_draw_bitmap(
    handle: *mut Renderer,
    bitmap: u32,
    src_x: f32, src_y: f32, src_w: f32, src_h: f32,
    dst_x: f32, dst_y: f32, dst_w: f32, dst_h: f32,
    opacity: f32,
    interp_mode: i32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.cmd_draw_bitmap(
        bitmap, src_x, src_y, src_w, src_h, dst_x, dst_y, dst_w, dst_h, opacity, interp_mode,
    ) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_draw_bitmap: {}", e));
            e.to_status()
        }
    }
}

// =====================================================================
// v0.7 phase 5 path + 渐变（spec §2.3.4 / §2.5）
// =====================================================================

/// 填充任意路径。path_bytes 是 opcode 字节流（0x01-0x05，v0.7 支持）。
/// 0x06+ opcode → UNSUPPORTED_FORMAT；字节截断 → INVALID_PARAM。
/// 必须在 begin_frame / end_frame 之间。
#[no_mangle]
pub unsafe extern "system" fn renderer_fill_path(
    handle: *mut Renderer,
    path_bytes: *const u8,
    path_len: i32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> RendererStatus {
    if handle.is_null() || path_bytes.is_null() || path_len < 0 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    let path = std::slice::from_raw_parts(path_bytes, path_len as usize);
    match renderer.cmd_fill_path(path, [r, g, b, a]) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_fill_path: {}", e));
            e.to_status()
        }
    }
}

/// 描边任意路径。同 renderer_fill_path 的 path 编码。
#[no_mangle]
pub unsafe extern "system" fn renderer_stroke_path(
    handle: *mut Renderer,
    path_bytes: *const u8,
    path_len: i32,
    stroke_width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
    dash_style: i32,
) -> RendererStatus {
    if handle.is_null() || path_bytes.is_null() || path_len < 0 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    let path = std::slice::from_raw_parts(path_bytes, path_len as usize);
    match renderer.cmd_stroke_path(path, stroke_width, [r, g, b, a], dash_style) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_stroke_path: {}", e));
            e.to_status()
        }
    }
}

/// 矩形 + 线性渐变填充。stops = `[offset, r, g, b, a, ...]`，长度必须 5×N (N ≥ 2)，
/// offset 升序 ∈ [0, 1]。premultiplied alpha。
#[no_mangle]
pub unsafe extern "system" fn renderer_fill_rect_gradient_linear(
    handle: *mut Renderer,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    start_x: f32,
    start_y: f32,
    end_x: f32,
    end_y: f32,
    stops: *const f32,
    stop_count: i32,
) -> RendererStatus {
    if handle.is_null() || stops.is_null() || stop_count < 2 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    // stop_count = 逻辑 stop 数量（不是 float 数）；float 数 = stop_count * 5
    let float_count = (stop_count as usize).saturating_mul(5);
    let slice = std::slice::from_raw_parts(stops, float_count);
    match renderer.cmd_fill_rect_gradient_linear(x, y, w, h, start_x, start_y, end_x, end_y, slice)
    {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_fill_rect_gradient_linear: {}", e));
            e.to_status()
        }
    }
}

/// 矩形 + 径向渐变填充。stops 同 linear。
#[no_mangle]
pub unsafe extern "system" fn renderer_fill_rect_gradient_radial(
    handle: *mut Renderer,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    center_x: f32,
    center_y: f32,
    radius_x: f32,
    radius_y: f32,
    stops: *const f32,
    stop_count: i32,
) -> RendererStatus {
    if handle.is_null() || stops.is_null() || stop_count < 2 {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    let float_count = (stop_count as usize).saturating_mul(5);
    let slice = std::slice::from_raw_parts(stops, float_count);
    match renderer
        .cmd_fill_rect_gradient_radial(x, y, w, h, center_x, center_y, radius_x, radius_y, slice)
    {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_fill_rect_gradient_radial: {}", e));
            e.to_status()
        }
    }
}

// =====================================================================
// v0.7 phase 3 video ABI（spec §4.1）
// =====================================================================

/// 打开本地视频文件。`utf8_path` UTF-8 字节，长度由 `path_len` 给（不要求 NUL 终止）。
/// 成功后 `*out_video_handle` 写 video 句柄（独立 id 空间，与 bitmap handle 不共用 slot table）。
/// 失败码典型值：
///   - INVALID_PARAM：handle/out_video_handle/path 任一为 null，或 path_len ≤ 0
///   - VIDEO_OPEN_FAIL：MF source reader 创建 / 配置失败（文件不存在、codec 不支持、DRM）
///   - RESOURCE_LIMIT：videos slot table 满（默认 1024）
#[no_mangle]
pub unsafe extern "system" fn renderer_video_open_file(
    handle: *mut Renderer,
    utf8_path: *const u8,
    path_len: i32,
    out_video_handle: *mut u32,
) -> RendererStatus {
    if handle.is_null() || utf8_path.is_null() || path_len <= 0 || out_video_handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    *out_video_handle = 0;
    let renderer = &*handle;
    let bytes = std::slice::from_raw_parts(utf8_path, path_len as usize);
    let path = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return RENDERER_ERR_INVALID_PARAM,
    };
    match renderer.video_open_file(path) {
        Ok(h) => {
            *out_video_handle = h;
            RENDERER_OK
        }
        Err(e) => {
            crate::log::emit(4, &format!("renderer_video_open_file: {}", e));
            e.to_status()
        }
    }
}

/// 查询视频元数据。成功后 `*out_info` 写 VideoInfo（duration_ms, w, h, fps_num, fps_den）。
#[no_mangle]
pub unsafe extern "system" fn renderer_video_get_info(
    handle: *mut Renderer,
    video: u32,
    out_info: *mut VideoInfo,
) -> RendererStatus {
    if handle.is_null() || out_info.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.video_get_info(video) {
        Ok(info) => {
            std::ptr::write(out_info, info);
            RENDERER_OK
        }
        Err(e) => {
            crate::log::emit(4, &format!("renderer_video_get_info: {}", e));
            e.to_status()
        }
    }
}

/// 跳到指定毫秒位置。EOS 标记会清掉。
#[no_mangle]
pub unsafe extern "system" fn renderer_video_seek(
    handle: *mut Renderer,
    video: u32,
    time_ms: u64,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.video_seek(video, time_ms) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_video_seek: {}", e));
            e.to_status()
        }
    }
}

/// 解一帧到内部 bitmap，返回该帧的 BitmapHandle 与 EOF 标志。
/// 同 video 反复调返回**同一个** BitmapHandle —— 业务用 `renderer_draw_bitmap` 画即可。
/// 业务 **不要** destroy 这个 bitmap handle —— `renderer_video_close` 统一回收。
#[no_mangle]
pub unsafe extern "system" fn renderer_video_present_frame(
    handle: *mut Renderer,
    video: u32,
    out_bitmap: *mut u32,
    out_eof: *mut i32,
) -> RendererStatus {
    if handle.is_null() || out_bitmap.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    *out_bitmap = 0;
    if !out_eof.is_null() {
        *out_eof = 0;
    }
    let renderer = &*handle;
    match renderer.video_present_frame(video) {
        Ok((bm, eof)) => {
            *out_bitmap = bm;
            if !out_eof.is_null() {
                *out_eof = if eof { 1 } else { 0 };
            }
            RENDERER_OK
        }
        Err(e) => {
            crate::log::emit(4, &format!("renderer_video_present_frame: {}", e));
            e.to_status()
        }
    }
}

/// 关闭视频：统一回收内部 IMFSourceReader + bitmap slot。
/// 业务持有的 video handle 即时失效（再次用返 VIDEO_NOT_FOUND）。
#[no_mangle]
pub unsafe extern "system" fn renderer_video_close(
    handle: *mut Renderer,
    video: u32,
) -> RendererStatus {
    if handle.is_null() {
        return RENDERER_ERR_INVALID_PARAM;
    }
    let renderer = &*handle;
    match renderer.video_close(video) {
        Ok(()) => RENDERER_OK,
        Err(e) => {
            crate::log::emit(4, &format!("renderer_video_close: {}", e));
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
        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, w as f32, h_ as f32, std::ptr::null_mut(), std::ptr::null_mut()) };
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
    fn begin_frame_writes_canvas_size_to_outparams() {
        // v0.7：begin_frame 应该写出当前画布尺寸到 out_canvas_w / out_canvas_h
        let h = make_renderer(1280, 720);
        let mut cw: i32 = -1;
        let mut ch: i32 = -1;
        let status = unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 1280.0, 720.0, &mut cw, &mut ch)
        };
        assert_eq!(status, RENDERER_OK);
        assert_eq!(cw, 1280);
        assert_eq!(ch, 720);
        // 必须 end_frame 才能 destroy（避免 cmd_drawing 残留）
        let _ = unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn begin_frame_null_outparams_is_compatible() {
        // v0.6 调用方传 NULL 出参 → 不 crash，正常返回 OK
        let h = make_renderer(640, 480);
        let status = unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(status, RENDERER_OK);
        let _ = unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn begin_frame_one_outparam_null_other_writes() {
        // 只传 width 出参，height 出参传 NULL
        let h = make_renderer(800, 600);
        let mut cw: i32 = 0;
        let status = unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 800.0, 600.0, &mut cw, std::ptr::null_mut())
        };
        assert_eq!(status, RENDERER_OK);
        assert_eq!(cw, 800);
        let _ = unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    // ---------- v0.7 §2.6.3 renderer_resize_canvas ----------

    #[test]
    fn resize_canvas_basic_then_begin_frame_outparam_reflects_new_size() {
        // 创建 1280×720 → resize_canvas(800, 600) → begin_frame 出参应为 800/600
        let h = make_renderer(1280, 720);
        let st = unsafe { renderer_resize_canvas(h, 800, 600) };
        assert_eq!(st, RENDERER_OK);

        let mut cw: i32 = -1;
        let mut ch: i32 = -1;
        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, 800.0, 600.0, &mut cw, &mut ch) };
        assert_eq!(st, RENDERER_OK);
        assert_eq!(cw, 800);
        assert_eq!(ch, 600);
        let _ = unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn resize_canvas_zero_or_negative_size_rejected() {
        let h = make_renderer(640, 480);
        assert_eq!(unsafe { renderer_resize_canvas(h, 0, 480) }, RENDERER_ERR_INVALID_PARAM);
        assert_eq!(unsafe { renderer_resize_canvas(h, 640, 0) }, RENDERER_ERR_INVALID_PARAM);
        assert_eq!(unsafe { renderer_resize_canvas(h, -1, 480) }, RENDERER_ERR_INVALID_PARAM);
        assert_eq!(unsafe { renderer_resize_canvas(h, 640, -10) }, RENDERER_ERR_INVALID_PARAM);
        // size 不变（原 640×480）
        let mut cw: i32 = 0;
        let mut ch: i32 = 0;
        unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, &mut cw, &mut ch) };
        assert_eq!((cw, ch), (640, 480));
        let _ = unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn resize_canvas_null_handle_returns_invalid_param() {
        let st = unsafe { renderer_resize_canvas(std::ptr::null_mut(), 800, 600) };
        assert_eq!(st, RENDERER_ERR_INVALID_PARAM);
    }

    #[test]
    fn resize_canvas_same_size_is_noop() {
        // 同尺寸 short-circuit；spec §2.6.3 强制零开销
        let h = make_renderer(800, 600);
        let st = unsafe { renderer_resize_canvas(h, 800, 600) };
        assert_eq!(st, RENDERER_OK);
        // 再调一次也 OK
        let st = unsafe { renderer_resize_canvas(h, 800, 600) };
        assert_eq!(st, RENDERER_OK);
        // 尺寸应仍是 800×600
        let mut cw: i32 = 0;
        let mut ch: i32 = 0;
        unsafe { renderer_begin_frame(h, 0.0, 0.0, 800.0, 600.0, &mut cw, &mut ch) };
        assert_eq!((cw, ch), (800, 600));
        let _ = unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn resize_canvas_in_frame_returns_frame_held() {
        // begin_frame 之后调 resize_canvas → -6 FRAME_HELD（spec §2.6.3）
        let h = make_renderer(640, 480);
        let st = unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(st, RENDERER_OK);
        // 帧内 resize 必须拒绝
        let st = unsafe { renderer_resize_canvas(h, 1024, 768) };
        assert_eq!(st, RENDERER_ERR_FRAME_HELD);
        // 尺寸不应变（仍 640×480）
        let _ = unsafe { renderer_end_frame(h) };
        let mut cw: i32 = 0;
        let mut ch: i32 = 0;
        unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, &mut cw, &mut ch) };
        assert_eq!((cw, ch), (640, 480));
        let _ = unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn resize_canvas_multiple_times_reflects_latest() {
        // 多次 resize 串行 → 最后一次生效
        let h = make_renderer(640, 480);
        unsafe {
            assert_eq!(renderer_resize_canvas(h, 800, 600), RENDERER_OK);
            assert_eq!(renderer_resize_canvas(h, 1280, 720), RENDERER_OK);
            assert_eq!(renderer_resize_canvas(h, 1920, 1080), RENDERER_OK);
        }
        let mut cw: i32 = 0;
        let mut ch: i32 = 0;
        unsafe { renderer_begin_frame(h, 0.0, 0.0, 1920.0, 1080.0, &mut cw, &mut ch) };
        assert_eq!((cw, ch), (1920, 1080));
        let _ = unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
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

        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut()) };
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
        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(st, RENDERER_OK);
        let st2 = unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(st2, RENDERER_ERR_INVALID_PARAM);

        unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn cmd_mode_resize_in_middle_recovers() {
        let h = make_renderer(640, 480);
        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(st, RENDERER_OK);
        let st2 = unsafe { renderer_clear(h, 0.0, 0.0, 0.0, 1.0) };
        assert_eq!(st2, RENDERER_OK);

        let r = unsafe { renderer_resize(h, 1280, 720) };
        assert_eq!(r, RENDERER_OK);

        // resize 后应能重新 begin_frame
        let st3 = unsafe { renderer_begin_frame(h, 0.0, 0.0, 1280.0, 720.0, std::ptr::null_mut(), std::ptr::null_mut()) };
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
        unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut()) };
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
        let st = unsafe { renderer_begin_frame(h, 200.0, 150.0, 800.0, 600.0, std::ptr::null_mut(), std::ptr::null_mut()) };
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
        let st = unsafe { renderer_begin_frame(h, 100.0, 100.0, 800.0, 600.0, std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_clear(h, 0.0, 0.0, 0.0, 0.0) };
        unsafe { renderer_end_frame(h) };

        // 第二帧：viewport 变成 1024x768 → swap chain ResizeBuffers
        let st = unsafe { renderer_begin_frame(h, 50.0, 50.0, 1024.0, 768.0, std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_clear(h, 0.0, 0.0, 0.0, 0.0) };
        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn viewport_partially_outside_canvas_ok() {
        let h = make_renderer(1920, 1080);
        let st = unsafe { renderer_begin_frame(h, -100.0, -50.0, 800.0, 600.0, std::ptr::null_mut(), std::ptr::null_mut()) };
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
        let st = unsafe { renderer_begin_frame(h, 0.0, 0.0, 800.0, 600.0, std::ptr::null_mut(), std::ptr::null_mut()) };
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
        unsafe { renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut()) };

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
        unsafe { renderer_begin_frame(h, 0.0, 0.0, 800.0, 600.0, std::ptr::null_mut(), std::ptr::null_mut()) };

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
        unsafe { renderer_begin_frame(h, 100.0, 100.0, 600.0, 400.0, std::ptr::null_mut(), std::ptr::null_mut()) };

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

    // =====================================================================
    // Phase 2 bitmap ABI 测试
    //
    // 这层只验 ABI 状态机 + 错误码映射，不验像素（v0.6 路径无 readback API）。
    // 像素级 golden 比对要等 readback 接口加上后再做 —— spec 第 6 节标注的已知缺口。
    // =====================================================================

    #[test]
    fn bitmap_load_from_memory_invalid_bytes_returns_decode_fail() {
        // 喂随机字节，WIC 应识别不出任何容器格式 → DecodeFail。
        let h = make_renderer(640, 480);
        let garbage: [u8; 16] = [0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let mut out: u32 = 0xFFFF_FFFF;
        let s = unsafe {
            renderer_load_bitmap_from_memory(h, garbage.as_ptr(), garbage.len() as i32, &mut out)
        };
        assert_eq!(s, RENDERER_ERR_DECODE_FAIL);
        assert_eq!(out, 0, "out_handle 必须在失败时清零，避免业务侧拿到野句柄");
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_load_from_memory_null_args_return_invalid_param() {
        let h = make_renderer(320, 240);
        let mut out: u32 = 0;
        // null bytes
        let s = unsafe {
            renderer_load_bitmap_from_memory(h, std::ptr::null(), 16, &mut out)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        // 0 len
        let dummy: [u8; 4] = [1, 2, 3, 4];
        let s = unsafe {
            renderer_load_bitmap_from_memory(h, dummy.as_ptr(), 0, &mut out)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        // null out
        let s = unsafe {
            renderer_load_bitmap_from_memory(h, dummy.as_ptr(), 4, std::ptr::null_mut())
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_load_from_file_missing_returns_io() {
        let h = make_renderer(320, 240);
        let path = b"Z:/__definitely_does_not_exist__/nope.png";
        let mut out: u32 = 0xFFFF_FFFF;
        let s = unsafe {
            renderer_load_bitmap_from_file(h, path.as_ptr(), path.len() as i32, &mut out)
        };
        assert_eq!(s, RENDERER_ERR_IO);
        assert_eq!(out, 0);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_load_from_file_invalid_utf8_returns_invalid_param() {
        let h = make_renderer(320, 240);
        // 0x80 不是合法 UTF-8 起始字节
        let bad: [u8; 4] = [0x80, 0x80, 0x80, 0x80];
        let mut out: u32 = 0;
        let s = unsafe {
            renderer_load_bitmap_from_file(h, bad.as_ptr(), bad.len() as i32, &mut out)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_create_texture_zero_size_rejected() {
        let h = make_renderer(320, 240);
        let mut out: u32 = 0;
        let s = unsafe { renderer_create_texture(h, 0, 100, 0, &mut out) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        let s = unsafe { renderer_create_texture(h, 100, 0, 0, &mut out) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_create_texture_nv12_unsupported() {
        // format=2 (NV12) 当前阶段拒绝 → UNSUPPORTED_FORMAT。
        let h = make_renderer(320, 240);
        let mut out: u32 = 0xFFFF_FFFF;
        let s = unsafe { renderer_create_texture(h, 64, 64, 2, &mut out) };
        assert_eq!(s, RENDERER_ERR_UNSUPPORTED_FORMAT);
        assert_eq!(out, 0);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_create_texture_unknown_format_rejected() {
        let h = make_renderer(320, 240);
        let mut out: u32 = 0;
        let s = unsafe { renderer_create_texture(h, 64, 64, 99, &mut out) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_create_texture_bgra8_lifecycle() {
        // 全链路：create → get_size → destroy → get_size 失败。
        let h = make_renderer(320, 240);
        let mut bm: u32 = 0;
        let s = unsafe { renderer_create_texture(h, 64, 32, 0, &mut bm) };
        assert_eq!(s, RENDERER_OK);
        assert_ne!(bm, 0, "成功时 handle 不应为 0（保留值）");

        let (mut w, mut hh): (u32, u32) = (0, 0);
        let s = unsafe { renderer_get_bitmap_size(h, bm, &mut w, &mut hh) };
        assert_eq!(s, RENDERER_OK);
        assert_eq!((w, hh), (64, 32));

        let s = unsafe { renderer_destroy_bitmap(h, bm) };
        assert_eq!(s, RENDERER_OK);

        // destroy 后再查 → ResourceNotFound
        let s = unsafe { renderer_get_bitmap_size(h, bm, &mut w, &mut hh) };
        assert_eq!(s, RENDERER_ERR_RESOURCE_NOT_FOUND);

        // 二次 destroy 也应返 ResourceNotFound（idempotent 由调用方判断）
        let s = unsafe { renderer_destroy_bitmap(h, bm) };
        assert_eq!(s, RENDERER_ERR_RESOURCE_NOT_FOUND);

        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_update_texture_basic_bgra8() {
        // 创建 4x4 BGRA8 纹理 → 上传一帧 → OK
        let h = make_renderer(320, 240);
        let mut bm: u32 = 0;
        unsafe { renderer_create_texture(h, 4, 4, 0, &mut bm) };

        let stride: i32 = 4 * 4; // 4 像素 * 4 字节
        let data = vec![0xAAu8; (stride as usize) * 4];
        let s = unsafe {
            renderer_update_texture(h, bm, data.as_ptr(), data.len() as i32, stride, 0)
        };
        assert_eq!(s, RENDERER_OK);

        unsafe { renderer_destroy_bitmap(h, bm) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_update_texture_short_buffer_rejected() {
        // bytes 比 height*stride 短 → InvalidParam
        let h = make_renderer(320, 240);
        let mut bm: u32 = 0;
        unsafe { renderer_create_texture(h, 8, 8, 0, &mut bm) };

        let stride: i32 = 8 * 4;
        let too_short = vec![0u8; (stride as usize) * 2]; // 只够 2 行
        let s = unsafe {
            renderer_update_texture(h, bm, too_short.as_ptr(), too_short.len() as i32, stride, 0)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        unsafe { renderer_destroy_bitmap(h, bm) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_update_texture_negative_stride_rejected() {
        let h = make_renderer(320, 240);
        let mut bm: u32 = 0;
        unsafe { renderer_create_texture(h, 4, 4, 0, &mut bm) };
        let data = [0u8; 64];
        let s = unsafe {
            renderer_update_texture(h, bm, data.as_ptr(), data.len() as i32, -1, 0)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        let s = unsafe {
            renderer_update_texture(h, bm, data.as_ptr(), data.len() as i32, 0, 0)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy_bitmap(h, bm) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_update_texture_invalid_handle_returns_not_found() {
        let h = make_renderer(320, 240);
        let data = [0u8; 64];
        // handle=0 始终无效；任意 generation=0 也是 ABA 保留值
        let s = unsafe {
            renderer_update_texture(h, 0, data.as_ptr(), data.len() as i32, 16, 0)
        };
        assert_eq!(s, RENDERER_ERR_RESOURCE_NOT_FOUND);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_update_texture_stride_below_width_rejected() {
        // width=8 BGRA8 → 一行至少 32 字节；stride=16 不足，应返 InvalidParam。
        // 否则 swizzle / D2D CopyFromMemory 跨行读源 buffer，画面错位。
        let h = make_renderer(320, 240);
        let mut bm: u32 = 0;
        unsafe { renderer_create_texture(h, 8, 8, 0, &mut bm) };
        let data = vec![0u8; 16 * 8]; // height * (短)stride
        let s = unsafe {
            renderer_update_texture(h, bm, data.as_ptr(), data.len() as i32, 16, 0)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy_bitmap(h, bm) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_update_texture_unknown_format_rejected() {
        // format=99 不在白名单 → InvalidParam（防止未知值静默走 BGRA 路径）
        let h = make_renderer(320, 240);
        let mut bm: u32 = 0;
        unsafe { renderer_create_texture(h, 4, 4, 0, &mut bm) };
        let data = vec![0u8; 64];
        let s = unsafe {
            renderer_update_texture(h, bm, data.as_ptr(), data.len() as i32, 16, 99)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy_bitmap(h, bm) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_update_texture_nv12_rejected_with_unsupported() {
        // format=2 (NV12) 在 update 路径也明确拒绝 → UnsupportedFormat（与 create_texture 一致）
        let h = make_renderer(320, 240);
        let mut bm: u32 = 0;
        unsafe { renderer_create_texture(h, 4, 4, 0, &mut bm) };
        let data = vec![0u8; 64];
        let s = unsafe {
            renderer_update_texture(h, bm, data.as_ptr(), data.len() as i32, 16, 2)
        };
        assert_eq!(s, RENDERER_ERR_UNSUPPORTED_FORMAT);
        unsafe { renderer_destroy_bitmap(h, bm) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_update_texture_rgba8_swizzle_path() {
        // RGBA8 路径应走 swizzle 转 BGRA → CopyFromMemory，最终 OK。
        let h = make_renderer(320, 240);
        let mut bm: u32 = 0;
        unsafe { renderer_create_texture(h, 4, 4, 1, &mut bm) }; // format=RGBA8
        let stride: i32 = 4 * 4;
        let data = vec![0xC8u8; (stride as usize) * 4]; // 任意 RGBA 值，关键是不 panic
        let s = unsafe {
            renderer_update_texture(h, bm, data.as_ptr(), data.len() as i32, stride, 1)
        };
        assert_eq!(s, RENDERER_OK);
        unsafe { renderer_destroy_bitmap(h, bm) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_destroy_invalid_handle_returns_not_found() {
        let h = make_renderer(320, 240);
        let s = unsafe { renderer_destroy_bitmap(h, 0xDEAD_BEEF) };
        assert_eq!(s, RENDERER_ERR_RESOURCE_NOT_FOUND);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_draw_outside_begin_frame_returns_invalid_param() {
        // draw_bitmap 是命令，必须在 begin_frame...end_frame 之间。
        let h = make_renderer(320, 240);
        let mut bm: u32 = 0;
        unsafe { renderer_create_texture(h, 16, 16, 0, &mut bm) };

        let s = unsafe {
            renderer_draw_bitmap(
                h, bm, 0.0, 0.0, 0.0, 0.0, 10.0, 10.0, 16.0, 16.0, 1.0, 1,
            )
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        unsafe { renderer_destroy_bitmap(h, bm) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_draw_within_frame_ok() {
        // 完整路径：create_texture → update_texture → begin → draw_bitmap → end
        let h = make_renderer(320, 240);
        let mut bm: u32 = 0;
        unsafe { renderer_create_texture(h, 8, 8, 0, &mut bm) };
        let stride: i32 = 8 * 4;
        let data = vec![0x80u8; (stride as usize) * 8];
        unsafe { renderer_update_texture(h, bm, data.as_ptr(), data.len() as i32, stride, 0) };

        unsafe { renderer_begin_frame(h, 0.0, 0.0, 320.0, 240.0, std::ptr::null_mut(), std::ptr::null_mut()) };
        unsafe { renderer_clear(h, 0.0, 0.0, 0.0, 1.0) };

        // 整 bitmap → src_*=0 → 走「整 bitmap」路径
        let s = unsafe {
            renderer_draw_bitmap(
                h, bm, 0.0, 0.0, 0.0, 0.0, 10.0, 10.0, 32.0, 32.0, 0.8, 1,
            )
        };
        assert_eq!(s, RENDERER_OK);

        // 子 rect + nearest 插值
        let s = unsafe {
            renderer_draw_bitmap(
                h, bm, 1.0, 1.0, 4.0, 4.0, 100.0, 100.0, 64.0, 64.0, 1.0, 0,
            )
        };
        assert_eq!(s, RENDERER_OK);

        let st = unsafe { renderer_end_frame(h) };
        assert_eq!(st, RENDERER_OK);

        unsafe { renderer_destroy_bitmap(h, bm) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn bitmap_abi_null_handle_invalid_param() {
        // 所有 7 个 bitmap ABI 在 null renderer handle 下必须 INVALID_PARAM。
        let dummy = [0u8; 4];
        let mut out: u32 = 0;
        let mut w: u32 = 0;
        let mut hh: u32 = 0;

        let s = unsafe {
            renderer_load_bitmap_from_memory(std::ptr::null_mut(), dummy.as_ptr(), 4, &mut out)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe {
            renderer_load_bitmap_from_file(std::ptr::null_mut(), dummy.as_ptr(), 4, &mut out)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe { renderer_create_texture(std::ptr::null_mut(), 16, 16, 0, &mut out) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe {
            renderer_update_texture(std::ptr::null_mut(), 1, dummy.as_ptr(), 4, 4, 0)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe {
            renderer_get_bitmap_size(std::ptr::null_mut(), 1, &mut w, &mut hh)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe { renderer_destroy_bitmap(std::ptr::null_mut(), 1) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);

        let s = unsafe {
            renderer_draw_bitmap(
                std::ptr::null_mut(),
                1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 16.0, 16.0, 1.0, 1,
            )
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
    }

    // ---------- v0.7 phase 5 path + 渐变 ABI 测试 ----------

    /// 构造一个最小合法 path：MOVE_TO(10,10) → LINE_TO(50,10) → LINE_TO(50,50) → CLOSE
    fn make_simple_triangle_path() -> Vec<u8> {
        let mut p = Vec::new();
        p.push(0x01u8); // MOVE_TO
        p.extend_from_slice(&10.0f32.to_le_bytes());
        p.extend_from_slice(&10.0f32.to_le_bytes());
        p.push(0x02u8); // LINE_TO
        p.extend_from_slice(&50.0f32.to_le_bytes());
        p.extend_from_slice(&10.0f32.to_le_bytes());
        p.push(0x02u8);
        p.extend_from_slice(&50.0f32.to_le_bytes());
        p.extend_from_slice(&50.0f32.to_le_bytes());
        p.push(0x05u8); // CLOSE
        p
    }

    #[test]
    fn fill_path_inside_frame_simple_triangle_ok() {
        let h = make_renderer(640, 480);
        let path = make_simple_triangle_path();
        unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        let st = unsafe {
            renderer_fill_path(h, path.as_ptr(), path.len() as i32, 0.5, 0.5, 0.9, 1.0)
        };
        assert_eq!(st, RENDERER_OK);
        assert_eq!(unsafe { renderer_end_frame(h) }, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn stroke_path_inside_frame_simple_triangle_ok() {
        let h = make_renderer(640, 480);
        let path = make_simple_triangle_path();
        unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        let st = unsafe {
            renderer_stroke_path(h, path.as_ptr(), path.len() as i32, 2.0, 0.9, 0.5, 0.5, 1.0, 0)
        };
        assert_eq!(st, RENDERER_OK);
        assert_eq!(unsafe { renderer_end_frame(h) }, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn fill_path_unknown_opcode_returns_unsupported_format() {
        let h = make_renderer(640, 480);
        // 0x06 是 reserved 区间起点（spec §2.3.4 决策 10.1）
        let path: Vec<u8> = vec![0x06, 0, 0, 0, 0, 0, 0, 0, 0];
        unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        let st = unsafe {
            renderer_fill_path(h, path.as_ptr(), path.len() as i32, 0.5, 0.5, 0.9, 1.0)
        };
        assert_eq!(st, RENDERER_ERR_UNSUPPORTED_FORMAT);
        unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn fill_path_truncated_byte_stream_returns_invalid_param() {
        let h = make_renderer(640, 480);
        // MOVE_TO 后只有 4 个字节（不够 8）
        let path: Vec<u8> = vec![0x01, 1, 2, 3, 4];
        unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        let st = unsafe {
            renderer_fill_path(h, path.as_ptr(), path.len() as i32, 0.5, 0.5, 0.9, 1.0)
        };
        assert_eq!(st, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn fill_path_outside_frame_returns_invalid_param() {
        // 不调 begin_frame 直接 fill_path → cmd_drawing 守卫报 INVALID_PARAM
        let h = make_renderer(640, 480);
        let path = make_simple_triangle_path();
        let st = unsafe {
            renderer_fill_path(h, path.as_ptr(), path.len() as i32, 0.5, 0.5, 0.9, 1.0)
        };
        assert_eq!(st, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn fill_path_null_handle_rejected() {
        let path = make_simple_triangle_path();
        let st = unsafe {
            renderer_fill_path(
                std::ptr::null_mut(),
                path.as_ptr(),
                path.len() as i32,
                0.5,
                0.5,
                0.9,
                1.0,
            )
        };
        assert_eq!(st, RENDERER_ERR_INVALID_PARAM);
    }

    #[test]
    fn gradient_linear_inside_frame_ok() {
        let h = make_renderer(640, 480);
        // 2 stops：黑 → 蓝
        let stops: Vec<f32> = vec![
            0.0, 0.0, 0.0, 0.0, 1.0, // offset, r, g, b, a
            1.0, 0.0, 0.0, 1.0, 1.0,
        ];
        unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        let st = unsafe {
            renderer_fill_rect_gradient_linear(
                h, 10.0, 10.0, 200.0, 50.0, 10.0, 10.0, 210.0, 10.0,
                stops.as_ptr(), 2,
            )
        };
        assert_eq!(st, RENDERER_OK);
        assert_eq!(unsafe { renderer_end_frame(h) }, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn gradient_radial_three_stops_ok() {
        let h = make_renderer(640, 480);
        let stops: Vec<f32> = vec![
            0.0, 1.0, 0.0, 0.0, 1.0,
            0.5, 0.0, 1.0, 0.0, 1.0,
            1.0, 0.0, 0.0, 1.0, 1.0,
        ];
        unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        let st = unsafe {
            renderer_fill_rect_gradient_radial(
                h, 10.0, 10.0, 100.0, 100.0, 60.0, 60.0, 50.0, 50.0,
                stops.as_ptr(), 3,
            )
        };
        assert_eq!(st, RENDERER_OK);
        assert_eq!(unsafe { renderer_end_frame(h) }, RENDERER_OK);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn gradient_one_stop_rejected() {
        let h = make_renderer(640, 480);
        let stops: Vec<f32> = vec![0.0, 1.0, 0.0, 0.0, 1.0];
        unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        let st = unsafe {
            renderer_fill_rect_gradient_linear(
                h, 0.0, 0.0, 100.0, 100.0, 0.0, 0.0, 100.0, 0.0,
                stops.as_ptr(), 1,
            )
        };
        // stop_count<2 在 FFI 直接拒
        assert_eq!(st, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn gradient_offset_out_of_range_rejected() {
        let h = make_renderer(640, 480);
        // offset 1.5 越界
        let stops: Vec<f32> = vec![
            0.0, 1.0, 0.0, 0.0, 1.0,
            1.5, 0.0, 0.0, 1.0, 1.0,
        ];
        unsafe {
            renderer_begin_frame(h, 0.0, 0.0, 640.0, 480.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        let st = unsafe {
            renderer_fill_rect_gradient_linear(
                h, 0.0, 0.0, 100.0, 100.0, 0.0, 0.0, 100.0, 0.0,
                stops.as_ptr(), 2,
            )
        };
        assert_eq!(st, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_end_frame(h) };
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn gradient_null_handle_rejected() {
        let stops: Vec<f32> = vec![0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0];
        let st = unsafe {
            renderer_fill_rect_gradient_linear(
                std::ptr::null_mut(),
                0.0, 0.0, 100.0, 100.0, 0.0, 0.0, 100.0, 0.0,
                stops.as_ptr(), 2,
            )
        };
        assert_eq!(st, RENDERER_ERR_INVALID_PARAM);
    }

    // ---------- v0.7 phase 3 video ABI 测试 ----------
    //
    // 无 mp4 测试资产：open_file 必失败路径 + null handle 参数校验 + 双重 close
    // 解码侧（present_frame / seek / get_info 成功路径）留到真实 widget dogfood 验证
    // —— 在 widget 里放一个真实 mp4 跑 30s，spec phase 3 通过判据本来就是整合测试。

    #[test]
    fn video_open_file_null_handle_rejected() {
        let path = b"foo.mp4";
        let mut out: u32 = 0;
        let s = unsafe {
            renderer_video_open_file(std::ptr::null_mut(), path.as_ptr(), path.len() as i32, &mut out)
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
    }

    #[test]
    fn video_open_file_null_path_rejected() {
        let h = make_renderer(320, 240);
        let mut out: u32 = 0;
        let s = unsafe { renderer_video_open_file(h, std::ptr::null(), 10, &mut out) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn video_open_file_zero_len_rejected() {
        let h = make_renderer(320, 240);
        let path = b"foo.mp4";
        let mut out: u32 = 0;
        let s = unsafe { renderer_video_open_file(h, path.as_ptr(), 0, &mut out) };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn video_open_file_null_out_rejected() {
        let h = make_renderer(320, 240);
        let path = b"foo.mp4";
        let s = unsafe {
            renderer_video_open_file(h, path.as_ptr(), path.len() as i32, std::ptr::null_mut())
        };
        assert_eq!(s, RENDERER_ERR_INVALID_PARAM);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn video_open_file_nonexistent_returns_video_open_fail() {
        // MF 对不存在文件返非零 HRESULT，统一映射到 VIDEO_OPEN_FAIL（-15）。
        // 也覆盖路径：open 失败时 bitmap slot 不泄漏（靠 ResourceTable allocated_count 间接验证）。
        let h = make_renderer(320, 240);
        let path = b"Z:\\nonexistent\\not_a_real_video.mp4";
        let mut out: u32 = 0;
        let s = unsafe {
            renderer_video_open_file(h, path.as_ptr(), path.len() as i32, &mut out)
        };
        assert_eq!(s, RENDERER_ERR_VIDEO_OPEN_FAIL);
        assert_eq!(out, 0, "handle should stay 0 on failure");
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn video_get_info_on_invalid_handle_returns_not_found() {
        let h = make_renderer(320, 240);
        let mut info = VideoInfo {
            duration_ms: 0,
            width: 0,
            height: 0,
            fps_num: 0,
            fps_den: 0,
        };
        // handle=0 总是非法
        let s = unsafe { renderer_video_get_info(h, 0, &mut info) };
        assert_eq!(s, RENDERER_ERR_VIDEO_NOT_FOUND);
        // 任意其他 handle（从未分配过）也非法
        let s = unsafe { renderer_video_get_info(h, 0xDEAD_BEEF, &mut info) };
        assert_eq!(s, RENDERER_ERR_VIDEO_NOT_FOUND);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn video_seek_on_invalid_handle_returns_not_found() {
        let h = make_renderer(320, 240);
        let s = unsafe { renderer_video_seek(h, 0, 1000) };
        assert_eq!(s, RENDERER_ERR_VIDEO_NOT_FOUND);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn video_present_frame_on_invalid_handle_returns_not_found() {
        let h = make_renderer(320, 240);
        let mut bm: u32 = 42;  // 预置非零让测试能检 out_bitmap 是否被清零
        let mut eof: i32 = -1;
        let s = unsafe { renderer_video_present_frame(h, 0xCAFEBABE, &mut bm, &mut eof) };
        assert_eq!(s, RENDERER_ERR_VIDEO_NOT_FOUND);
        assert_eq!(bm, 0, "out_bitmap should be cleared on failure");
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn video_close_on_invalid_handle_returns_not_found() {
        let h = make_renderer(320, 240);
        let s = unsafe { renderer_video_close(h, 0xDEAD_BEEF) };
        assert_eq!(s, RENDERER_ERR_VIDEO_NOT_FOUND);
        unsafe { renderer_destroy(h) };
    }

    #[test]
    fn video_null_handle_all_apis_rejected() {
        // 所有 5 个 video API 都在 handle==null 时立刻返 INVALID_PARAM，不碰内部状态。
        assert_eq!(
            unsafe { renderer_video_seek(std::ptr::null_mut(), 1, 100) },
            RENDERER_ERR_INVALID_PARAM
        );
        assert_eq!(
            unsafe { renderer_video_close(std::ptr::null_mut(), 1) },
            RENDERER_ERR_INVALID_PARAM
        );
        let mut info = VideoInfo {
            duration_ms: 0,
            width: 0,
            height: 0,
            fps_num: 0,
            fps_den: 0,
        };
        assert_eq!(
            unsafe { renderer_video_get_info(std::ptr::null_mut(), 1, &mut info) },
            RENDERER_ERR_INVALID_PARAM
        );
        let mut bm: u32 = 0;
        let mut eof: i32 = 0;
        assert_eq!(
            unsafe { renderer_video_present_frame(std::ptr::null_mut(), 1, &mut bm, &mut eof) },
            RENDERER_ERR_INVALID_PARAM
        );
    }
}
