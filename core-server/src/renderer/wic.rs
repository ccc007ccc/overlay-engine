//! WIC（Windows Imaging Component）解码包装。
//!
//! ## 用途
//!
//! - PNG / JPG / BMP / GIF / WEBP（系统装了对应 codec 都行）→ ID2D1Bitmap1
//! - 通过 IWICFormatConverter 把任何输入像素格式转为 32bppPBGRA
//!   （premultiplied BGRA），与 D2D bitmap PREMULTIPLIED alpha 模式对齐
//!
//! ## 流程
//!
//! ```text
//! bytes &[u8]
//!   ↓ IWICStream::InitializeFromMemory
//! IWICStream
//!   ↓ IWICImagingFactory::CreateDecoderFromStream
//! IWICBitmapDecoder
//!   ↓ GetFrame(0)（多帧 GIF 暂只取第一帧）
//! IWICBitmapFrameDecode
//!   ↓ IWICFormatConverter::Initialize(PBGRA)
//! IWICFormatConverter (= IWICBitmapSource)
//!   ↓ ID2D1DeviceContext::CreateBitmapFromWicBitmap
//! ID2D1Bitmap1
//! ```
//!
//! ## 线程
//!
//! WIC factory 创建在 D2DEngine 里（与 D2D / DWrite factory 同生命周期）。
//! 调用方（D2DEngine）已被外层 Mutex 串行化，所以 WIC 调用线程安全靠这层兜底。
//!
//! ## COM 初始化
//!
//! `CoCreateInstance(CLSID_WICImagingFactory)` 必须先 CoInitialize。
//! D2DEngine::create 在首次构造时调一次 `CoInitializeEx(MULTITHREADED)`；
//! 已初始化（S_FALSE）当成功处理。

#![allow(non_snake_case)]

use windows::core::Interface;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmapSource,
    IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapPaletteTypeMedianCut,
    WICDecodeMetadataCacheOnLoad,
    // 注：IWICFormatConverter 类型在 CreateFormatConverter() 返回值上靠类型推断
    // 拿到，Initialize 是其 inherent method，不需要把类型名 import 进来。
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

use crate::error::{RendererError, RendererResult};

/// WIC factory + 一次性 COM init。
pub(crate) struct WicDecoder {
    factory: IWICImagingFactory,
}

impl WicDecoder {
    pub(crate) fn create() -> RendererResult<Self> {
        // CoInitializeEx 幂等：已初始化返 S_FALSE（HRESULT 1），不视为错误。
        // 注意 windows-rs 的 CoInitializeEx 返回 HRESULT 而不是 Result。
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            // RPC_E_CHANGED_MODE = 0x80010106 ：当前线程已用其他模式 init 了
            // （例如 STA），此时不该当致命错误 —— 我们在已 init 线程上跑也行。
            // S_FALSE = 1 ：本线程已 init，不算错。
            if hr.is_err() && hr.0 != 0x80010106u32 as i32 {
                return Err(RendererError::DeviceInit(windows::core::Error::from_hresult(
                    hr,
                )));
            }
        }

        let factory: IWICImagingFactory = unsafe {
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(RendererError::DeviceInit)?
        };
        crate::log::emit(2, "WicDecoder created (IWICImagingFactory ready)");
        Ok(Self { factory })
    }

    /// 从 byte 数组解码，返回 IWICBitmapSource（premultiplied BGRA8）。
    /// 调用方拿去喂 `ID2D1DeviceContext::CreateBitmapFromWicBitmap`。
    ///
    /// 多帧文件（GIF / WEBP 动图）只取第 0 帧。
    pub(crate) fn decode_to_pbgra(&self, bytes: &[u8]) -> RendererResult<IWICBitmapSource> {
        if bytes.is_empty() {
            return Err(RendererError::InvalidParam("empty image bytes"));
        }

        // 1) IWICStream 包 memory buffer
        let stream = unsafe {
            self.factory
                .CreateStream()
                .map_err(RendererError::DecodeFail)?
        };
        unsafe {
            // InitializeFromMemory 把 buffer 直接当 stream 内容（不复制）—— WIC 契约要求
            // 这块内存在 stream 仍被使用期间一直可读。本函数返回前 D2D 端
            // CreateBitmapFromWicBitmap 才真正读取像素（CopyPixels），所以调用方
            // load_bitmap_from_memory 持有的 `&[u8]` 生命周期覆盖了所有 WIC 读路径，安全。
            stream
                .InitializeFromMemory(bytes)
                .map_err(RendererError::DecodeFail)?;
        }

        // 2) IWICBitmapDecoder 自动嗅探格式（WICDecodeMetadataCacheOnLoad 仅 cache 元数据，
        //    像素仍是 lazy；下面 GetFrame 也只拿 frame metadata）
        let decoder = unsafe {
            self.factory
                .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnLoad)
                .map_err(RendererError::DecodeFail)?
        };

        // 3) 第 0 帧
        let frame = unsafe { decoder.GetFrame(0).map_err(RendererError::DecodeFail)? };

        // 4) IWICFormatConverter 转 PBGRA
        let converter = unsafe {
            self.factory
                .CreateFormatConverter()
                .map_err(RendererError::DecodeFail)?
        };
        let frame_src: IWICBitmapSource = frame.cast().map_err(RendererError::DecodeFail)?;
        unsafe {
            converter
                .Initialize(
                    &frame_src,
                    &GUID_WICPixelFormat32bppPBGRA,
                    WICBitmapDitherTypeNone,
                    None,
                    0.0,
                    WICBitmapPaletteTypeMedianCut,
                )
                .map_err(RendererError::DecodeFail)?;
        }

        converter.cast().map_err(RendererError::DecodeFail)
    }
}

// SAFETY: WIC factory 是 free-threaded（apartment-neutral），可跨线程访问。
// 我们的外层 Mutex 在 Renderer 边界做串行化，更稳妥。
unsafe impl Send for WicDecoder {}
unsafe impl Sync for WicDecoder {}
