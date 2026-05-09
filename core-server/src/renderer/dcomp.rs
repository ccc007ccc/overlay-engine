use windows::core::{IUnknown, Interface, Result};
use windows::Win32::Foundation::{HANDLE, HMODULE, RECT};
use windows::Win32::Graphics::CompositionSwapchain::{
    CreatePresentationFactory, IPresentationBuffer, IPresentationFactory, IPresentationManager,
    IPresentationSurface,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_DISPLAYABLE,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, DCompositionCreateSurfaceHandle, IDCompositionDesktopDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};

pub const COMPOSITIONOBJECT_ALL_ACCESS: u32 = 0x0003;

pub struct CoreDevices {
    pub d3d: ID3D11Device,
    pub d3d_ctx: ID3D11DeviceContext,
    pub dcomp: IDCompositionDesktopDevice,
}

// These are COM pointers which are thread-safe in our context as long as we only use them
// for initialization or behind a lock. We mark them Send and Sync so we can store them in ServerState.
unsafe impl Send for CoreDevices {}
unsafe impl Sync for CoreDevices {}

impl CoreDevices {
    pub fn new() -> Result<Self> {
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
        let d3d = d3d_opt.ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "D3D11CreateDevice null"))?;
        let d3d_ctx = ctx_opt.ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "D3D11Context null"))?;
        let dxgi: IDXGIDevice = d3d.cast()?;
        let dcomp: IDCompositionDesktopDevice = unsafe { DCompositionCreateDevice2(&dxgi)? };

        Ok(Self { d3d, d3d_ctx, dcomp })
    }
}

pub struct CanvasResources {
    pub handle: HANDLE,
    pub manager: IPresentationManager,
    pub surface: IPresentationSurface,
    pub buffer: IPresentationBuffer,
    pub texture: ID3D11Texture2D,
    pub rtv: ID3D11RenderTargetView,
    pub render_w: u32,
    pub render_h: u32,
}

unsafe impl Send for CanvasResources {}
unsafe impl Sync for CanvasResources {}

impl CanvasResources {
    pub fn new(d3d: &ID3D11Device, render_w: u32, render_h: u32) -> Result<Self> {
        let factory: IPresentationFactory = unsafe { CreatePresentationFactory(d3d)? };
        let manager: IPresentationManager = unsafe { factory.CreatePresentationManager()? };
        let handle = unsafe { DCompositionCreateSurfaceHandle(COMPOSITIONOBJECT_ALL_ACCESS, None)? };
        let surface: IPresentationSurface = unsafe { manager.CreatePresentationSurface(handle)? };
        unsafe {
            surface.SetAlphaMode(DXGI_ALPHA_MODE_PREMULTIPLIED)?;
            let src = RECT {
                left: 0,
                top: 0,
                right: render_w as i32,
                bottom: render_h as i32,
            };
            surface.SetSourceRect(&src)?;
        }

        let misc_flags = D3D11_RESOURCE_MISC_SHARED.0
            | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
            | D3D11_RESOURCE_MISC_SHARED_DISPLAYABLE.0;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: render_w,
            Height: render_h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: misc_flags as u32,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe { d3d.CreateTexture2D(&desc, None, Some(&mut texture))? };
        let texture = texture.ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "CreateTexture2D failed"))?;

        let mut rtv: Option<ID3D11RenderTargetView> = None;
        unsafe { d3d.CreateRenderTargetView(&texture, None, Some(&mut rtv))? };
        let rtv = rtv.ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "CreateRenderTargetView failed"))?;

        let texture_unk: IUnknown = texture.cast()?;
        let buffer: IPresentationBuffer = unsafe { manager.AddBufferFromResource(&texture_unk)? };

        Ok(Self {
            handle,
            manager,
            surface,
            buffer,
            texture,
            rtv,
            render_w,
            render_h,
        })
    }

    pub fn present_color(&self, d3d_ctx: &ID3D11DeviceContext, rgba: [f32; 4]) -> Result<()> {
        unsafe {
            d3d_ctx.ClearRenderTargetView(&self.rtv, &rgba);
            d3d_ctx.Flush();
            self.surface.SetBuffer(&self.buffer)?;
            self.manager.Present()?;
            windows::Win32::System::Threading::SleepEx(0, true);
            while self.manager.GetNextPresentStatistics().is_ok() {}
        }
        Ok(())
    }
}
