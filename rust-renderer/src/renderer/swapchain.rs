//! v0.6 DComp swap chain 路径
//!
//! 历史
//! - **R3 阶段**（已废弃）：尝试 SwapChainPanel + ISwapChainPanelNative 直挂 swap chain，
//!   widget host 跨进程代理拒绝该 native COM 接口（QI E_NOINTERFACE），路被堵死。
//! - **V1 阶段**（已废弃）：单 RT + staging texture + CPU readback，C# `MediaStreamSource`
//!   喂 byte[] 给 MediaPlayer。readback 22ms 撑不住稳定 60fps。
//! - **V2 阶段**（已废弃）：双 RT 池 + GPU surface 直接给 MediaPlayer，避开 CPU readback。
//!   MediaPlayer 内部对 BGRA8 D3D11Surface 走 fallback path 累积 leak。
//! - **V3 阶段（已废弃）**：双 RT 池 + Image+WriteableBitmap +
//!   `SoftwareBitmap.CreateCopyFromSurfaceAsync` (C# 端 14ms readback) → 解决 leak
//!   但 C# tick 贴 60fps 红线。
//! - **V3 Pinned 阶段（已废弃）**：双 RT 池 + 双 staging + pipelined async readback +
//!   Image+WriteableBitmap 直拷。WriteableBitmap 路径在 modal move loop（用户拖动 widget）
//!   期间 UI 线程冻结、`CompositionTarget.Rendering` 不 fire、`wb.Invalidate()` 排队等
//!   modal 结束 —— 用户拖动 x 轴期间画面冻结。无法解决。
//! - **v0.5 viewport-aware（已废弃）**：上面基础上把 RT 池缩到 widget 物理像素，
//!   `SetTransform(translate(-vx,-vy))` 让命令仍按 canvas-space 推。GPU/CPU 流水线
//!   降到 ~1.5-3ms，但 modal block 问题依然在。
//! - **v0.6 DComp（这里）**：用 `IDXGIFactory2::CreateSwapChainForComposition` 创建一个
//!   不绑 HWND 的 swap chain，直接渲染 + Present 到 swap chain back buffer。C# 端通过
//!   `ICompositorInterop::CreateCompositionSurfaceForSwapChain` 把 swap chain 包装成
//!   `ICompositionSurface`，挂到 `SpriteVisual.Brush` →
//!   `ElementCompositionPreview.SetElementChildVisual(rootGrid, visual)`。
//!   widget 内容由 OS 级 visual tree + DWM 内核合成器直接显示，**不经 XAML compositor，
//!   modal 不阻塞**。
//!
//! ## 设计
//! - `swap_chain: IDXGISwapChain1`：CreateSwapChainForComposition 创建，BGRA8 + premul alpha。
//!   双 buffer + FLIP_SEQUENTIAL；DComp 自动从 GetBuffer(0) 拉当前 back buffer。
//! - `d2d_bitmap: Option<ID2D1Bitmap1>`：当前 back buffer 的 D2D wrapper，按需重建。
//!   Present 后 buffer 0 是新的 back buffer，但 swap chain GetBuffer(0) 在 FLIP_SEQUENTIAL
//!   下总是当前可写 buffer，所以 D2D bitmap 可以一直 wrap buffer 0（只在 ResizeBuffers 时重建）。
//!   实际上 windows-rs 的 GetBuffer 每次返回新的 ID3D11Texture2D ref，所以每次 begin_frame
//!   都重新 GetBuffer(0) + 重建 bitmap 是最安全的；CreateBitmapFromDxgiSurface ~50us，可以接受。
//! - viewport size 变化 → `swap_chain.ResizeBuffers` + 重建 bitmap
//! - canvas resize 不动 swap chain（只更新 width/height 字段，作为命令坐标系参考）
//!
//! ## ABI 改动（v0.6 vs v0.5）
//! - 删 `release_pinned_frame` 和 `PinnedFrame.data/row_pitch`（不需要 readback）
//! - `end_frame` 不再返 mapped pointer，只返 timing
//! - 新增 `get_swapchain()` 让 C# 拿 IDXGISwapChain raw pointer，包成 ICompositionSurface
//!
//! ## 资源参数
//! - swap chain Format: `DXGI_FORMAT_B8G8R8A8_UNORM`（BGRA8 SDR）
//! - AlphaMode: `DXGI_ALPHA_MODE_PREMULTIPLIED`（widget 透明背景必需）
//! - SwapEffect: `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`（DComp 推荐；FLIP_DISCARD 不能与 D2D bitmap 兼容）
//! - BufferCount: 2（双缓冲 Present 节奏）
//! - Scaling: `DXGI_SCALING_STRETCH`（DComp 通常会 transform，但 brush.Stretch 控制实际 stretch）

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::time::Instant;

use windows::core::Interface;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use crate::error::{RendererError, RendererResult};

use super::painter::{D2DEngine, Painter};

/// v0.6 DComp swap chain backend。
///
/// 名字保留 `PinnedReadbackBackend` 是历史 anchor —— 实际不再做 pinned readback，
/// 改成 swap chain present + DComp surface 直接合成。重命名留给后续清理。
pub(crate) struct PinnedReadbackBackend {
    /// 持有 device 仅供 ResizeBuffers / 重建 bitmap 时引用。
    #[allow(dead_code)]
    device: ID3D11Device,
    /// 渲染发命令用（Present 也走它）
    #[allow(dead_code)]
    context: ID3D11DeviceContext,

    /// DComp swap chain（不绑 HWND）。每次 viewport size 变化调 ResizeBuffers。
    swap_chain: IDXGISwapChain1,
    /// 当前 back buffer 的 D2D wrapper。viewport 变化时重建。
    /// 第一次 begin_frame 时按需创建，避免 create() 阶段就锁定 buffer
    /// （swap chain BindFlags 必须 RT，CreateBitmapFromDxgiSurface 才能 wrap）。
    d2d_bitmap: Option<ID2D1Bitmap1>,

    /// D2D + DWrite 引擎，跨 viewport resize 长存
    d2d: D2DEngine,

    /// canvas 逻辑尺寸（业务命令坐标系参考）
    width: u32,
    height: u32,
    /// 当前 swap chain back buffer 物理尺寸（widget 物理像素）
    vp_w: u32,
    vp_h: u32,

    /// `begin_frame` 后置 true，`end_frame` 消费后清回 false。防 cmd_* 在 begin 之外调；防双 begin。
    cmd_drawing: bool,
    /// `begin_frame` 时记录，`end_frame` 算 render_us
    cmd_render_start: Option<Instant>,
}

// SAFETY: D3D11/DXGI 资源持有内部 COM ref。RendererState 用 parking_lot::Mutex 在 C ABI
// 边界做了串行化保护。Present + GetBuffer 跨线程调（C# ThreadPool）也通过该 Mutex 串行。
unsafe impl Send for PinnedReadbackBackend {}

impl PinnedReadbackBackend {
    pub(crate) fn create(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> RendererResult<Self> {
        if width == 0 || height == 0 {
            return Err(RendererError::InvalidParam("zero pixel size on create"));
        }

        let factory = create_dxgi_factory2(device)?;
        // 初始 swap chain size = canvas size；首次 begin_frame 按 widget 大小 ResizeBuffers
        let swap_chain = create_composition_swap_chain(&factory, device, width, height)?;
        let d2d = D2DEngine::create(device)?;

        crate::log::emit(
            2,
            &format!(
                "PinnedReadbackBackend (DComp swap chain) created canvas {}x{} BGRA8 premul (init swap size = canvas)",
                width, height
            ),
        );

        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            swap_chain,
            d2d_bitmap: None,
            d2d,
            width,
            height,
            vp_w: width,
            vp_h: height,
            cmd_drawing: false,
            cmd_render_start: None,
        })
    }

    /// 把 swap chain 暴露给 C# 端用 ICompositorInterop 包成 ICompositionSurface。
    ///
    /// 返 `IUnknown*` (AddRef 给调用方)。C# 拿到后用 `Marshal.GetObjectForIUnknown` 转
    /// IDXGISwapChain，然后 QI ICompositorInterop 调 CreateCompositionSurfaceForSwapChain。
    /// 调用方用完必须 `Marshal.Release` 一次（成对 AddRef）。
    pub(crate) fn get_swapchain_iunknown(&self) -> *mut c_void {
        // swap_chain.cast::<IUnknown>() AddRef 一次，取 raw 后 forget 让计数留给 C#
        let unk = match self.swap_chain.cast::<windows::core::IUnknown>() {
            Ok(u) => u,
            Err(_) => return std::ptr::null_mut(),
        };
        let raw = unk.into_raw();
        raw
    }

    /// v0.6 DComp 入口：开启一帧。
    ///
    /// 业务侧告诉 renderer "本帧只关心 canvas 中 (vx, vy, vw, vh) 这块矩形"。
    /// - 业务命令坐标系仍是 canvas-space
    /// - viewport 物理像素与 swap chain back buffer 大小不一致 → ResizeBuffers + 重建 bitmap
    /// - SetTarget(d2d_bitmap) + BeginDraw + SetTransform(translate(-vx, -vy))
    ///
    /// 不可重入：begin_frame 后必须先 end_frame 才能再次 begin_frame。
    pub(crate) fn begin_frame(
        &mut self,
        vp_x: f32,
        vp_y: f32,
        vp_w: f32,
        vp_h: f32,
    ) -> RendererResult<()> {
        if self.cmd_drawing {
            return Err(RendererError::InvalidParam(
                "begin_frame called while previous frame is still in cmd-mode",
            ));
        }
        let new_vp_w = (vp_w.round() as i32).max(1) as u32;
        let new_vp_h = (vp_h.round() as i32).max(1) as u32;

        // viewport 大小变化或首次 begin_frame（_d2d_bitmap=None）→ ResizeBuffers + 重建 bitmap
        if new_vp_w != self.vp_w || new_vp_h != self.vp_h || self.d2d_bitmap.is_none() {
            // 重建前先解绑 D2D target（如果旧 bitmap 还被 dc 当 target）
            unsafe { self.d2d.dc.SetTarget(None); }
            self.d2d_bitmap = None;

            if new_vp_w != self.vp_w || new_vp_h != self.vp_h {
                unsafe {
                    self.swap_chain
                        .ResizeBuffers(
                            2,
                            new_vp_w,
                            new_vp_h,
                            DXGI_FORMAT_B8G8R8A8_UNORM,
                            DXGI_SWAP_CHAIN_FLAG(0),
                        )
                        .map_err(RendererError::SwapChainInit)?;
                }
                self.vp_w = new_vp_w;
                self.vp_h = new_vp_h;
                crate::log::emit(
                    1,
                    &format!(
                        "DComp swap chain resized to {}x{} (canvas {}x{})",
                        new_vp_w, new_vp_h, self.width, self.height
                    ),
                );
            }
            // 创建新 D2D bitmap wrapper
            self.d2d_bitmap = Some(create_d2d_bitmap_from_buffer(&self.swap_chain, &self.d2d)?);
        }

        let bitmap = self.d2d_bitmap.as_ref().unwrap();
        unsafe {
            self.d2d.dc.SetTarget(bitmap);
            self.d2d.dc.BeginDraw();
            // canvas-space → RT-space: 命令坐标 (cx, cy) 变成 RT 坐标 (cx - vx, cy - vy)
            let m = Matrix3x2 {
                M11: 1.0,
                M12: 0.0,
                M21: 0.0,
                M22: 1.0,
                M31: -vp_x,
                M32: -vp_y,
            };
            self.d2d.dc.SetTransform(&m);
        }
        self.cmd_drawing = true;
        self.cmd_render_start = Some(Instant::now());
        Ok(())
    }

    /// 清屏到指定颜色（premultiplied alpha）。`Clear` 不受 SetTransform 影响 —— 清的是
    /// 当前 RT 的全部像素，即 viewport 区域。
    pub(crate) fn cmd_clear(&mut self, color: [f32; 4]) -> RendererResult<()> {
        if !self.cmd_drawing {
            return Err(RendererError::InvalidParam(
                "cmd_clear called outside begin_frame/end_frame",
            ));
        }
        let mut painter = Painter::new(&self.d2d, (self.width, self.height));
        painter.clear(color);
        Ok(())
    }

    /// 实心矩形。坐标 = canvas-space。
    pub(crate) fn cmd_fill_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        if !self.cmd_drawing {
            return Err(RendererError::InvalidParam(
                "cmd_fill_rect called outside begin_frame/end_frame",
            ));
        }
        let mut painter = Painter::new(&self.d2d, (self.width, self.height));
        painter.fill_rect(x, y, w, h, color);
        Ok(())
    }

    /// 单行 UTF-8 文本。坐标 = canvas-space。
    pub(crate) fn cmd_draw_text(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        color: [f32; 4],
    ) -> RendererResult<()> {
        if !self.cmd_drawing {
            return Err(RendererError::InvalidParam(
                "cmd_draw_text called outside begin_frame/end_frame",
            ));
        }
        let mut painter = Painter::new(&self.d2d, (self.width, self.height));
        painter.draw_text(text, x, y, font_size, color);
        Ok(())
    }

    /// v0.6 DComp 入口：提交一帧 → EndDraw + Present(0, 0)。
    ///
    /// Present 触发 DComp 拉新内容（异步、跨线程安全）；不返 mapped pointer。
    /// 必须先 begin_frame，否则返 InvalidParam。
    pub(crate) fn end_frame(&mut self) -> RendererResult<PresentFrame> {
        if !self.cmd_drawing {
            return Err(RendererError::InvalidParam(
                "end_frame called without begin_frame",
            ));
        }
        let render_us = self
            .cmd_render_start
            .take()
            .map(|s| s.elapsed().as_micros() as u64)
            .unwrap_or(0);

        // 1) EndDraw + 解绑 target
        unsafe {
            self.d2d
                .dc
                .EndDraw(None, None)
                .map_err(RendererError::FrameAcquire)?;
            self.d2d.dc.SetTarget(None);
        }

        // 2) Present(0, 0) —— SyncInterval=0 让 DComp 自己安排（不强制 vsync），Flags=0
        let present_start = Instant::now();
        unsafe {
            // Present 返回 HRESULT；FLIP_SEQUENTIAL 下偶尔返 DXGI_STATUS_OCCLUDED 但不致命
            let _ = self.swap_chain.Present(0, DXGI_PRESENT(0));
        }
        let present_us = present_start.elapsed().as_micros() as u64;

        self.cmd_drawing = false;

        Ok(PresentFrame {
            width: self.vp_w,
            height: self.vp_h,
            render_us,
            present_us,
        })
    }

    /// v0.6 DComp：no-op 保持 ABI 兼容。Present 路径不需要 Unmap（没 mapped）。
    pub(crate) fn release_pinned_frame(&mut self) {
        // no-op
    }

    /// canvas 逻辑尺寸变化（显示器分辨率改了）。
    /// 不动 swap chain（swap chain 大小由 viewport 决定，下次 begin_frame 处理）。
    pub(crate) fn resize(&mut self, width: u32, height: u32) -> RendererResult<()> {
        if width == 0 || height == 0 {
            return Err(RendererError::InvalidParam("zero pixel size on resize"));
        }
        if width == self.width && height == self.height {
            return Ok(());
        }
        if self.cmd_drawing {
            // 命令式 begin/end 中间不允许 resize
            unsafe {
                let _ = self.d2d.dc.EndDraw(None, None);
                self.d2d.dc.SetTarget(None);
            }
            self.cmd_drawing = false;
            self.cmd_render_start = None;
            crate::log::emit(3, "resize emergency-ended cmd-mode frame; rerun begin_frame");
        }
        self.width = width;
        self.height = height;
        crate::log::emit(
            2,
            &format!(
                "DComp backend canvas resized to {}x{} (current swap chain: {}x{})",
                width, height, self.vp_w, self.vp_h
            ),
        );
        Ok(())
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// v0.6 DComp `end_frame` 的返回值。不再有 mapped pointer / row_pitch / staging slot —
/// DComp 拉 swap chain 自己合成，C# 不需要 readback。
pub(crate) struct PresentFrame {
    pub width: u32,
    pub height: u32,
    /// `begin_frame` → `end_frame` 之间所有 cmd_* + EndDraw 累积耗时
    pub render_us: u64,
    /// Present(0,0) 调用耗时（CPU 端时间，不等 GPU 完成）
    pub present_us: u64,
}

// =====================================================================
// 内部 helper：DXGI factory / swap chain / D2D bitmap
// =====================================================================

fn create_dxgi_factory2(device: &ID3D11Device) -> RendererResult<IDXGIFactory2> {
    let dxgi_device: IDXGIDevice = device.cast().map_err(RendererError::SwapChainInit)?;
    let adapter: IDXGIAdapter = unsafe {
        dxgi_device
            .GetAdapter()
            .map_err(RendererError::SwapChainInit)?
    };
    let factory: IDXGIFactory2 = unsafe {
        adapter
            .GetParent()
            .map_err(RendererError::SwapChainInit)?
    };
    Ok(factory)
}

fn create_composition_swap_chain(
    factory: &IDXGIFactory2,
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> RendererResult<IDXGISwapChain1> {
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    };

    let swap_chain = unsafe {
        factory
            .CreateSwapChainForComposition(device, &desc, None)
            .map_err(RendererError::SwapChainInit)?
    };
    Ok(swap_chain)
}

fn create_d2d_bitmap_from_buffer(
    swap_chain: &IDXGISwapChain1,
    d2d: &D2DEngine,
) -> RendererResult<ID2D1Bitmap1> {
    // GetBuffer<T>(0) 返当前 back buffer
    let buffer: ID3D11Texture2D = unsafe {
        swap_chain
            .GetBuffer(0)
            .map_err(RendererError::SwapChainInit)?
    };
    let _surface: IDXGISurface = buffer.cast().map_err(RendererError::SwapChainInit)?;
    // d2d.create_target_bitmap 已经在 painter 里做了 GetBuffer 转 IDXGISurface →
    // CreateBitmapFromDxgiSurface 的逻辑，复用
    d2d.create_target_bitmap(&buffer)
}
