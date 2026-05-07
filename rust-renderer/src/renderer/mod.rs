//! 渲染器内部模块（v0.6 DComp swap chain 路径）
//!
//! 阶段路线：
//! - **R1-R3**：（已废弃）SwapChainPanel + ISwapChainPanelNative + IDXGISwapChain1。
//!   widget host 跨进程代理拒绝该 native COM 接口。
//! - **V1**：（已废弃）单 RT + CPU staging + Map readback，C# byte[] 喂 MediaPlayer。
//!   readback 22ms 撑不住稳定 60fps。
//! - **V2**：（已废弃）双 RT 池 + GPU surface 直接给 MediaPlayer。
//!   MediaPlayer 内部对 BGRA8 D3D11Surface 走 fallback path 累积 leak。
//! - **V3 Pinned**：（已废弃）双 RT 池 + 双 staging + pipelined async readback +
//!   Image+WriteableBitmap 直拷。WriteableBitmap 在 modal move loop 期间不更新（XAML
//!   compositor 静止），用户拖动 widget 时画面冻结。
//! - **v0.5 viewport-aware**：（已废弃）上面基础上 RT 池缩到 widget 大小，CPU 流水线
//!   降到 ~1.5-3ms。modal block 问题依旧。
//! - **v0.6 DComp（当前）**：纯 swap chain + DComp surface 路径。Rust 端
//!   `IDXGIFactory2::CreateSwapChainForComposition` 创建不绑 HWND 的 swap chain，
//!   命令式 ABI begin/cmd/end 内部走 D2D + Present(0,0)。C# 端拿 swap chain ptr 通过
//!   `ICompositorInterop::CreateCompositionSurfaceForSwapChain` 包成 ICompositionSurface，
//!   挂到 ElementCompositionPreview.SetElementChildVisual(rootGrid) → DWM 内核合成器
//!   直接显示，不经 XAML compositor，**modal 不阻塞**。

pub(crate) mod device;
pub(crate) mod painter;
/// v0.7 phase 2：bitmap / video / capture handle 共享的 slot table + ABA 防护
pub(crate) mod resources;
/// v0.3 历史文件名，内部 OffscreenSurface 已重命名为 `PinnedReadbackBackend`，v0.6 改成 swap chain。
/// 文件名保留以便 git blame 追溯历史。
pub(crate) mod swapchain;
/// v0.7 phase 2：WIC 图片解码 → IWICBitmapSource → ID2D1Bitmap1
pub(crate) mod wic;

use std::ffi::c_void;

use crate::error::RendererResult;
use crate::ffi::PerfStats;

use self::device::GpuDevice;
use self::swapchain::{PinnedReadbackBackend, PresentFrame};

/// 滑动统计窗口大小（最近 N 帧）。60 帧约 1 秒（@60fps）。
const PERF_WINDOW: usize = 60;

/// 渲染器内部状态
///
/// pull-driven：没有独立渲染线程。每次 `end_frame` 在调用线程上同步执行
/// EndDraw → Present。Renderer 句柄外层有 parking_lot::Mutex 串行化，
/// 所以这里所有方法都是 `&mut self`。
pub(crate) struct RendererState {
    /// device + immediate context 的"母体"。Backend 借走 clone（COM AddRef），
    /// 但 GpuDevice 必须最后 drop 才能保证整个图形 device 还活着。
    #[allow(dead_code)]
    gpu: GpuDevice,
    surface: PinnedReadbackBackend,

    /// 已 end_frame 成功的帧数（从 1 开始递增）
    frame_index: u64,

    /// Perf 滑动统计
    render_samples: [u64; PERF_WINDOW],
    /// v0.6 起：原 readback_us 改为 present_us（Present 调用耗时）。字段名保持不变以减少 ABI 改动
    readback_samples: [u64; PERF_WINDOW],
    sample_idx: usize,
    valid_samples: u32,
    peak_render_us: u64,
    peak_readback_us: u64,
}

impl RendererState {
    pub(crate) fn new(width: u32, height: u32) -> RendererResult<Self> {
        let gpu = GpuDevice::create()?;
        let surface = PinnedReadbackBackend::create(&gpu.device, &gpu.context, width, height)?;

        Ok(Self {
            gpu,
            surface,
            frame_index: 0,
            render_samples: [0; PERF_WINDOW],
            readback_samples: [0; PERF_WINDOW],
            sample_idx: 0,
            valid_samples: 0,
            peak_render_us: 0,
            peak_readback_us: 0,
        })
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) -> RendererResult<()> {
        self.surface.resize(width, height)
    }

    /// v0.7 §2.6.3 — 显式 canvas 改尺寸，cmd 帧中调用返 FrameStillHeld。
    pub(crate) fn resize_canvas(&mut self, new_w: u32, new_h: u32) -> RendererResult<()> {
        self.surface.resize_canvas(new_w, new_h)
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        self.surface.size()
    }

    /// 暴露 swap chain 的 IUnknown raw ptr 给 C#（Marshal.GetObjectForIUnknown 转 IDXGISwapChain）。
    /// 调用方用完必须 Marshal.Release（Rust 端 AddRef 一次给 C#）。
    pub(crate) fn get_swapchain_iunknown(&self) -> *mut c_void {
        self.surface.get_swapchain_iunknown()
    }

    // ===== 命令式 Painter API（薄转发到 PinnedReadbackBackend） =====

    pub(crate) fn begin_frame(
        &mut self,
        vp_x: f32,
        vp_y: f32,
        vp_w: f32,
        vp_h: f32,
    ) -> RendererResult<()> {
        self.surface.begin_frame(vp_x, vp_y, vp_w, vp_h)
    }

    pub(crate) fn cmd_clear(&mut self, color: [f32; 4]) -> RendererResult<()> {
        self.surface.cmd_clear(color)
    }

    pub(crate) fn cmd_fill_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.surface.cmd_fill_rect(x, y, w, h, color)
    }

    pub(crate) fn cmd_draw_text(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.surface.cmd_draw_text(text, x, y, font_size, color)
    }

    // ===== v0.7 矢量图元（薄转发） =====

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_draw_line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        stroke_width: f32,
        color: [f32; 4],
        dash_style: i32,
    ) -> RendererResult<()> {
        self.surface
            .cmd_draw_line(x0, y0, x1, y1, stroke_width, color, dash_style)
    }

    pub(crate) fn cmd_draw_polyline(
        &mut self,
        points: &[(f32, f32)],
        stroke_width: f32,
        color: [f32; 4],
        closed: bool,
    ) -> RendererResult<()> {
        self.surface
            .cmd_draw_polyline(points, stroke_width, color, closed)
    }

    pub(crate) fn cmd_stroke_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        stroke_width: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.surface.cmd_stroke_rect(x, y, w, h, stroke_width, color)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_fill_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius_x: f32,
        radius_y: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.surface
            .cmd_fill_rounded_rect(x, y, w, h, radius_x, radius_y, color)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_stroke_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius_x: f32,
        radius_y: f32,
        stroke_width: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.surface
            .cmd_stroke_rounded_rect(x, y, w, h, radius_x, radius_y, stroke_width, color)
    }

    pub(crate) fn cmd_fill_ellipse(
        &mut self,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.surface.cmd_fill_ellipse(cx, cy, rx, ry, color)
    }

    pub(crate) fn cmd_stroke_ellipse(
        &mut self,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        stroke_width: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        self.surface
            .cmd_stroke_ellipse(cx, cy, rx, ry, stroke_width, color)
    }

    pub(crate) fn cmd_push_clip_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> RendererResult<()> {
        self.surface.cmd_push_clip_rect(x, y, w, h)
    }

    pub(crate) fn cmd_pop_clip(&mut self) -> RendererResult<()> {
        self.surface.cmd_pop_clip()
    }

    pub(crate) fn cmd_set_transform(&mut self, matrix: [f32; 6]) -> RendererResult<()> {
        self.surface.cmd_set_transform(matrix)
    }

    pub(crate) fn cmd_reset_transform(&mut self) -> RendererResult<()> {
        self.surface.cmd_reset_transform()
    }

    // ===== v0.7 phase 2 bitmap 资源 =====

    pub(crate) fn load_bitmap_from_memory(
        &mut self,
        bytes: &[u8],
    ) -> RendererResult<crate::renderer::resources::BitmapHandle> {
        self.surface.load_bitmap_from_memory(bytes)
    }

    pub(crate) fn create_texture(
        &mut self,
        width: u32,
        height: u32,
        format: i32,
    ) -> RendererResult<crate::renderer::resources::BitmapHandle> {
        self.surface.create_texture(width, height, format)
    }

    pub(crate) fn update_texture(
        &mut self,
        h: crate::renderer::resources::BitmapHandle,
        bytes: &[u8],
        stride: i32,
        format: i32,
    ) -> RendererResult<()> {
        self.surface.update_texture(h, bytes, stride, format)
    }

    pub(crate) fn get_bitmap_size(
        &self,
        h: crate::renderer::resources::BitmapHandle,
    ) -> RendererResult<(u32, u32)> {
        self.surface.get_bitmap_size(h)
    }

    pub(crate) fn destroy_bitmap(
        &mut self,
        h: crate::renderer::resources::BitmapHandle,
    ) -> RendererResult<()> {
        self.surface.destroy_bitmap(h)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_draw_bitmap(
        &mut self,
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
        self.surface.cmd_draw_bitmap(
            bitmap, src_x, src_y, src_w, src_h, dst_x, dst_y, dst_w, dst_h, opacity, interp_mode,
        )
    }

    /// v0.6 end_frame：内部 EndDraw + Present(0, 0)。不返 mapped pointer。
    pub(crate) fn end_frame(&mut self) -> RendererResult<PresentFrame> {
        let frame = self.surface.end_frame()?;
        self.record_perf(frame.render_us, frame.present_us);
        self.frame_index = self.frame_index.wrapping_add(1);
        Ok(frame)
    }

    /// 兼容残留：v0.6 不需要 release。保留 no-op 让 C# 端旧代码调到也不出错。
    pub(crate) fn release_pinned_frame(&mut self) {
        self.surface.release_pinned_frame();
    }

    pub(crate) fn perf_stats(&self) -> PerfStats {
        let n = self.valid_samples as u64;
        if n == 0 {
            return PerfStats {
                avg_render_us: 0,
                avg_readback_us: 0,
                avg_total_us: 0,
                peak_render_us: self.peak_render_us,
                peak_readback_us: self.peak_readback_us,
                total_frames: self.frame_index,
                window_size: PERF_WINDOW as u32,
                valid_samples: 0,
            };
        }
        let valid = self.valid_samples as usize;
        let render_sum: u64 = self.render_samples[..valid].iter().sum();
        let readback_sum: u64 = self.readback_samples[..valid].iter().sum();
        PerfStats {
            avg_render_us: render_sum / n,
            avg_readback_us: readback_sum / n,
            avg_total_us: (render_sum + readback_sum) / n,
            peak_render_us: self.peak_render_us,
            peak_readback_us: self.peak_readback_us,
            total_frames: self.frame_index,
            window_size: PERF_WINDOW as u32,
            valid_samples: self.valid_samples,
        }
    }

    fn record_perf(&mut self, render_us: u64, readback_us: u64) {
        self.render_samples[self.sample_idx] = render_us;
        self.readback_samples[self.sample_idx] = readback_us;
        self.sample_idx = (self.sample_idx + 1) % PERF_WINDOW;
        if (self.valid_samples as usize) < PERF_WINDOW {
            self.valid_samples += 1;
        }
        if render_us > self.peak_render_us {
            self.peak_render_us = render_us;
        }
        if readback_us > self.peak_readback_us {
            self.peak_readback_us = readback_us;
        }
    }
}

// pull-driven 不需要独立渲染线程，所以 `Drop` 也不需要 join。
// PinnedReadbackBackend 的 Drop 会自动 Release swap chain / D2D bitmap / D2DEngine（windows-rs RAII）。
