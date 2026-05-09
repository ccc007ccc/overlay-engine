//! D2D + DWrite 文字与基础图形渲染（阶段 3：文字渲染基建 + 阶段 3.1 缓存 + v0.7 矢量图元）
//!
//! ## 设计
//! - D2DEngine：从已有 D3D11 device 派生 D2D Factory + Device + DC + DWrite Factory，
//!   一次创建终身复用（resize 不影响）。阶段 3.1 起内嵌 text_format / brush 缓存。
//!   v0.7 起加 stroke_style 缓存（dash 模式 4 种 × 端点形状）。
//! - Painter：每帧短生命周期的高层 API（clear / draw_text / fill_rect 等 v0.7 共 14 个命令），
//!   传给业务 Frame trait 实现，业务侧不直接接触 COM
//! - D2D Bitmap1：从 D3D11 RT (BIND_RENDER_TARGET + BIND_SHADER_RESOURCE) 通过
//!   IDXGISurface QI 包装而成，pool 大小同 RT 池（双 buffer）
//!
//! ## v0.7 命令分发
//!
//! 按 spec painter-abi-v0.7 第 10.5 节决策：painter 内部业务级操作通过 `enum DrawCmd`
//! 加 `match` 派发。这是为未来 v0.8+ 命令流（录制 / 回放 / 序列化 / debug 工具）铺路 ——
//! enum 直接是天然序列化对象，今天「绕一道弯」避免明天大重构。
//!
//! 老 3 个命令（clear / fill_rect / draw_text）保留直调入口（向前兼容、热路径），
//! 新 11 个命令统一走 `Painter::execute(DrawCmd)`。
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
//! ## 性能
//! - CreateBitmapFromDxgiSurface: 一次 + resize 时各一次，per-frame 0
//! - Per-frame: SetTarget (无成本) + BeginDraw + draw_*  + EndDraw
//! - draw_text 首帧：CreateTextFormat ~300us + CreateSolidColorBrush ~200us
//! - draw_text 后续帧：HashMap 命中 ~1us（u32 / [u8;4] key + clone COM ref）
//! - v0.7 stroke style 首次：CreateStrokeStyle ~50us，4 种 dash 模式上限即缓存满
//!
//! ## ABI 影响
//! Painter 类型自身不暴露给 C# —— 通过 swapchain.rs 的 cmd_* 间接桥接到 lib.rs C ABI。

#![allow(non_snake_case)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::ManuallyDrop;

use windows::core::{w, Interface};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_GRADIENT_STOP,
    D2D1_PIXEL_FORMAT, D2D_POINT_2F, D2D_RECT_F, D2D_RECT_U, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1,
    ID2D1GradientStopCollection, ID2D1LinearGradientBrush,
    ID2D1RadialGradientBrush, ID2D1SolidColorBrush, ID2D1StrokeStyle, D2D1_ARC_SEGMENT,
    D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_CAP_STYLE_FLAT, D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_DASH, D2D1_DASH_STYLE_DASH_DOT,
    D2D1_DASH_STYLE_DOT, D2D1_DASH_STYLE_SOLID, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE, D2D1_FACTORY_OPTIONS,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_INTERPOLATION_MODE_LINEAR,
    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES,
    D2D1_LINE_JOIN_MITER, D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES, D2D1_ROUNDED_RECT,
    D2D1_STROKE_STYLE_PROPERTIES1, D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
    D2D1_SWEEP_DIRECTION_CLOCKWISE, D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE,
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

use super::resources::{BitmapHandle, ResourceTable};
use super::wic::WicDecoder;

/// font_size 量化到 0.25 px 精度（size * 4）。Segoe UI / NORMAL weight / NORMAL style 固定。
type TextFormatKey = u32;
/// RGBA float [0, 1] 量化到 u8。alpha 也参与 key —— 同色不同透明度是不同 brush。
type BrushKey = [u8; 4];
/// v0.7 stroke style cache key = dash_style（0..=3）。端点形状 / line join 固定 FLAT / MITER。
type StrokeStyleKey = u8;

// ============================================================
// v0.7 dash_style 常量（与 ABI / spec 第 2.3.1 节一致）
// ============================================================
//
// `pub` 是给 Rust 内 demo / test / 业务模块用名字而不是裸数字。
// C# / 外部业务通过 i32 入参传递，不依赖这些常量名。

/// 实线（默认）
#[allow(dead_code)]
pub const DASH_STYLE_SOLID: i32 = 0;
/// 长划线
#[allow(dead_code)]
pub const DASH_STYLE_DASH: i32 = 1;
/// 点
#[allow(dead_code)]
pub const DASH_STYLE_DOT: i32 = 2;
/// 长划-点交替
#[allow(dead_code)]
pub const DASH_STYLE_DASH_DOT: i32 = 3;

// ============================================================
// v0.7 phase 2 bitmap format / interpolation 常量（与 ABI 一致）
// ============================================================

/// `renderer_create_texture` 用：BGRA8 premultiplied alpha（D2D 默认）
#[allow(dead_code)]
pub const TEXTURE_FORMAT_BGRA8: i32 = 0;
/// RGBA8 premultiplied alpha（业务方常见输入；内部转 BGRA8 上传）
#[allow(dead_code)]
pub const TEXTURE_FORMAT_RGBA8: i32 = 1;
/// NV12（视频常见，phase 2 暂只支持 Y plane 灰度，phase 3 视频接通时补全）
#[allow(dead_code)]
pub const TEXTURE_FORMAT_NV12: i32 = 2;

/// `renderer_draw_bitmap` 的 interpolation mode
#[allow(dead_code)]
pub const INTERP_NEAREST: i32 = 0;
#[allow(dead_code)]
pub const INTERP_LINEAR: i32 = 1;

// ============================================================
// v0.7 phase 5 path opcode 常量（spec §2.3.4）
// ============================================================
//
// Byte 流编码：业务一次性给一个 byte 流（Vec<u8>），Rust 端解码喂给 ID2D1PathGeometry。
// 0x06+ 全部 reserved（决策 10.1），遇到立刻报 UnsupportedFormat 不静默崩溃 —— 让
// 未来加新 opcode 时老二进制有明确报错。

#[allow(dead_code)]
pub const PATH_OP_MOVE_TO: u8 = 0x01;
#[allow(dead_code)]
pub const PATH_OP_LINE_TO: u8 = 0x02;
#[allow(dead_code)]
pub const PATH_OP_BEZIER: u8 = 0x03;
#[allow(dead_code)]
pub const PATH_OP_ARC: u8 = 0x04;
#[allow(dead_code)]
pub const PATH_OP_CLOSE: u8 = 0x05;

// ============================================================
// v0.7 phase 2 bitmap 资源类型
// ============================================================

/// 一个 bitmap slot 内部存的实体。所有来源（file / memory / 外部 texture）
/// 最终都是一个 ID2D1Bitmap1，painter 不区分来源。
///
/// 元数据（width / height / source kind）随 bitmap 同生命周期，便于 get_size
/// 和未来的 update_texture 校验。
pub(crate) struct BitmapResource {
    pub bitmap: ID2D1Bitmap1,
    pub width: u32,
    pub height: u32,
    /// 是否支持 update_texture（即创建时给了 CPU_READ + 没绑 RT）
    /// File / memory 加载的 bitmap 不允许 update_texture；只有 create_texture 创建的才允许
    pub updatable: bool,
}

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
    /// v0.7：D2D StrokeStyle 缓存。Stroke style 与 factory 绑定不与 dc/RT 绑定，跨 resize 长存。
    /// 仅 4 种 dash 模式（实线/划/点/划点），命中率接近 100%。
    stroke_style_cache: RefCell<HashMap<StrokeStyleKey, ID2D1StrokeStyle>>,

    /// v0.7 phase 2：WIC 解码器（PNG/JPG/...→ IWICBitmapSource）
    wic: WicDecoder,
    /// v0.7 phase 2：bitmap 资源表（slot 1024，ABA generation 防护）。
    /// `&self` 加载方法用 RefCell 内部可变。
    pub(crate) bitmaps: RefCell<ResourceTable<BitmapResource>>,
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

        // 5) v0.7 phase 2：WIC factory（CoInitializeEx 在内部一次性做）
        let wic = WicDecoder::create()?;

        crate::log::emit(
            2,
            "D2DEngine created (D2D1 + DirectWrite + WIC, BGRA8 premul, text/brush/bitmap caches enabled)",
        );

        Ok(Self {
            factory,
            device,
            dc,
            dwrite,
            text_format_cache: RefCell::new(HashMap::with_capacity(8)),
            brush_cache: RefCell::new(HashMap::with_capacity(16)),
            stroke_style_cache: RefCell::new(HashMap::with_capacity(4)),
            wic,
            bitmaps: RefCell::new(ResourceTable::new()),
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
        self.stroke_style_cache.borrow_mut().clear();
    }

    /// v0.7：按 dash_style 拿/造 ID2D1StrokeStyle。
    ///
    /// 端点（startCap/endCap）固定 FLAT；**dashCap 用 ROUND**——D2D 内置 DOT 模式
    /// 内部 dashes = [0, 2]，dash 长度 0 + dashCap=FLAT 会画 0 像素 → DOT/DASH_DOT 完全不可见。
    /// ROUND 让每个点延伸成直径 stroke_width 的圆，DASH_DOT 中的点也能正确显示。
    /// line join 固定 MITER。dash_style 不识别（< 0 或 > 3）→ 退到 SOLID。
    /// 命中：HashMap O(1) + clone（COM AddRef）≈ < 1us。
    /// 未命中：CreateStrokeStyle ~50us，4 种 dash 模式上限即缓存满。
    fn get_stroke_style(&self, dash_style: i32) -> Option<ID2D1StrokeStyle> {
        let key: StrokeStyleKey = match dash_style {
            1 => 1,
            2 => 2,
            3 => 3,
            _ => 0, // SOLID（含越界）
        };
        let mut cache = self.stroke_style_cache.borrow_mut();
        if let Some(s) = cache.get(&key) {
            return Some(s.clone());
        }
        let dash = match key {
            1 => D2D1_DASH_STYLE_DASH,
            2 => D2D1_DASH_STYLE_DOT,
            3 => D2D1_DASH_STYLE_DASH_DOT,
            _ => D2D1_DASH_STYLE_SOLID,
        };
        let props = D2D1_STROKE_STYLE_PROPERTIES1 {
            startCap: D2D1_CAP_STYLE_FLAT,
            endCap: D2D1_CAP_STYLE_FLAT,
            dashCap: D2D1_CAP_STYLE_ROUND,
            lineJoin: D2D1_LINE_JOIN_MITER,
            miterLimit: 10.0,
            dashStyle: dash,
            dashOffset: 0.0,
            transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
        };
        let style = match unsafe {
            // ID2D1Factory1::CreateStrokeStyle 返 ID2D1StrokeStyle1（带几何变换支持的子类）。
            // DrawLine / DrawRectangle 接受的是基类 ID2D1StrokeStyle —— 用 Interface::cast 拿基类引用。
            self.factory
                .CreateStrokeStyle(&props, None)
                .and_then(|s| s.cast::<ID2D1StrokeStyle>())
        } {
            Ok(s) => s,
            Err(e) => {
                crate::log::emit(4, &format!("CreateStrokeStyle failed: {}", e));
                return None;
            }
        };
        cache.insert(key, style.clone());
        Some(style)
    }

    // ============================================================
    // v0.7 phase 2 bitmap 资源 API（从 D2DEngine 角度）
    // ============================================================
    //
    // 设计：所有方法都用 `&self` + RefCell（bitmaps 字段）。
    // 调用方（Renderer / RendererState）通过外层 Mutex 串行化，所以 RefCell 借用安全。
    //
    // load_* 不需要 begin_frame —— 它们是「构造资源」操作，与帧无关。
    // draw_bitmap 才需要 begin_frame（在 Painter::execute 里走 DrawCmd::DrawBitmap）。

    /// 从内存字节解码（PNG/JPG/BMP/GIF/WEBP）→ ID2D1Bitmap1 → slot table。
    pub(crate) fn load_bitmap_from_memory(&self, bytes: &[u8]) -> RendererResult<BitmapHandle> {
        let wic_source = self.wic.decode_to_pbgra(bytes)?;

        // WIC source → D2D bitmap。CreateBitmapFromWicBitmap 自动 GPU 上传。
        // bitmap properties 必须显式给 PBGRA + premultiplied，与 WIC 输出对齐。
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: ManuallyDrop::new(None),
        };
        let bitmap = unsafe {
            self.dc
                .CreateBitmapFromWicBitmap(&wic_source, Some(&props))
                .map_err(RendererError::DecodeFail)?
        };

        // 拿尺寸
        let size = unsafe { bitmap.GetPixelSize() };
        let res = BitmapResource {
            bitmap,
            width: size.width,
            height: size.height,
            updatable: false,
        };
        self.bitmaps.borrow_mut().insert(res)
    }

    /// 创建一个空 bitmap，业务后续 update_texture 喂数据。
    /// 当前只支持 BGRA8 / RGBA8（NV12 phase 3 视频接通时补）。
    pub(crate) fn create_texture(
        &self,
        width: u32,
        height: u32,
        format: i32,
    ) -> RendererResult<BitmapHandle> {
        if width == 0 || height == 0 {
            return Err(RendererError::InvalidParam("zero size on create_texture"));
        }
        match format {
            x if x == TEXTURE_FORMAT_BGRA8 || x == TEXTURE_FORMAT_RGBA8 => {}
            x if x == TEXTURE_FORMAT_NV12 => {
                return Err(RendererError::UnsupportedFormat(
                    "NV12 not supported until phase 3 video pipeline",
                ));
            }
            _ => {
                return Err(RendererError::InvalidParam(
                    "unknown texture format constant",
                ));
            }
        }

        // 创建空 bitmap：D2D1_BITMAP_OPTIONS_NONE = 默认（CPU 可写）
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: ManuallyDrop::new(None),
        };
        let size = D2D_SIZE_U { width, height };
        let bitmap = unsafe {
            self.dc
                .CreateBitmap(size, None, 0, &props)
                .map_err(RendererError::DecodeFail)?
        };
        let res = BitmapResource {
            bitmap,
            width,
            height,
            updatable: true,
        };
        self.bitmaps.borrow_mut().insert(res)
    }

    /// 上传一帧像素到外部纹理。要求该 handle 是 create_texture 创建的（updatable=true）。
    /// stride = 每行字节数（含 padding）。RGBA8 输入会就地翻转 R/B 转 BGRA8。
    pub(crate) fn update_texture(
        &self,
        h: BitmapHandle,
        bytes: &[u8],
        stride: i32,
        format: i32,
    ) -> RendererResult<()> {
        if stride <= 0 {
            return Err(RendererError::InvalidParam("non-positive stride"));
        }
        // format 白名单 —— 不识别就拒，避免业务方拿 NV12 / 未知值「静默」走 BGRA 路径，
        // 上传出乱码画面后到处排查。NV12 留 phase 3 视频接通时补全。
        match format {
            x if x == TEXTURE_FORMAT_BGRA8 || x == TEXTURE_FORMAT_RGBA8 => {}
            x if x == TEXTURE_FORMAT_NV12 => {
                return Err(RendererError::UnsupportedFormat(
                    "NV12 update not supported until phase 3 video pipeline",
                ));
            }
            _ => {
                return Err(RendererError::InvalidParam(
                    "unknown texture format constant",
                ));
            }
        }
        let stride_u = stride as u32;

        let mut bitmaps = self.bitmaps.borrow_mut();
        let res = bitmaps.get_mut(h)?;
        if !res.updatable {
            return Err(RendererError::InvalidParam(
                "bitmap not updatable (loaded from file/memory, not create_texture)",
            ));
        }
        // BGRA8 / RGBA8 都是 4 bytes/pixel —— stride 至少要够装一行像素，否则
        // swizzle_rgba_to_bgra 会跨行读源 buffer，画面错位（而 buffer 总长够时不 panic，
        // 是肉眼难查的语义 bug）。
        let min_stride = (res.width as usize) * 4;
        if (stride_u as usize) < min_stride {
            return Err(RendererError::InvalidParam(
                "stride less than width * 4 bytes",
            ));
        }
        let expected = (res.height as usize).saturating_mul(stride_u as usize);
        if bytes.len() < expected {
            return Err(RendererError::InvalidParam(
                "bytes shorter than height * stride",
            ));
        }

        // RGBA8 → BGRA8 swizzle（CPU 端做，alloc 一份临时 buffer）。
        // 性能上每帧上传几兆数据时 swizzle ~纯内存带宽，不算瓶颈；将来可改 PS shader。
        let upload_buf: Vec<u8>;
        let upload_slice: &[u8] = if format == TEXTURE_FORMAT_RGBA8 {
            upload_buf = swizzle_rgba_to_bgra(bytes, res.width, res.height, stride_u);
            &upload_buf
        } else {
            // BGRA8：直接用
            bytes
        };

        let dst = D2D_RECT_U {
            left: 0,
            top: 0,
            right: res.width,
            bottom: res.height,
        };
        unsafe {
            res.bitmap
                .CopyFromMemory(
                    Some(&dst),
                    upload_slice.as_ptr() as *const _,
                    stride_u,
                )
                .map_err(RendererError::DecodeFail)?;
        }
        Ok(())
    }

    /// 取 bitmap 尺寸。失效 handle → ResourceNotFound。
    pub(crate) fn get_bitmap_size(&self, h: BitmapHandle) -> RendererResult<(u32, u32)> {
        let bitmaps = self.bitmaps.borrow();
        let res = bitmaps.get(h)?;
        Ok((res.width, res.height))
    }

    /// 显式释放 bitmap。已释放或失效 → ResourceNotFound（idempotent）。
    pub(crate) fn destroy_bitmap(&self, h: BitmapHandle) -> RendererResult<()> {
        // 拿出来 drop（COM ref 自动 Release）
        let _ = self.bitmaps.borrow_mut().remove(h)?;
        Ok(())
    }
}

/// CPU 端 RGBA8 → BGRA8 swizzle。每像素 4 字节。
/// 注意：bytes 可能比 width*4*height 大（stride 含 padding）—— 按 stride 行步进。
fn swizzle_rgba_to_bgra(bytes: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let row_bytes = (width as usize) * 4;
    let h = height as usize;
    let s = stride as usize;
    let mut out = vec![0u8; s * h];
    for y in 0..h {
        let src_row = &bytes[y * s..y * s + row_bytes];
        let dst_row = &mut out[y * s..y * s + row_bytes];
        for x in 0..(width as usize) {
            let i = x * 4;
            // R G B A → B G R A
            dst_row[i] = src_row[i + 2];
            dst_row[i + 1] = src_row[i + 1];
            dst_row[i + 2] = src_row[i];
            dst_row[i + 3] = src_row[i + 3];
        }
    }
    out
}

/// 每帧的高层绘制 API。生命周期仅在 `Frame::render` 调用期间有效。
///
/// 设计理念：业务侧（库使用者）不该直接接触 COM 对象。Painter 把 D2D / DWrite
/// 包成"画家"风格 API：clear / draw_text / fill_rect。
pub struct Painter<'a> {
    engine: &'a D2DEngine,
    /// 当前 RT 像素尺寸，draw_text 默认 layout rect 用得到
    size: (u32, u32),
    /// v0.7：begin_frame 设的 viewport (vp_x, vp_y)。`reset_transform` 恢复用。
    /// swapchain 在 begin_frame 里 SetTransform(translate(-vp_x, -vp_y))，
    /// painter 自己不发起 transform —— 但业务 set_transform 后想 reset 时
    /// 必须恢复成这个平移而不是 identity，否则坐标体系就漂了。
    viewport_origin: (f32, f32),
}

impl<'a> Painter<'a> {
    pub(crate) fn new(engine: &'a D2DEngine, size: (u32, u32)) -> Self {
        Self {
            engine,
            size,
            viewport_origin: (0.0, 0.0),
        }
    }

    /// v0.7：swapchain 在构造 Painter 后调一次，告知本帧的 viewport offset。
    /// 仅 reset_transform 用得到。
    pub(crate) fn set_viewport_origin(&mut self, vx: f32, vy: f32) {
        self.viewport_origin = (vx, vy);
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

// ============================================================
// v0.7 矢量图元 —— DrawCmd enum + execute 派发
// ============================================================
//
// 决策 spec 10.5：painter 内部业务级操作通过 enum + match 派发，为未来 v0.8+
// 命令流（录制 / 回放 / 序列化 / debug 工具）铺路。
//
// 老 3 个命令（clear / fill_rect / draw_text）保留直调入口（向前兼容、热路径），
// v0.7 新增的 11 个命令统一走 Painter::execute(DrawCmd)。

/// v0.7 业务级绘制命令。`Painter::execute` 一对一 dispatch 到 D2D 调用。
///
/// 命名约定与 spec 第 2.3 节 ABI 对齐 —— 这里是 Rust 内部表示，
/// 未来若要做命令流序列化，本 enum 即天然 schema。
#[derive(Debug, Clone)]
pub enum DrawCmd {
    /// 直线
    DrawLine {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        stroke_width: f32,
        rgba: [f32; 4],
        dash_style: i32,
    },
    /// 折线 / 闭合多边形
    DrawPolyline {
        points: Vec<(f32, f32)>,
        stroke_width: f32,
        rgba: [f32; 4],
        closed: bool,
    },
    /// 矩形描边
    StrokeRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        stroke_width: f32,
        rgba: [f32; 4],
    },
    /// 圆角矩形填充
    FillRoundedRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius_x: f32,
        radius_y: f32,
        rgba: [f32; 4],
    },
    /// 圆角矩形描边
    StrokeRoundedRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius_x: f32,
        radius_y: f32,
        stroke_width: f32,
        rgba: [f32; 4],
    },
    /// 椭圆填充（含正圆，rx == ry 时）
    FillEllipse {
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        rgba: [f32; 4],
    },
    /// 椭圆描边
    StrokeEllipse {
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        stroke_width: f32,
        rgba: [f32; 4],
    },
    /// 推矩形 clip
    PushClipRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
    /// 弹 clip 栈顶
    PopClip,
    /// 设置 2D 仿射变换（叠加在 viewport 平移之上）
    SetTransform {
        /// `[m11, m12, m21, m22, dx, dy]`，与 D2D Matrix3x2 一致
        matrix: [f32; 6],
    },
    /// 重置 transform 为「viewport 平移」（v0.6 默认）
    ResetTransform,
    /// 绘制 bitmap。bitmap 必须先通过 `load_bitmap_from_*` 或 `create_texture` 创建。
    /// `src_rect_*` 全 0 时 = 整个 bitmap。
    DrawBitmap {
        bitmap: BitmapHandle,
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
    },

    // ===== v0.7 phase 5 path + 渐变（spec §2.3.4 / §2.5） =====

    /// 填充任意路径（path opcode byte 流）。0x06+ → UnsupportedFormat。
    FillPath {
        path: Vec<u8>,
        rgba: [f32; 4],
    },
    /// 描边任意路径（同上）。dash_style 沿用 stroke_rect 系列。
    StrokePath {
        path: Vec<u8>,
        stroke_width: f32,
        rgba: [f32; 4],
        dash_style: i32,
    },
    /// 矩形 + 线性渐变。stops: [offset, r, g, b, a, ...]。premultiplied alpha。
    FillRectGradientLinear {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        start_x: f32,
        start_y: f32,
        end_x: f32,
        end_y: f32,
        stops: Vec<f32>,
    },
    /// 矩形 + 径向渐变。同上但中心 + 椭圆半径。
    FillRectGradientRadial {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        center_x: f32,
        center_y: f32,
        radius_x: f32,
        radius_y: f32,
        stops: Vec<f32>,
    },
}

impl<'a> Painter<'a> {
    /// v0.7 命令派发入口。所有新命令（11 个）经由这里。
    /// 老 3 个命令保留独立入口（性能 + 向前兼容）。
    ///
    /// 失败：单条命令的失败不抛错，只 log 跳过（与老路径行为一致 ——
    /// 一帧里某个 brush 创建失败不应该阻断整帧渲染）。
    pub fn execute(&mut self, cmd: &DrawCmd) {
        match cmd {
            DrawCmd::DrawLine {
                x0,
                y0,
                x1,
                y1,
                stroke_width,
                rgba,
                dash_style,
            } => self.do_draw_line(*x0, *y0, *x1, *y1, *stroke_width, *rgba, *dash_style),
            DrawCmd::DrawPolyline {
                points,
                stroke_width,
                rgba,
                closed,
            } => self.do_draw_polyline(points, *stroke_width, *rgba, *closed),
            DrawCmd::StrokeRect {
                x,
                y,
                w,
                h,
                stroke_width,
                rgba,
            } => self.do_stroke_rect(*x, *y, *w, *h, *stroke_width, *rgba),
            DrawCmd::FillRoundedRect {
                x,
                y,
                w,
                h,
                radius_x,
                radius_y,
                rgba,
            } => self.do_fill_rounded_rect(*x, *y, *w, *h, *radius_x, *radius_y, *rgba),
            DrawCmd::StrokeRoundedRect {
                x,
                y,
                w,
                h,
                radius_x,
                radius_y,
                stroke_width,
                rgba,
            } => self.do_stroke_rounded_rect(
                *x,
                *y,
                *w,
                *h,
                *radius_x,
                *radius_y,
                *stroke_width,
                *rgba,
            ),
            DrawCmd::FillEllipse {
                cx,
                cy,
                rx,
                ry,
                rgba,
            } => self.do_fill_ellipse(*cx, *cy, *rx, *ry, *rgba),
            DrawCmd::StrokeEllipse {
                cx,
                cy,
                rx,
                ry,
                stroke_width,
                rgba,
            } => self.do_stroke_ellipse(*cx, *cy, *rx, *ry, *stroke_width, *rgba),
            DrawCmd::PushClipRect { x, y, w, h } => self.do_push_clip_rect(*x, *y, *w, *h),
            DrawCmd::PopClip => self.do_pop_clip(),
            DrawCmd::SetTransform { matrix } => self.do_set_transform(*matrix),
            DrawCmd::ResetTransform => self.do_reset_transform(),
            DrawCmd::DrawBitmap {
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
            } => self.do_draw_bitmap(
                *bitmap,
                *src_x,
                *src_y,
                *src_w,
                *src_h,
                *dst_x,
                *dst_y,
                *dst_w,
                *dst_h,
                *opacity,
                *interp_mode,
            ),
            DrawCmd::FillPath { path, rgba } => self.do_fill_path(path, *rgba),
            DrawCmd::StrokePath {
                path,
                stroke_width,
                rgba,
                dash_style,
            } => self.do_stroke_path(path, *stroke_width, *rgba, *dash_style),
            DrawCmd::FillRectGradientLinear {
                x,
                y,
                w,
                h,
                start_x,
                start_y,
                end_x,
                end_y,
                stops,
            } => self.do_fill_rect_gradient_linear(
                *x, *y, *w, *h, *start_x, *start_y, *end_x, *end_y, stops,
            ),
            DrawCmd::FillRectGradientRadial {
                x,
                y,
                w,
                h,
                center_x,
                center_y,
                radius_x,
                radius_y,
                stops,
            } => self.do_fill_rect_gradient_radial(
                *x, *y, *w, *h, *center_x, *center_y, *radius_x, *radius_y, stops,
            ),
        }
    }

    // -------- 私有实现：每个命令一个 do_* 方法 --------

    #[allow(clippy::too_many_arguments)]
    fn do_draw_line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        stroke_width: f32,
        color: [f32; 4],
        dash_style: i32,
    ) {
        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        let style = self.engine.get_stroke_style(dash_style);
        let p0 = D2D_POINT_2F { x: x0, y: y0 };
        let p1 = D2D_POINT_2F { x: x1, y: y1 };
        unsafe {
            self.engine
                .dc
                .DrawLine(p0, p1, &brush, stroke_width, style.as_ref());
        }
    }

    fn do_draw_polyline(
        &mut self,
        points: &[(f32, f32)],
        stroke_width: f32,
        color: [f32; 4],
        closed: bool,
    ) {
        if points.len() < 2 {
            return;
        }
        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        let style = self.engine.get_stroke_style(DASH_STYLE_SOLID);
        // 折线用一组 DrawLine 拼出来 —— 比起为多边形单独建 PathGeometry，
        // 在点数较少（HUD 常见 < 20 点）时 DrawLine 更轻；点数多时再换 path 也不迟。
        for pair in points.windows(2) {
            let (x0, y0) = pair[0];
            let (x1, y1) = pair[1];
            unsafe {
                self.engine.dc.DrawLine(
                    D2D_POINT_2F { x: x0, y: y0 },
                    D2D_POINT_2F { x: x1, y: y1 },
                    &brush,
                    stroke_width,
                    style.as_ref(),
                );
            }
        }
        if closed {
            let (x0, y0) = *points.last().unwrap();
            let (x1, y1) = points[0];
            unsafe {
                self.engine.dc.DrawLine(
                    D2D_POINT_2F { x: x0, y: y0 },
                    D2D_POINT_2F { x: x1, y: y1 },
                    &brush,
                    stroke_width,
                    style.as_ref(),
                );
            }
        }
    }

    fn do_stroke_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        stroke_width: f32,
        color: [f32; 4],
    ) {
        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        let style = self.engine.get_stroke_style(DASH_STYLE_SOLID);
        let rect = D2D_RECT_F {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        };
        unsafe {
            // windows-rs 0.59 中 DrawRectangle 返 ()（D2D 的失败推迟到 EndDraw 才报）。
            self.engine
                .dc
                .DrawRectangle(&rect, &brush, stroke_width, style.as_ref());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn do_fill_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius_x: f32,
        radius_y: f32,
        color: [f32; 4],
    ) {
        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        let rr = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            },
            radiusX: radius_x,
            radiusY: radius_y,
        };
        unsafe {
            self.engine.dc.FillRoundedRectangle(&rr, &brush);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn do_stroke_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius_x: f32,
        radius_y: f32,
        stroke_width: f32,
        color: [f32; 4],
    ) {
        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        let style = self.engine.get_stroke_style(DASH_STYLE_SOLID);
        let rr = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            },
            radiusX: radius_x,
            radiusY: radius_y,
        };
        unsafe {
            self.engine
                .dc
                .DrawRoundedRectangle(&rr, &brush, stroke_width, style.as_ref());
        }
    }

    fn do_fill_ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, color: [f32; 4]) {
        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        let e = D2D1_ELLIPSE {
            point: D2D_POINT_2F { x: cx, y: cy },
            radiusX: rx,
            radiusY: ry,
        };
        unsafe {
            self.engine.dc.FillEllipse(&e, &brush);
        }
    }

    fn do_stroke_ellipse(
        &mut self,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        stroke_width: f32,
        color: [f32; 4],
    ) {
        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        let style = self.engine.get_stroke_style(DASH_STYLE_SOLID);
        let e = D2D1_ELLIPSE {
            point: D2D_POINT_2F { x: cx, y: cy },
            radiusX: rx,
            radiusY: ry,
        };
        unsafe {
            self.engine
                .dc
                .DrawEllipse(&e, &brush, stroke_width, style.as_ref());
        }
    }

    fn do_push_clip_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let rect = D2D_RECT_F {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        };
        unsafe {
            // 用 ALIASED 不开抗锯齿（clip 边缘走整像素，常见预期）。
            // 业务想要 antialiased 边缘可以用 transform + 矢量描边自己包。
            self.engine.dc.PushAxisAlignedClip(
                &rect,
                windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_ALIASED,
            );
        }
    }

    fn do_pop_clip(&mut self) {
        unsafe {
            self.engine.dc.PopAxisAlignedClip();
        }
    }

    fn do_set_transform(&mut self, m: [f32; 6]) {
        // m = [m11, m12, m21, m22, dx, dy]
        //
        // 关键:业务传入的矩阵作用于 canvas-space 坐标,但 D2D RT 是 viewport-local —
        // begin_frame 已经 SetTransform(translate(-vp_x, -vp_y)) 把 canvas → RT。
        // 业务调 set_transform 时必须保留这个 viewport translate,否则旋转/缩放后的
        // 命令会丢掉 viewport 偏移,在 widget 缩小/移位后(vp_x/vp_y 非零)位置漂走。
        //
        // D2D 行向量约定:final_point = src_point ∗ M_business ∗ T_viewport。
        // 复合矩阵 M_business ∗ T_viewport 仅平移分量受 T_vp 影响:dx → dx - vp_x。
        let (vx, vy) = self.viewport_origin;
        let mat = Matrix3x2 {
            M11: m[0],
            M12: m[1],
            M21: m[2],
            M22: m[3],
            M31: m[4] - vx,
            M32: m[5] - vy,
        };
        unsafe {
            self.engine.dc.SetTransform(&mat);
        }
    }

    fn do_reset_transform(&mut self) {
        // 还原成 viewport 平移 —— 不能 identity，因为 begin_frame 内部已 SetTransform(translate(-vp_x, -vp_y))。
        // 这里通过 swapchain 透传当前 viewport offset；painter 自己不知道 viewport。
        // 妥协方案：painter 记录 begin_frame 时传入的 viewport offset，reset 时恢复。
        // 见 Painter::set_viewport_origin。
        let (vx, vy) = self.viewport_origin;
        let mat = Matrix3x2 {
            M11: 1.0,
            M12: 0.0,
            M21: 0.0,
            M22: 1.0,
            M31: -vx,
            M32: -vy,
        };
        unsafe {
            self.engine.dc.SetTransform(&mat);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn do_draw_bitmap(
        &mut self,
        bitmap_handle: BitmapHandle,
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
    ) {
        // 借出 bitmap。bitmaps RefCell 借用持续 unsafe DrawBitmap 调用 —— 短作用域无重入。
        let bitmaps = self.engine.bitmaps.borrow();
        let res = match bitmaps.get(bitmap_handle) {
            Ok(r) => r,
            Err(_) => {
                crate::log::emit(
                    4,
                    &format!("draw_bitmap: handle {:#x} not found / expired", bitmap_handle),
                );
                return;
            }
        };

        // src_rect 全 0 视为整图
        let src_rect_opt = if src_x == 0.0 && src_y == 0.0 && src_w == 0.0 && src_h == 0.0 {
            None
        } else {
            Some(D2D_RECT_F {
                left: src_x,
                top: src_y,
                right: src_x + src_w,
                bottom: src_y + src_h,
            })
        };
        let dst_rect = D2D_RECT_F {
            left: dst_x,
            top: dst_y,
            right: dst_x + dst_w,
            bottom: dst_y + dst_h,
        };

        let interp = if interp_mode == INTERP_NEAREST {
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
        } else {
            D2D1_INTERPOLATION_MODE_LINEAR
        };

        unsafe {
            self.engine.dc.DrawBitmap(
                &res.bitmap,
                Some(&dst_rect),
                opacity.clamp(0.0, 1.0),
                interp,
                src_rect_opt.as_ref().map(|r| r as *const _),
                None, // perspective transform
            );
        }
    }

    // ============================================================
    // v0.7 phase 5：path + 渐变 实现
    // ============================================================

    /// 解码 path byte 流 → ID2D1PathGeometry。BeginFigure 用 figure_begin（FILLED/HOLLOW），
    /// figure 由 0x05 CLOSE 触发 EndFigure(CLOSED)，否则 path 末尾自动 EndFigure(OPEN)。
    ///
    /// 失败处理：path 解码错误 / 0x06+ 不在这里报 —— 调用前由 swapchain 层
    /// `validate_path_bytes` 校验。这里假设 path 已合法；遇到意外仍尽量画完不 panic。
    fn build_path_geometry(
        &self,
        path: &[u8],
        figure_begin: windows::Win32::Graphics::Direct2D::Common::D2D1_FIGURE_BEGIN,
    ) -> Option<windows::Win32::Graphics::Direct2D::ID2D1PathGeometry1> {
        let geom = unsafe {
            match self.engine.factory.CreatePathGeometry() {
                Ok(g) => g,
                Err(e) => {
                    crate::log::emit(4, &format!("CreatePathGeometry failed: {}", e));
                    return None;
                }
            }
        };
        let sink: windows::Win32::Graphics::Direct2D::ID2D1GeometrySink = unsafe {
            match geom.Open() {
                Ok(s) => s,
                Err(e) => {
                    crate::log::emit(4, &format!("PathGeometry.Open failed: {}", e));
                    return None;
                }
            }
        };

        // 解码状态机：
        //   in_figure = false：必须先 MOVE_TO 才能进 figure
        //   in_figure = true ：可以 LINE_TO / BEZIER / ARC / CLOSE / 新的 MOVE_TO（隐式 EndFigure(OPEN)）
        let mut i = 0usize;
        let mut in_figure = false;
        let mut current = D2D_POINT_2F { x: 0.0, y: 0.0 };

        // 小工具：从 byte 流读 N 个 f32（小端，与 host 一致）
        let read_f32 = |bytes: &[u8], off: usize| -> f32 {
            let mut buf = [0u8; 4];
            buf.copy_from_slice(&bytes[off..off + 4]);
            f32::from_le_bytes(buf)
        };

        while i < path.len() {
            let op = path[i];
            i += 1;
            match op {
                PATH_OP_MOVE_TO => {
                    // 先关闭旧 figure
                    if in_figure {
                        unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
                    }
                    let x = read_f32(path, i);
                    let y = read_f32(path, i + 4);
                    i += 8;
                    current = D2D_POINT_2F { x, y };
                    unsafe { sink.BeginFigure(current, figure_begin) };
                    in_figure = true;
                }
                PATH_OP_LINE_TO => {
                    let x = read_f32(path, i);
                    let y = read_f32(path, i + 4);
                    i += 8;
                    if in_figure {
                        unsafe { sink.AddLine(D2D_POINT_2F { x, y }) };
                        current = D2D_POINT_2F { x, y };
                    }
                }
                PATH_OP_BEZIER => {
                    let x1 = read_f32(path, i);
                    let y1 = read_f32(path, i + 4);
                    let x2 = read_f32(path, i + 8);
                    let y2 = read_f32(path, i + 12);
                    let x3 = read_f32(path, i + 16);
                    let y3 = read_f32(path, i + 20);
                    i += 24;
                    if in_figure {
                        let seg =
                            windows::Win32::Graphics::Direct2D::Common::D2D1_BEZIER_SEGMENT {
                                point1: D2D_POINT_2F { x: x1, y: y1 },
                                point2: D2D_POINT_2F { x: x2, y: y2 },
                                point3: D2D_POINT_2F { x: x3, y: y3 },
                            };
                        unsafe { sink.AddBezier(&seg as *const _) };
                        current = D2D_POINT_2F { x: x3, y: y3 };
                    }
                }
                PATH_OP_ARC => {
                    let x = read_f32(path, i);
                    let y = read_f32(path, i + 4);
                    let rx = read_f32(path, i + 8);
                    let ry = read_f32(path, i + 12);
                    let rotation = read_f32(path, i + 16);
                    let large_arc = path[i + 20];
                    let sweep = path[i + 21];
                    i += 22;
                    if in_figure {
                        let seg = D2D1_ARC_SEGMENT {
                            point: D2D_POINT_2F { x, y },
                            size: windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F {
                                width: rx,
                                height: ry,
                            },
                            rotationAngle: rotation,
                            sweepDirection: if sweep != 0 {
                                D2D1_SWEEP_DIRECTION_CLOCKWISE
                            } else {
                                D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
                            },
                            arcSize: if large_arc != 0 {
                                D2D1_ARC_SIZE_LARGE
                            } else {
                                D2D1_ARC_SIZE_SMALL
                            },
                        };
                        unsafe { sink.AddArc(&seg as *const _) };
                        current = D2D_POINT_2F { x, y };
                    }
                }
                PATH_OP_CLOSE => {
                    if in_figure {
                        unsafe { sink.EndFigure(D2D1_FIGURE_END_CLOSED) };
                        in_figure = false;
                    }
                }
                _ => {
                    // 已被 validate_path_bytes 过滤，理论走不到 —— 防御加 log。
                    crate::log::emit(4, &format!("path: unknown opcode 0x{:02X}", op));
                    break;
                }
            }
        }
        // path 末尾还在 figure 内 → EndFigure(OPEN)
        if in_figure {
            unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
        }
        unsafe {
            if let Err(e) = sink.Close() {
                crate::log::emit(4, &format!("GeometrySink.Close failed: {}", e));
                return None;
            }
        }
        let _ = current; // 未来可能加 quad bezier 时用到
        Some(geom)
    }

    fn do_fill_path(&mut self, path: &[u8], color: [f32; 4]) {
        let geom = match self.build_path_geometry(path, D2D1_FIGURE_BEGIN_FILLED) {
            Some(g) => g,
            None => return,
        };
        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        // FillGeometry: brush 必填（windows-rs IntoParam 自动从 SolidColorBrush
        // cast 到 ID2D1Brush），opacity_brush None。ID2D1RenderTarget 版返 ()。
        unsafe {
            self.engine.dc.FillGeometry(&geom, &brush, None);
        }
    }

    fn do_stroke_path(
        &mut self,
        path: &[u8],
        stroke_width: f32,
        color: [f32; 4],
        dash_style: i32,
    ) {
        let geom = match self.build_path_geometry(path, D2D1_FIGURE_BEGIN_HOLLOW) {
            Some(g) => g,
            None => return,
        };
        let brush = match self.engine.get_brush(color) {
            Some(b) => b,
            None => return,
        };
        let style = self.engine.get_stroke_style(dash_style);
        unsafe {
            self.engine
                .dc
                .DrawGeometry(&geom, &brush, stroke_width, style.as_ref());
        }
    }

    /// 把 [offset, r, g, b, a, ...] 平铺数组转 D2D1_GRADIENT_STOP 数组。
    /// 返 None 表示 stops 不合法（数量、offset 范围）—— 调用方 swapchain 层会先校验，
    /// 这里再防一道。
    fn build_gradient_stops(&self, stops: &[f32]) -> Option<Vec<D2D1_GRADIENT_STOP>> {
        if stops.len() < 10 || stops.len() % 5 != 0 {
            return None;
        }
        let n = stops.len() / 5;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let o = i * 5;
            let offset = stops[o].clamp(0.0, 1.0);
            let r = stops[o + 1];
            let g = stops[o + 2];
            let b = stops[o + 3];
            let a = stops[o + 4];
            out.push(D2D1_GRADIENT_STOP {
                position: offset,
                color: D2D1_COLOR_F { r, g, b, a },
            });
        }
        Some(out)
    }

    fn build_gradient_collection(
        &self,
        stops: &[f32],
    ) -> Option<ID2D1GradientStopCollection> {
        let arr = self.build_gradient_stops(stops)?;
        // dc 是 ID2D1DeviceContext。其上的 CreateGradientStopCollection 是 6 参版返
        // ID2D1GradientStopCollection1（子接口）。windows-rs Interface trait 的
        // .cast() 转回基础接口供后续 brush API 使用。
        unsafe {
            use windows::Win32::Graphics::Direct2D::{
                D2D1_BUFFER_PRECISION_8BPC_UNORM, D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED,
                D2D1_COLOR_SPACE_SRGB, D2D1_EXTEND_MODE_CLAMP,
            };
            let coll1 = match self.engine.dc.CreateGradientStopCollection(
                &arr,
                D2D1_COLOR_SPACE_SRGB,
                D2D1_COLOR_SPACE_SRGB,
                D2D1_BUFFER_PRECISION_8BPC_UNORM,
                D2D1_EXTEND_MODE_CLAMP,
                D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED,
            ) {
                Ok(c) => c,
                Err(e) => {
                    crate::log::emit(4, &format!("CreateGradientStopCollection failed: {}", e));
                    return None;
                }
            };
            // ID2D1GradientStopCollection1 → ID2D1GradientStopCollection (父接口)
            match coll1.cast::<ID2D1GradientStopCollection>() {
                Ok(c) => Some(c),
                Err(e) => {
                    crate::log::emit(4, &format!("cast collection1→collection: {}", e));
                    None
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn do_fill_rect_gradient_linear(
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
    ) {
        let coll = match self.build_gradient_collection(stops) {
            Some(c) => c,
            None => return,
        };
        let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
            startPoint: D2D_POINT_2F { x: sx, y: sy },
            endPoint: D2D_POINT_2F { x: ex, y: ey },
        };
        let brush: ID2D1LinearGradientBrush = unsafe {
            match self
                .engine
                .dc
                .CreateLinearGradientBrush(&props, None, &coll)
            {
                Ok(b) => b,
                Err(e) => {
                    crate::log::emit(4, &format!("CreateLinearGradientBrush failed: {}", e));
                    return;
                }
            }
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

    #[allow(clippy::too_many_arguments)]
    fn do_fill_rect_gradient_radial(
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
    ) {
        let coll = match self.build_gradient_collection(stops) {
            Some(c) => c,
            None => return,
        };
        let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
            center: D2D_POINT_2F { x: cx, y: cy },
            gradientOriginOffset: D2D_POINT_2F { x: 0.0, y: 0.0 },
            radiusX: rx,
            radiusY: ry,
        };
        let brush: ID2D1RadialGradientBrush = unsafe {
            match self
                .engine
                .dc
                .CreateRadialGradientBrush(&props, None, &coll)
            {
                Ok(b) => b,
                Err(e) => {
                    crate::log::emit(4, &format!("CreateRadialGradientBrush failed: {}", e));
                    return;
                }
            }
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
}
