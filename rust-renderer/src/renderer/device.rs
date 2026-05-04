//! D3D11 device 创建
//!
//! - feature level 退化序列：11_1 → 11_0
//! - HARDWARE driver type，启用 `BGRA_SUPPORT`（D2D 互操作必需）
//!
//! V1 offscreen 路径不需要 DXGI factory（直接用 device 创建纹理），
//! 所以这里把 factory 反查省了。V2 升级到 GPU shared texture 时再加回来。

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};

use crate::error::{RendererError, RendererResult};

/// GPU 资源容器：device + immediate context
///
/// 两者共同生命周期：随 `RendererState` 一起 drop。
/// `OffscreenSurface` 借走 device + context 的 clone（COM AddRef），所以即便
/// 这里 drop，纹理还能存活到 surface drop 才一起释放。
pub(crate) struct GpuDevice {
    pub(crate) device: ID3D11Device,
    pub(crate) context: ID3D11DeviceContext,
}

impl GpuDevice {
    pub(crate) fn create() -> RendererResult<Self> {
        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        // 安全：所有指针参数都是 stack 上的 Option 槽，
        // D3D11CreateDevice 写完即返回，不持有引用。
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(RendererError::DeviceInit)?;
        }

        let device = device.ok_or_else(|| {
            RendererError::DeviceInit(windows::core::Error::new(
                windows::core::HRESULT(0x80004005u32 as i32), // E_FAIL
                "D3D11CreateDevice returned null device",
            ))
        })?;
        let context = context.ok_or_else(|| {
            RendererError::DeviceInit(windows::core::Error::new(
                windows::core::HRESULT(0x80004005u32 as i32),
                "D3D11CreateDevice returned null context",
            ))
        })?;

        Ok(Self { device, context })
    }
}
