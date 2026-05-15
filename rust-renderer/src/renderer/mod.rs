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
/// v0.7 phase 3：Media Foundation 本地视频解码 → CPU BGRA32 → update_texture 路径
pub(crate) mod mediafoundation;
pub(crate) mod painter;
/// v0.7 phase 2：bitmap / video / capture handle 共享的 slot table + ABA 防护
pub(crate) mod resources;
/// v0.3 历史文件名，内部 OffscreenSurface 已重命名为 `PinnedReadbackBackend`，v0.6 改成 swap chain。
/// 文件名保留以便 git blame 追溯历史。
pub(crate) mod swapchain;
/// v0.7 phase 2：WIC 图片解码 → IWICBitmapSource → ID2D1Bitmap1
pub(crate) mod wic;

use std::ffi::c_void;

use crate::error::{RendererError, RendererResult};
use crate::ffi::{PerfStats, VideoInfo};

use self::device::GpuDevice;
use self::mediafoundation::VideoSource;
use self::painter::TEXTURE_FORMAT_BGRA8;
use self::resources::{BitmapHandle, ResourceTable};
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

    /// v0.7 phase 3：视频解码上下文集合。每个 VideoSource 持有一个 IMFSourceReader +
    /// 它专属的 BitmapHandle（在 painter.bitmaps 里）。close 时先 destroy bitmap 再
    /// 从 videos 移除，保证两边引用一起退场。
    videos: ResourceTable<VideoSource>,

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
            videos: ResourceTable::new(),
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
        self.surface
            .cmd_stroke_rect(x, y, w, h, stroke_width, color)
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
            bitmap,
            src_x,
            src_y,
            src_w,
            src_h,
            dst_x,
            dst_y,
            dst_w,
            dst_h,
            opacity,
            interp_mode,
        )
    }

    // ===== v0.7 phase 5 path + 渐变（薄转发） =====

    pub(crate) fn cmd_fill_path(&mut self, path: &[u8], color: [f32; 4]) -> RendererResult<()> {
        self.surface.cmd_fill_path(path, color)
    }

    pub(crate) fn cmd_stroke_path(
        &mut self,
        path: &[u8],
        stroke_width: f32,
        color: [f32; 4],
        dash_style: i32,
    ) -> RendererResult<()> {
        self.surface
            .cmd_stroke_path(path, stroke_width, color, dash_style)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_fill_rect_gradient_linear(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        sx: f32,
        sy: f32,
        ex: f32,
        ey: f32,
        stops: &[f32],
    ) -> RendererResult<()> {
        self.surface
            .cmd_fill_rect_gradient_linear(x, y, w, h, sx, sy, ex, ey, stops)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cmd_fill_rect_gradient_radial(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        stops: &[f32],
    ) -> RendererResult<()> {
        self.surface
            .cmd_fill_rect_gradient_radial(x, y, w, h, cx, cy, rx, ry, stops)
    }

    // ===== v0.7 phase 3 video（spec §4.1） =====

    /// 打开本地视频文件：先 painter.create_texture(BGRA8) 拿空 bitmap，
    /// 再 VideoSource::open_file 配 IMFSourceReader（输出 RGB32）。
    /// 失败时回滚 bitmap，避免泄漏 slot。
    pub(crate) fn video_open_file(&mut self, path: &str) -> RendererResult<u32> {
        // VideoSource open 之前先用占位尺寸 1x1 创 bitmap —— 真实尺寸从 MF 拿到后再 resize。
        // 走两步是因为 video_open_file 不能调 painter（painter 借走 self.surface）。
        // 简化：先用 dummy 占位 → MF open 拿到 (w,h) → destroy_bitmap → 重新 create_texture。
        // 总共 1 次额外 create + 1 次额外 destroy（仅 open 时一次性开销，可接受）。

        // step 1：先用临时小 bitmap 占 slot
        let dummy = self.surface.create_texture(1, 1, TEXTURE_FORMAT_BGRA8)?;
        // step 2：open MF source reader 拿 (w,h)
        let video_temp = match VideoSource::open_file(path, dummy) {
            Ok(v) => v,
            Err(e) => {
                // open 失败 → 释放占位 bitmap 不留泄漏
                let _ = self.surface.destroy_bitmap(dummy);
                return Err(e);
            }
        };
        let info = video_temp.info();
        // step 3：destroy 占位 + 创真实尺寸的 bitmap
        // 顺序：先 destroy → 再 create。slot 不够时 ResourceLimit 报回去。
        let _ = self.surface.destroy_bitmap(dummy);
        let real_bitmap =
            self.surface
                .create_texture(info.width, info.height, TEXTURE_FORMAT_BGRA8)?;
        // step 4：把 video_temp 拆出来重建 with real bitmap handle
        // VideoSource 内部 reader / staging 不依赖 bitmap 字段（仅在 close 时由上层用）
        let mut video = video_temp;
        video.bitmap = real_bitmap;

        // step 5：插 videos table。失败要回滚 bitmap。
        match self.videos.insert(video) {
            Ok(handle) => Ok(handle),
            Err(e) => {
                let _ = self.surface.destroy_bitmap(real_bitmap);
                Err(e)
            }
        }
    }

    pub(crate) fn video_get_info(&self, video: u32) -> RendererResult<VideoInfo> {
        let v = self
            .videos
            .get(video)
            .map_err(|_| RendererError::VideoNotFound)?;
        let info = v.info();
        Ok(VideoInfo {
            duration_ms: info.duration_ms,
            width: info.width,
            height: info.height,
            fps_num: info.fps_num,
            fps_den: info.fps_den,
        })
    }

    pub(crate) fn video_seek(&mut self, video: u32, time_ms: u64) -> RendererResult<()> {
        let v = self
            .videos
            .get_mut(video)
            .map_err(|_| RendererError::VideoNotFound)?;
        v.seek(time_ms)
    }

    /// 解一帧并 update_texture 到内部 bitmap。
    /// 返 (bitmap_handle, eof)。eof=true 表示流结束，bitmap 仍是最后一帧。
    /// **每个 video 反复调返同一 BitmapHandle**（spec §4.1）—— 业务用 cmd_draw_bitmap 画即可。
    pub(crate) fn video_present_frame(
        &mut self,
        video: u32,
    ) -> RendererResult<(BitmapHandle, bool)> {
        // 拿 video 引用 → 解一帧到 staging → 把 staging 上传到 painter bitmap。
        // 借用栅栏：read_next_frame 借 &mut videos.get_mut(video)，update_texture 借 &mut self.surface。
        // 两个借用不重叠。
        let (bitmap, w, eof) = {
            let v = self
                .videos
                .get_mut(video)
                .map_err(|_| RendererError::VideoNotFound)?;
            let (slice_ptr, slice_len, stride, eof, bitmap) = {
                let (s, stride, eof) = v.read_next_frame()?;
                (s.as_ptr(), s.len(), stride, eof, v.bitmap)
            };
            // SAFETY：slice 来自 v.staging，借用结束前不动。
            let slice: &[u8] = unsafe { std::slice::from_raw_parts(slice_ptr, slice_len) };
            self.surface
                .update_texture(bitmap, slice, stride, TEXTURE_FORMAT_BGRA8)?;
            (bitmap, stride, eof)
        };
        let _ = w; // 留给未来加 perf log
        Ok((bitmap, eof))
    }

    /// 关闭一个视频：先 destroy 它的 bitmap，再从 videos 移除。
    /// 顺序很重要：bitmap 在 painter 的 ResourceTable 里，video drop 不会自动 destroy bitmap。
    pub(crate) fn video_close(&mut self, video: u32) -> RendererResult<()> {
        let removed = self
            .videos
            .remove(video)
            .map_err(|_| RendererError::VideoNotFound)?;
        // bitmap 可能已经被业务（错误地）destroy 过了 —— 这种情况下 painter 端返
        // ResourceNotFound，我们吞掉（双重释放视为成功，跟其他 destroy_* 路径一致）。
        let _ = self.surface.destroy_bitmap(removed.bitmap);
        Ok(())
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
