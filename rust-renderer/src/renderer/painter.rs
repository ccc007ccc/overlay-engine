//! D2D + DWrite 文字与基础图形渲染（阶段 3：文字渲染基建 + 阶段 3.1 缓存）
//!
//! ## 设计
//! - D2DEngine：从已有 D3D11 device 派生 D2D Factory + Device + DC + DWrite Factory，
//!   一次创建终身复用（resize 不影响）。阶段 3.1 起内嵌 text_format / brush 缓存。
//! - Painter：每帧短生命周期的高层 API（clear / draw_text / fill_rect），
//!   传给业务 Frame trait 实现，业务侧不直接接触 COM
//! - D2D Bitmap1：从 D3D11 RT (BIND_RENDER_TARGET + BIND_SHADER_RESOURCE) 通过
//!   IDXGISurface QI 包装而成，pool 大小同 RT 池（双 buffer）
//!
//! ## 与现有 V3 Pinned 路径的耦合
//! OffscreenSurface 在 acquire_pinned_frame 中：
//! 1. SetTarget(d2d_bitmaps[w])
//! 2. BeginDraw
//! 3. 调业务 closure（业务用 Painter API）
//! 4. EndDraw
//! 5. 然后是已有的 CopyResource RT→staging + Flush + Map 路径
//!
//! D2D EndDraw 内部会 Flush 命令到 GPU，但不等 GPU 完成 —— 所以紧接着的
//! CopyResource 还是异步发命令，pipelined readback 仍然 work。
//!
//! ## 资源参数
//! - D2D Factory: SINGLE_THREADED（OffscreenSurface 已被外层 Mutex 串行化）
//! - DWrite Factory: SHARED（线程安全，可跨线程，但我们也是单线程访问）
//! - Bitmap1: BGRA8 PREMULTIPLIED + TARGET + CANNOT_DRAW
//! - 字体默认 "Segoe UI"（weight/style 固定 NORMAL，因此 cache key 仅按 font_size 量化）
//!
//! ## 性能（阶段 3.1）
//! - CreateBitmapFromDxgiSurface: 一次 + resize 时各一次，per-frame 0
//! - Per-frame: SetTarget (无成本) + BeginDraw + draw_*  + EndDraw
//! - draw_text 首帧：CreateTextFormat ~300us + CreateSolidColorBrush ~200us
//! - draw_text 后续帧：HashMap 命中 ~1us（u32 / [u8;4] key + clone COM ref）
//!
//! ## ABI 影响
//! 不暴露给 C# —— Painter 只在 Rust 内部业务侧使用。

#![allow(non_snake_case)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::ManuallyDrop;

use windows::core::{w, Interface};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1,
    ID2D1SolidColorBrush, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED,
};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_MEASURING_MODE_NATURAL,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGISurface;

use crate::error::{RendererError, RendererResult};

/// font_size 量化到 0.25 px 精度（size * 4）。Segoe UI / NORMAL weight / NORMAL style 固定。
type TextFormatKey = u32;
/// RGBA float [0, 1] 量化到 u8。alpha 也参与 key —— 同色不同透明度是不同 brush。
type BrushKey = [u8; 4];

/// D2D + DWrite 全局引擎，跟 GpuDevice 一样长生命周期
pub(crate) struct D2DEngine {
    /// 用于创建 D2D Bitmap1（包装 D3D11 RT）
    #[allow(dead_code)]
    factory: ID2D1Factory1,
    /// 派生自 IDXGIDevice，与 D3D11 device 共享 GPU
    #[allow(dead_code)]
    device: ID2D1Device,
    /// 真正的绘制接口；OffscreenSurface 每帧 SetTarget → BeginDraw → ... → EndDraw
    pub(crate) dc: ID2D1DeviceContext,
    /// 创建 IDWriteTextFormat 用，draw_text 调用方持有引用
    dwrite: IDWriteFactory,

    /// 阶段 3.1：DWrite TextFormat 缓存（与 D2D dc 无关，跨 resize 长存）
    text_format_cache: RefCell<HashMap<TextFormatKey, IDWriteTextFormat>>,
    /// 阶段 3.1：D2D SolidColorBrush 缓存。Brush 与 dc 绑定，但不与 RT 绑定，跨 resize 长存。
    brush_cache: RefCell<HashMap<BrushKey, ID2D1SolidColorBrush>>,
}

// SAFETY: D2D / DWrite 在 SINGLE_THREADED / SHARED factory 下都是单线程使用。
// OffscreenSurface 由外层 Mutex 串行化访问，跨线程 Send 是安全的。
// RefCell + COM 对象不 auto-impl Send，这里手动 unsafe impl —— 安全前提见上。
unsafe impl Send for D2DEngine {}

impl D2DEngine {
    pub(crate) fn create(d3d11_device: &ID3D11Device) -> RendererResult<Self> {
        // 1) D2D Factory（SINGLE_THREADED 因为外层已 Mutex 串行化）
        let factory: ID2D1Factory1 = unsafe {
            let opts = D2D1_FACTORY_OPTIONS::default();
            D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&opts))
                .map_err(RendererError::DeviceInit)?
        };

        // 2) DXGI device → D2D Device（共享 GPU）
        let dxgi_device: windows::Win32::Graphics::Dxgi::IDXGIDevice =
            d3d11_device.cast().map_err(RendererError::DeviceInit)?;
        let device = unsafe {
            factory
                .CreateDevice(&dxgi_device)
                .map_err(RendererError::DeviceInit)?
        };

        // 3) D2D Device Context
        let dc = unsafe {
            device
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                .map_err(RendererError::DeviceInit)?
        };

        // 4) DWrite Factory（SHARED 是 DWrite 的标准选择）
        let dwrite: IDWriteFactory = unsafe {
            DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).map_err(RendererError::DeviceInit)?
        };

        crate::log::emit(
            2,
            "D2DEngine created (D2D1 + DirectWrite, BGRA8 premul, text/brush cache enabled)",
        );

        Ok(Self {
            factory,
            device,
            dc,
            dwrite,
            text_format_cache: RefCell::new(HashMap::with_capacity(8)),
            brush_cache: RefCell::new(HashMap::with_capacity(16)),
        })
    }

    /// 把 D3D11 RT (BIND_RENDER_TARGET) 包装成 D2D Bitmap1 target。
    pub(crate) fn create_target_bitmap(
        &self,
        rt: &ID3D11Texture2D,
    ) -> RendererResult<ID2D1Bitmap1> {
        let dxgi_surface: IDXGISurface = rt.cast().map_err(RendererError::FrameAcquire)?;

        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: ManuallyDrop::new(None),
        };

        let bitmap = unsafe {
            self.dc
                .CreateBitmapFromDxgiSurface(&dxgi_surface, Some(&props))
                .map_err(RendererError::FrameAcquire)?
        };
        Ok(bitmap)
    }

    /// 阶段 3.1：按 (font_size 量化) 拿/造 IDWriteTextFormat。
    ///
    /// 命中：HashMap O(1) + clone（COM AddRef）≈ < 1us。
    /// 未命中：CreateTextFormat ~300us，插表后该 size 后续帧全命中。
    fn get_text_format(&self, font_size: f32) -> Option<IDWriteTextFormat> {
        let key: TextFormatKey = (font_size.clamp(1.0, 1024.0) * 4.0) as u32;
        let mut cache = self.text_format_cache.borrow_mut();
        if let Some(tf) = cache.get(&key) {
            return Some(tf.clone());
        }
        let tf = match unsafe {
            self.dwrite.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                font_size,
                w!("en-us"),
            )
        } {
            Ok(f) => f,
            Err(e) => {
                crate::log::emit(4, &format!("CreateTextFormat failed: {}", e));
                return None;
            }
        };
        cache.insert(key, tf.clone());
        Some(tf)
    }

    /// 阶段 3.1：按 RGBA8 量化拿/造 SolidColorBrush。
    ///
    /// premultiplied alpha 路径下颜色范围 [0, 1]；超出会被 clamp。
    fn get_brush(&self, color: [f32; 4]) -> Option<ID2D1SolidColorBrush> {
        let key: BrushKey = [
            (color[0].clamp(0.0, 1.0) * 255.0) as u8,
            (color[1].clamp(0.0, 1.0) * 255.0) as u8,
            (color[2].clamp(0.0, 1.0) * 255.0) as u8,
            (color[3].clamp(0.0, 1.0) * 255.0) as u8,
        ];
        let mut cache = self.brush_cache.borrow_mut();
        if let Some(b) = cache.get(&key) {
            return Some(b.clone());
        }
        let c = D2D1_COLOR_F {
            r: color[0],
            g: color[1],
            b: color[2],
            a: color[3],
        };
        let brush = match unsafe { self.dc.CreateSolidColorBrush(&c, None) } {
            Ok(b) => b,
            Err(e) => {
                crate::log::emit(4, &format!("CreateSolidColorBrush failed: {}", e));
                return None;
            }
        };
        cache.insert(key, brush.clone());
        Some(brush)
    }

    /// resize / device 重置时调（暂未触发；预留给将来 device-lost 恢复）
    #[allow(dead_code)]
    pub(crate) fn flush_cache(&self) {
        self.text_format_cache.borrow_mut().clear();
        self.brush_cache.borrow_mut().clear();
    }
}

/// 每帧的高层绘制 API。生命周期仅在 `Frame::render` 调用期间有效。
///
/// 设计理念：业务侧（库使用者）不该直接接触 COM 对象。Painter 把 D2D / DWrite
/// 包成"画家"风格 API：clear / draw_text / fill_rect。
pub struct Painter<'a> {
    engine: &'a D2DEngine,
    /// 当前 RT 像素尺寸，draw_text 默认 layout rect 用得到
    size: (u32, u32),
}

impl<'a> Painter<'a> {
    pub(crate) fn new(engine: &'a D2DEngine, size: (u32, u32)) -> Self {
        Self { engine, size }
    }

    /// 清屏到指定颜色（premultiplied alpha）。
    /// 透明色：clear([0.0, 0.0, 0.0, 0.0])
    pub fn clear(&mut self, color: [f32; 4]) {
        let c = D2D1_COLOR_F {
            r: color[0],
            g: color[1],
            b: color[2],
            a: color[3],
        };
        unsafe {
            self.engine.dc.Clear(Some(&c));
        }
    }

    /// 实心矩形。
    #[allow(dead_code)] // 公共 Painter API，业务侧未来用到
    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        let rect = D2D_RECT_F {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        };
        unsafe {
            self.engine.dc.FillRectangle(&rect, &brush);
        }
    }

    /// 在 (x, y) 处绘制单行文本。layout rect 自动延伸到 RT 右下角。
    /// 字体硬编码 "Segoe UI"，可通过参数化扩展。
    pub fn draw_text(&mut self, text: &str, x: f32, y: f32, font_size: f32, color: [f32; 4]) {
        if text.is_empty() {
            return;
        }
        let wtext: Vec<u16> = text.encode_utf16().collect();

        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        let text_format = match self.engine.get_text_format(font_size) {
            Some(f) => f,
            None => return,
        };

        let layout = D2D_RECT_F {
            left: x,
            top: y,
            right: self.size.0 as f32,
            bottom: self.size.1 as f32,
        };

        unsafe {
            self.engine.dc.DrawText(
                &wtext,
                &text_format,
                &layout,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// 当前 RT 像素尺寸。业务侧用来算坐标 / 居中 / vw vh 等。
    #[allow(dead_code)]
    pub fn size(&self) -> (u32, u32) {
        self.size
    }
}
