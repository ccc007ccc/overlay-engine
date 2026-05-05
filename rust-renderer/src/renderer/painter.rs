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
//! + `match` 派发。这是为未来 v0.8+ 命令流（录制 / 回放 / 序列化 / debug 工具）铺路 —
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
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_POINT_2F, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1,
    ID2D1SolidColorBrush, ID2D1StrokeStyle, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_CAP_STYLE_FLAT,
    D2D1_DASH_STYLE_DASH, D2D1_DASH_STYLE_DASH_DOT, D2D1_DASH_STYLE_DOT, D2D1_DASH_STYLE_SOLID,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE,
    D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_LINE_JOIN_MITER,
    D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES1, D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
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
            stroke_style_cache: RefCell::new(HashMap::with_capacity(4)),
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
    /// 端点固定 FLAT，line join 固定 MITER。dash_style 不识别（< 0 或 > 3）→ 退到 SOLID。
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
            dashCap: D2D1_CAP_STYLE_FLAT,
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
        }
    }

    // -------- 私有实现：每个命令一个 do_* 方法 --------

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
            // DrawRectangle 返 Result，但 D2D 的 BeginDraw/EndDraw 模型里这种调用不会立刻失败，
            // 失败会被推迟到 EndDraw —— 所以丢 Result 是 D2D 一贯做法。
            let _ = self
                .engine
                .dc
                .DrawRectangle(&rect, &brush, stroke_width, style.as_ref());
        }
    }

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
            let _ = self.engine.dc.PushAxisAlignedClip(
                &rect,
                windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_ALIASED,
            );
        }
    }

    fn do_pop_clip(&mut self) {
        unsafe {
            let _ = self.engine.dc.PopAxisAlignedClip();
        }
    }

    fn do_set_transform(&mut self, m: [f32; 6]) {
        // m = [m11, m12, m21, m22, dx, dy]
        let mat = Matrix3x2 {
            M11: m[0],
            M12: m[1],
            M21: m[2],
            M22: m[3],
            M31: m[4],
            M32: m[5],
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
}
