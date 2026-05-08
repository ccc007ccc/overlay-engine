//! D3D11 / DComp / CompositionSwapchain 公共封装。
//!
//! Producer 写内容必须走 CompositionSwapchain（IPresentationManager），并且 buffer 用的
//! D3D11 texture 必须带 D3D11_RESOURCE_MISC_SHARED_NTHANDLE + KEYED_MUTEX flag。
//! Producer 写之前 keyed_mutex.AcquireSync(0)，写完 ReleaseSync(0)；Present 不需要再
//! 显式 release（PresentationManager 内部跟 GPU sync）。

use windows::core::{IUnknown, Interface, Result};
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::Graphics::CompositionSwapchain::{
    CreatePresentationFactory, IPresentationBuffer, IPresentationFactory, IPresentationManager,
    IPresentationSurface,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, DCompositionCreateSurfaceHandle, IDCompositionDesktopDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice, IDXGIKeyedMutex};

pub const COMPOSITIONOBJECT_ALL_ACCESS: u32 = 0x0003;

pub struct Devices {
    pub d3d: ID3D11Device,
    pub d3d_ctx: ID3D11DeviceContext,
    pub dcomp: IDCompositionDesktopDevice,
}

pub fn create_devices() -> Result<Devices> {
    let mut d3d_opt: Option<ID3D11Device> = None;
    let mut ctx_opt: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut d3d_opt),
            None,
            Some(&mut ctx_opt),
        )?;
    }
    let d3d = d3d_opt.ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "d3d null"))?;
    let d3d_ctx = ctx_opt.ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "ctx null"))?;
    let dxgi: IDXGIDevice = d3d.cast()?;
    let dcomp: IDCompositionDesktopDevice = unsafe { DCompositionCreateDevice2(&dxgi)? };
    Ok(Devices { d3d, d3d_ctx, dcomp })
}

pub struct ProducerSurface {
    pub handle: HANDLE,
    pub manager: IPresentationManager,
    pub surface: IPresentationSurface,
    pub buffer: IPresentationBuffer,
    pub texture: ID3D11Texture2D,
    pub rtv: ID3D11RenderTargetView,
    pub keyed_mutex: IDXGIKeyedMutex,
    pub width: u32,
    pub height: u32,
}

pub fn producer_create(d3d: &ID3D11Device, width: u32, height: u32) -> Result<ProducerSurface> {
    eprintln!("[dcomp] step 2.1: CreatePresentationFactory");
    let factory: IPresentationFactory = unsafe { CreatePresentationFactory(d3d)? };
    eprintln!("[dcomp] step 2.2: CreatePresentationManager");
    let manager: IPresentationManager = unsafe { factory.CreatePresentationManager()? };
    eprintln!("[dcomp] step 2.3: DCompositionCreateSurfaceHandle");
    let handle = unsafe { DCompositionCreateSurfaceHandle(COMPOSITIONOBJECT_ALL_ACCESS, None)? };
    eprintln!("[dcomp] step 2.4: CreatePresentationSurface");
    let surface: IPresentationSurface = unsafe { manager.CreatePresentationSurface(handle)? };

    eprintln!("[dcomp] step 2.5: CreateTexture2D (NTHANDLE | KEYEDMUTEX)");
    let misc_flags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: misc_flags as u32,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { d3d.CreateTexture2D(&desc, None, Some(&mut texture))? };
    let texture = texture.ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "tex null"))?;

    eprintln!("[dcomp] step 2.6: CreateRenderTargetView");
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    unsafe { d3d.CreateRenderTargetView(&texture, None, Some(&mut rtv))? };
    let rtv = rtv.ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "rtv null"))?;

    eprintln!("[dcomp] step 2.7: cast IDXGIKeyedMutex");
    let keyed_mutex: IDXGIKeyedMutex = texture.cast()?;

    eprintln!("[dcomp] step 2.8: AddBufferFromResource");
    let texture_unk: IUnknown = texture.cast()?;
    let buffer: IPresentationBuffer = unsafe { manager.AddBufferFromResource(&texture_unk)? };

    Ok(ProducerSurface {
        handle, manager, surface, buffer, texture, rtv, keyed_mutex, width, height,
    })
}

/// Producer：keyed_mutex.AcquireSync(0) → ClearRenderTargetView → ReleaseSync(0) → SetBuffer → Present
pub fn producer_present_color(
    d3d_ctx: &ID3D11DeviceContext,
    p: &ProducerSurface,
    rgba: [f32; 4],
) -> Result<()> {
    unsafe {
        // Acquire key 0（producer 持有），写完 release 0（让 compositor 拿）。
        // INFINITE = 0xFFFFFFFF（Microsoft 文档：使用 INFINITE 等待无限期）。
        p.keyed_mutex.AcquireSync(0, u32::MAX)?;
        d3d_ctx.ClearRenderTargetView(&p.rtv, &rgba);
        p.keyed_mutex.ReleaseSync(0)?;
        p.surface.SetBuffer(&p.buffer)?;
        p.manager.Present()?;
    }
    Ok(())
}

pub fn consumer_open_surface(
    dcomp: &IDCompositionDesktopDevice,
    h: HANDLE,
) -> Result<IUnknown> {
    unsafe { dcomp.CreateSurfaceFromHandle(h) }
}
