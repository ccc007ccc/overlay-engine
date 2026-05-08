//! D3D11 / DComp / CompositionSwapchain 公共封装。
//!
//! 本 spike 明确区分：
//! - logical canvas：业务坐标系，默认对应屏幕/虚拟桌面物理坐标，不要求固定分辨率或比例
//! - render resolution：producer 实际渲染 texture 分辨率，可任意设置；只影响清晰度/模糊度
//! - consumer viewport：consumer 窗口在屏幕上的 client 矩形，决定看 logical canvas 的哪一块

use windows::core::{IUnknown, Interface, Result};
use windows::Win32::Foundation::{HANDLE, HMODULE, RECT};
use windows::Win32::Graphics::CompositionSwapchain::{
    CreatePresentationFactory, IPresentationBuffer, IPresentationFactory, IPresentationManager,
    IPresentationSurface,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
    ID3D11Resource, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_DISPLAYABLE, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, DCompositionCreateSurfaceHandle, IDCompositionDesktopDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};

pub const COMPOSITIONOBJECT_ALL_ACCESS: u32 = 0x0003;

#[derive(Clone, Copy, Debug)]
pub struct CanvasMeta {
    pub logical_w: u32,
    pub logical_h: u32,
    pub render_w: u32,
    pub render_h: u32,
}

impl CanvasMeta {
    pub fn to_payload(self, handle: HANDLE) -> [u8; 24] {
        let mut out = [0u8; 24];
        out[0..8].copy_from_slice(&(handle.0 as u64).to_le_bytes());
        out[8..12].copy_from_slice(&self.logical_w.to_le_bytes());
        out[12..16].copy_from_slice(&self.logical_h.to_le_bytes());
        out[16..20].copy_from_slice(&self.render_w.to_le_bytes());
        out[20..24].copy_from_slice(&self.render_h.to_le_bytes());
        out
    }

    pub fn from_payload(buf: [u8; 24]) -> (HANDLE, Self) {
        let handle = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let logical_w = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let logical_h = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        let render_w = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        let render_h = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        (
            HANDLE(handle as *mut _),
            Self { logical_w, logical_h, render_w, render_h },
        )
    }

    pub fn render_to_logical_scale_x(self) -> f32 {
        self.logical_w as f32 / self.render_w as f32
    }

    pub fn render_to_logical_scale_y(self) -> f32 {
        self.logical_h as f32 / self.render_h as f32
    }
}

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
    pub meta: CanvasMeta,
}

pub fn producer_create(d3d: &ID3D11Device, meta: CanvasMeta) -> Result<ProducerSurface> {
    eprintln!("[dcomp] CreatePresentationFactory");
    let factory: IPresentationFactory = unsafe { CreatePresentationFactory(d3d)? };
    eprintln!("[dcomp] CreatePresentationManager");
    let manager: IPresentationManager = unsafe { factory.CreatePresentationManager()? };
    eprintln!("[dcomp] DCompositionCreateSurfaceHandle");
    let handle = unsafe { DCompositionCreateSurfaceHandle(COMPOSITIONOBJECT_ALL_ACCESS, None)? };
    eprintln!("[dcomp] CreatePresentationSurface");
    let surface: IPresentationSurface = unsafe { manager.CreatePresentationSurface(handle)? };
    unsafe {
        surface.SetAlphaMode(DXGI_ALPHA_MODE_IGNORE)?;
        let src = RECT {
            left: 0,
            top: 0,
            right: meta.render_w as i32,
            bottom: meta.render_h as i32,
        };
        surface.SetSourceRect(&src)?;
    }

    eprintln!("[dcomp] CreateTexture2D {}x{}", meta.render_w, meta.render_h);
    let misc_flags = D3D11_RESOURCE_MISC_SHARED.0
        | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
        | D3D11_RESOURCE_MISC_SHARED_DISPLAYABLE.0;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: meta.render_w,
        Height: meta.render_h,
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
    let texture = texture.ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "tex null"))?;

    let mut rtv: Option<ID3D11RenderTargetView> = None;
    unsafe { d3d.CreateRenderTargetView(&texture, None, Some(&mut rtv))? };
    let rtv = rtv.ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "rtv null"))?;

    eprintln!("[dcomp] AddBufferFromResource");
    let texture_unk: IUnknown = texture.cast()?;
    let buffer: IPresentationBuffer = unsafe { manager.AddBufferFromResource(&texture_unk)? };

    Ok(ProducerSurface { handle, manager, surface, buffer, texture, rtv, meta })
}

pub fn producer_present_grid(
    d3d_ctx: &ID3D11DeviceContext,
    p: &ProducerSurface,
    phase: u32,
) -> Result<()> {
    let pixels = generate_grid_bgra(p.meta, phase);
    let resource: ID3D11Resource = p.texture.cast()?;
    unsafe {
        d3d_ctx.UpdateSubresource(
            &resource,
            0,
            None,
            pixels.as_ptr() as *const _,
            p.meta.render_w * 4,
            p.meta.render_w * p.meta.render_h * 4,
        );
        d3d_ctx.Flush();
        p.surface.SetBuffer(&p.buffer)?;
        p.manager.Present()?;
    }
    Ok(())
}

fn generate_grid_bgra(meta: CanvasMeta, _phase: u32) -> Vec<u8> {
    let mut pixels = vec![0u8; (meta.render_w * meta.render_h * 4) as usize];
    let sx = meta.logical_w as f32 / meta.render_w as f32;
    let sy = meta.logical_h as f32 / meta.render_h as f32;
    let center_x = meta.logical_w as f32 * 0.5;
    let center_y = meta.logical_h as f32 * 0.5;

    for ry in 0..meta.render_h {
        let ly = (ry as f32 + 0.5) * sy;
        for rx in 0..meta.render_w {
            let lx = (rx as f32 + 0.5) * sx;
            let idx = ((ry * meta.render_w + rx) * 4) as usize;

            let mut r = ((lx / meta.logical_w as f32) * 60.0 + 20.0) as u8;
            let mut g = ((ly / meta.logical_h as f32) * 60.0 + 20.0) as u8;
            let mut b = 45u8;

            let minor_x = (lx % 50.0).min(50.0 - (lx % 50.0));
            let minor_y = (ly % 50.0).min(50.0 - (ly % 50.0));
            let major_x = (lx % 250.0).min(250.0 - (lx % 250.0));
            let major_y = (ly % 250.0).min(250.0 - (ly % 250.0));

            if minor_x < sx.max(1.0) || minor_y < sy.max(1.0) {
                r = r.saturating_add(24);
                g = g.saturating_add(24);
                b = b.saturating_add(24);
            }
            if major_x < (sx * 2.0).max(2.0) || major_y < (sy * 2.0).max(2.0) {
                r = 190;
                g = 190;
                b = 190;
            }

            let center_thick_x = (sx * 3.0).max(3.0);
            let center_thick_y = (sy * 3.0).max(3.0);
            if (lx - center_x).abs() < center_thick_x {
                r = 255;
                g = 48;
                b = 48;
            }
            if (ly - center_y).abs() < center_thick_y {
                r = 48;
                g = 255;
                b = 48;
            }
            if (lx - center_x).abs() < center_thick_x && (ly - center_y).abs() < center_thick_y {
                r = 255;
                g = 255;
                b = 255;
            }

            pixels[idx + 0] = b;
            pixels[idx + 1] = g;
            pixels[idx + 2] = r;
            pixels[idx + 3] = 255;
        }
    }

    pixels
}

pub fn consumer_open_surface(
    dcomp: &IDCompositionDesktopDevice,
    h: HANDLE,
) -> Result<IUnknown> {
    unsafe { dcomp.CreateSurfaceFromHandle(h) }
}
