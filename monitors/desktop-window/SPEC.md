# Desktop Window Monitor SPEC v1.0

> **状态**：DRAFT（等待用户审定）
> **日期**：2026-05
> **依赖**：renderer.dll v0.7-phase2 起的 30 个 ABI；可与未来 phase 3-5 一同扩展
> **定位**：renderer.dll 的**第二种 monitor 实现**，与 `monitors/game-bar-widget/`（即原 csharp-shell）
> 并列。这是一个完整的桌面窗口形态 monitor（双击 .exe 就跑），同时内置一个 demo
> gallery 让人肉眼对照每个 ABI 的行为——所以也能顺便当 dogfood / 调试工具用。
> 同时为社区写更多 monitor 形态做范本。
> **语言**：**Rust**（与 renderer 同语言；纯 Rust 单 .exe 无 .NET / WinRT 依赖）

## 0. 决策摘要 (TL;DR)

| 项 | 决策 | 备注 |
|---|---|---|
| 项目位置 | `monitors/desktop-window/`，与 `monitors/game-bar-widget/` 平级 | 顶层 `Cargo.toml` 加 member |
| 语言 + GUI 框架 | **Rust + `winit`**（创建 HWND + 输入事件循环） | 不引入 egui / iced / NWG；UI 元素全部用 renderer.dll 自己画（dogfood 拉满） |
| 跟 renderer 集成方式 | **`libloading` 动态加载 `renderer.dll` 走 C ABI**，与未来社区 monitor 路径一致 | 不用 cargo workspace 的 rlib 直调，因为那样不验证 ABI 边界 |
| visual / 显示路径 | 直接 Win32 `IDCompositionDevice` + `IDCompositionTarget` + `IDCompositionVisual`，把 swap chain 挂到 winit HWND | **不用** WinRT 的 `Compositor` + `ICompositorInterop`（那是 WinRT/XAML 路径，纯 Rust 进程不该背 WinRT 依赖） |
| 画布与 viewport | **画布跟随窗口物理像素**（`winit::WindowEvent::Resized` → `renderer_resize_canvas(physical_w, physical_h)`）；业务用百分比布局 | 见 §13.7；任何窗口大小都是物理像素 1:1 渲染（点对点）；屏幕比例不一致由业务百分比布局自然适配 |
| 业务画图 | 每帧 `begin_frame` 出参拿当前 `(canvas_w, canvas_h)`，所有图元用百分比/相对位置 | 不写死像素坐标。renderer.dll 提供 `set_transform` 给业务自由实现 FixedVertical 等 ScalingMode |
| 窗口大小 | resizable，无固定上限；最小尺寸 480×320（防 UI 元素挤没） | 不像 v0 草稿那样固定 1280×720 |
| UI 渲染方式 | **immediate-mode self-rendering**：每帧在 begin_frame/end_frame 之间渲染 demo 画面 + 左侧虚拟 ListView + hover tooltip + 顶部 ExpectedSummary | 全部用 renderer 自己的 stroke_rect / fill_rect / draw_text 画；不用 OS 控件 |
| 内置 demo gallery | 25 个 demo 覆盖 30 个 ABI + 画布 resize 行为；左侧 ~20% 宽虚拟列表，鼠标 click 切换；hover 显示 tooltip | 同时是 monitor 的"演示模式"和 dogfood 工具 |
| 输入处理 | `winit::event::WindowEvent::CursorMoved` / `MouseInput` / `KeyboardInput` 由 monitor 自己 hit-test 哪个虚拟按钮被命中 | renderer 端零输入 ABI（输入与渲染解耦） |
| widget 端改造 | desktop-window monitor 上线后：(a) `RendererPInvoke.cs` 升级到 v0.7 ABI（`begin_frame` 出参 + `resize_canvas`）；(b) 删 `DrawPrimitivesShowcase` / `DrawPhase2Showcase` / `Phase2BitmapShowcase`；(c) `DrawDebugContent` 改用 `(canvas_w, canvas_h)` 百分比布局 | widget 回归生产 host 职责 + 跟 desktop-window 用同一套 API + 业务编码风格 |
| 不在范围 | 多 monitor 同进程同屏 / 虚拟屏切换 / 自动化 golden 比对 / 输入 ABI / 跨平台 | 见第 11 节 |

---

## 1. 背景与定位

### 1.1 Monitor 是什么

**核心架构观**：renderer.dll 是唯一的渲染引擎，"monitor" 是任何把它的画面显示出来
的载体。一个 monitor 进程 = 一个 `renderer_create` + 一个或多个 swap chain visual。
Monitor 形态是开放的，社区可以照着 ABI（`docs/spec/painter-abi-v0.7.md`）写：

| Monitor 形态 | 例子 | 状态 |
|---|---|---|
| Xbox Game Bar widget (UWP/WinRT) | `monitors/game-bar-widget/`（C#） | 已实现（v0.6 起） |
| Desktop window (Win32 + DComp) | 本 spec → `monitors/desktop-window/`（Rust） | 本 spec 实现 |
| 全屏 overlay (DXGI fullscreen) | 未来 | 未实现 |
| 嵌 WebView2 / CEF | 未来 | 未实现 |
| 命令行 / 截图工具 | 未来需要先加 readback ABI | 未实现 |

每种 monitor 自己决定：
- 用什么语言 + UI 框架（C# / Rust / C++ / Python / ...）
- canvas 多大（虚拟屏分辨率与物理设备无关）
- viewport 怎么传（屏幕坐标固定 / 监视器坐标固定 / 跟随窗口 / ...）
- 画什么（HUD / 视频 / 用户自定义脚本 / demo gallery / ...）

### 1.2 ABI 已经支持的所有 monitor 行为（不需要新加）

| 形态 | 实现路径 |
|---|---|
| 一个 monitor 单画面 | 单 `Renderer` + 单 swap chain |
| 多 monitor 同画面 | 多个进程各自 `renderer_create`，同步内容；或共享 swap chain handle（DXGI shared resource） |
| 多 monitor 各画面 | 多个 `Renderer` 实例，互不干扰 |
| 画布跟随物理像素（点对点） | host 在 `WindowEvent::Resized` 调 `renderer_resize_canvas(physical_w, physical_h)` —— desktop-window 默认行为 |
| 画布固定（DComp 自动拉伸） | host **不调** `resize_canvas`，画布永远 = `renderer_create` 时的尺寸 —— widget v0.6 现在的行为 |
| 用户面板选档位（720p / 1080p / 4K） | host 接 UI 事件 → `renderer_resize_canvas(选中档位)` |
| 屏幕坐标系固定 | `begin_frame(winScreenX, winScreenY, vpW, vpH)` |
| 监视器坐标系固定 | `begin_frame(0, 0, winW, winH)` —— 画面跟着 host 窗口走（desktop-window 默认） |
| 虚拟屏（分辨率与物理无关） | host 自己定义"虚拟分辨率"概念，传 `renderer_create(2560, 1440)` 后**不调** resize_canvas，DComp visual.Size 跟物理像素走 |
| 业务"FixedVertical 720"等 ScalingMode | 业务自己用 `set_transform` 加缩放矩阵；renderer 不内置 mode 概念（决策见 painter-abi-v0.7 §10.8） |

### 1.3 为什么 desktop-window 用 Rust 而不是 C#

1. **零运行时依赖**：纯 Rust .exe 不需要 .NET runtime / Windows App SDK runtime / MSIX 签名；`cargo build --release` 出产物双击就跑
2. **跟 renderer 同语言**：贡献者只要懂 Rust + 一点 Win32，不用切到 C# 心智模式
3. **真正的"社区 monitor"范本**：ABI 是 C extern，用任意语言绑都行；先做 Rust 范本验证 ABI 在 C# 之外的语言里也好用
4. **dogfood 强度更高**：UI 元素全部用 renderer 自己画（虚拟列表 + tooltip + 状态栏），等于 monitor 一上线就压测 renderer 矢量图元 + 文字 + clip 在真实 UI 场景下的稳定性
5. **DComp 直挂 HWND 比 WinRT Compositor 路径短**：`DCompositionCreateDevice2` → `IDCompositionTarget` + `SetRoot` 这条 Win32 原生路径在 Rust 里通过 windows-rs 直接调，不需要 WinRT activation context

### 1.4 为什么不用 egui / iced / NWG / WPF / WinUI 3

| 方案 | 否决理由 |
|---|---|
| egui + DComp 同窗口 | egui 自己用 wgpu 渲染，会跟 DComp swap chain 抢 visual layer；要么 egui 渲染到 swap chain（绕远）要么两个 swap chain（视觉层叠复杂） |
| iced | retained-mode UI，跟 immediate-mode 渲染循环阻抗不匹配；不是为外部 swap chain 集成设计 |
| NWG (native-windows-gui) | 是 Win32 controls 包装，但 Win32 ListView / Button 这些是 GDI 绘制，不能跟 DComp visual 在同一窗口和谐共存（GDI 是位图层，DComp 是 visual tree，混用有 z-order 翻车记录） |
| WPF / WinUI 3 | 用户明确指定 Rust |
| 裸 Win32 controls (HWND ListView) | 同 NWG 否决理由：GDI vs DComp 视觉层冲突 |

**结论**：immediate-mode self-rendering（用 renderer 自己画 UI）是唯一不冲突且最 dogfood 的路径。

---

## 2. 范围

### 2.1 在范围（v1.0）

- 单窗口（resizable，可拖动），无固定尺寸；最小尺寸 480×320
- **画布跟随物理像素**：`WindowEvent::Resized` → `renderer_resize_canvas(physical_w, physical_h)`
- **业务用百分比布局**：每帧 `begin_frame` 出参拿 `(canvas_w, canvas_h)`，UI 元素全部按相对比例画
- 25 个内置 demo 覆盖 30 个 ABI + 画布 resize 行为
- 左侧 ~20% 宽虚拟列表 ListView：每行 = 一个 demo 名 + group header（`row_h = canvas_h * 0.04` 等比例）
- Hover tooltip：鼠标悬浮列表项 → 在该项右侧弹出多行 tooltip
- 切换 demo：单击列表项；上/下箭头键切换；Enter / 数字键也支持
- 顶部一行 in-canvas 文本：当前 demo 的 ExpectedSummary（hover tooltip 的精简版）
- 状态栏（底部一行）：FPS / render_us / present_us / total_frames / last_error_status / **当前画布尺寸**
- 关闭窗口干净退出（teardown 当前 demo + renderer_destroy + 释放 DComp 资源）

### 2.2 不在范围（v1.0）

- 多 monitor 同进程同屏（保留 v1.1）
- 虚拟屏切换 / 画布 mode 切换 demo（v1.1 加"用户面板选 720p/1080p/4K"演示）
- 用户自定义内容（脚本 / 配置文件）—— 外部内容加载是独立 spec
- 自动化 golden pixel 比对（需要 readback ABI；painter-abi-v0.7 第 6 节"已知缺口"）
- 性能压测 / GPU profiler 集成
- 跨平台 / ARM64 / Linux

---

## 3. 项目结构

```
monitors/desktop-window/
├── SPEC.md                         (本文件)
├── README.md                       (build / 运行说明)
├── Cargo.toml                      (workspace member)
├── build.rs                        (可选：拷贝 renderer.dll 到 target 同目录)
├── assets/
│   └── test-image.png              (64×64 嵌入资源 — LoadFromMemory demo 用)
└── src/
    ├── main.rs                     (winit event loop + 主流程)
    ├── ffi.rs                      (libloading 加载 renderer.dll + 31 个 ABI 函数指针表，含 v0.7 resize_canvas / 改 begin_frame)
    ├── dcomp.rs                    (DCompositionDevice / Target / Visual + 把 swap chain 挂 HWND)
    ├── monitor.rs                  (DesktopMonitor: 60Hz tick + begin/end frame 调度 + Resized 事件 → resize_canvas + 当前 demo 渲染)
    ├── ui.rs                       (immediate-mode UI 渲染 + 命中测试，全部按 (canvas_w, canvas_h) 百分比布局)
    ├── input.rs                    (winit 事件 → UI 状态更新 + demo 切换)
    └── demos/
        ├── mod.rs                  (Demo trait + DemoRegistry；Demo::render 接收 (canvas_w, canvas_h))
        ├── v06_basics.rs           (Clear / FillRect / DrawText / Viewport / PerfStats)
        ├── phase1_lines.rs         (4 dash styles，含修过 dot 渲染 bug 后的可见效果验证)
        ├── phase1_rects.rs         (stroke / rounded fill+stroke)
        ├── phase1_ellipse.rs
        ├── phase1_clip.rs
        ├── phase1_transform.rs
        ├── phase1_state_errors.rs
        ├── phase2_create_texture.rs
        ├── phase2_update_bgra.rs
        ├── phase2_update_rgba.rs    (验证 swizzle)
        ├── phase2_interp.rs
        ├── phase2_src_rect.rs
        ├── phase2_opacity.rs
        ├── phase2_load_memory.rs    (嵌入 PNG)
        ├── phase2_lifecycle.rs
        ├── phase2_resource_limit.rs
        ├── phase2_decode_fail.rs
        └── canvas_resize.rs         (v0.7 新加：拖窗口验证画布跟随 + 永远居中布局)
```

```toml
# monitors/desktop-window/Cargo.toml
[package]
name = "desktop-window-monitor"
version = "0.1.0"
edition = "2021"

[dependencies]
winit = "0.30"
windows = { version = "0.59", features = [
    "Win32_Foundation",
    "Win32_Graphics_Direct3D",
    "Win32_Graphics_Direct3D11",
    "Win32_Graphics_DirectComposition",
    "Win32_Graphics_Dxgi",
    "Win32_Graphics_Dxgi_Common",
    "Win32_UI_WindowsAndMessaging",
    "Win32_UI_HiDpi",
] }
libloading = "0.8"
raw-window-handle = "0.6"
```

### 3.1 顶层 Cargo.toml workspace 改动

```toml
[workspace]
resolver = "2"
members = [
    "rust-renderer",
    "monitors/desktop-window",   # ← 新增
]
```

注意：`monitors/game-bar-widget/` 是 C# 项目，不进 cargo workspace。

---

## 4. 架构概览

```
┌──────────────────────────────────────────────────────────────────┐
│              winit::Window (HWND, resizable, min 480×320)        │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │     整个客户区 = 单 SwapChain 渲染面（画布 = 物理像素）  │   │
│  │  ┌────────────┐ ┌────────────────────────────────────┐  │   │
│  │  │ canvas_w   │ │    剩余宽度 = demo 渲染区          │  │   │
│  │  │  × 0.20    │ │    + 顶部 ExpectedSummary 文本     │  │   │
│  │  │ 虚拟       │ │    + 当前 demo Render() 结果       │  │   │
│  │  │ ListView   │ │                                     │  │   │
│  │  │            │ │                                     │  │   │
│  │  │ • Clear    │ │                                     │  │   │
│  │  │ • FillRect │ │                                     │  │   │
│  │  │ • Lines    │ │                                     │  │   │
│  │  │   (hover ▶)├──┐                                    │  │   │
│  │  │ • ...      │  │ tooltip                            │  │   │
│  │  └────────────┘  └────────────────────────────────────┘  │   │
│  │ ┌────────────────────────────────────────────────────┐   │   │
│  │ │ FPS: 59.8 | render: 142us | canvas: 1920×1080      │   │   │
│  │ └────────────────────────────────────────────────────┘   │   │
│  └──────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
                             │
                  IDCompositionVisual.SetContent(swap_chain)
                             │
                  IDCompositionTarget on HWND
                             ▼
┌──────────────────────────────────────────────────────────────────┐
│                       DesktopMonitor                             │
│  - libloading::Library::new("renderer.dll")                      │
│  - renderer_create(initial_w, initial_h) via fn ptr              │
│      初始 = winit 创建窗口时的物理像素                            │
│  - renderer_get_swapchain → IDXGISwapChain1 raw ptr              │
│  - DComp visual.SetContent(swap_chain)                           │
│  - winit WindowEvent::Resized 触发：                              │
│      renderer_resize_canvas(physical_w, physical_h)              │
│  - winit RedrawRequested 触发：                                   │
│      let (cw, ch) = begin_frame(0, 0, winW, winH)                │
│      ui::draw_frame(&state, cw, ch)  ← 全部百分比布局            │
│      current_demo.render(t, cw as f32, ch as f32)                │
│      end_frame                                                   │
│  - WindowEvent → UiState（hover_index / selected_index / ...）   │
└──────────────────────────────────────────────────────────────────┘
                             │
                             ▼
                       renderer.dll (无修改)
```

### 4.1 DComp 集成关键点（vs widget WinRT 路径）

widget 端：`Compositor.CreateSurfaceBrush(ICompositionSurface)` → `SpriteVisual.Brush` → `SetElementChildVisual`

desktop-window 端：

```rust
// 1) 拿到 winit 窗口的 HWND
let hwnd: HWND = match window.window_handle()?.as_raw() {
    RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
    _ => unreachable!("Windows-only"),
};

// 2) 创建一个最低 D3D11 device 给 DComp（不跟 renderer 共享 device，
//    因为 renderer 内部 device 是私有的；DComp 只需要任意一个 D3D11 device）
let d3d_device = create_d3d11_device()?;
let dxgi_device: IDXGIDevice = d3d_device.cast()?;
let dcomp_device: IDCompositionDevice =
    DCompositionCreateDevice(&dxgi_device)?;

// 3) target 绑到 HWND（topmost=false 是普通桌面窗口）
let target: IDCompositionTarget =
    dcomp_device.CreateTargetForHwnd(hwnd, false)?;

// 4) visual 包装 swap chain
let visual: IDCompositionVisual = dcomp_device.CreateVisual()?;
let swap_chain_iunknown: *mut c_void = ffi.get_swapchain(handle)?;
let swap_chain: IDXGISwapChain1 = unsafe {
    IUnknown::from_raw(swap_chain_iunknown).cast()?
};
visual.SetContent(&swap_chain)?;

// 5) target.SetRoot(visual) + Commit
target.SetRoot(&visual)?;
dcomp_device.Commit()?;
```

之后每次 `renderer_end_frame` 内部 Present(0,0)，DComp 自动拉新内容上屏。

### 4.2 画布 resize 流程（v0.7 新加）

winit `WindowEvent::Resized` 是 desktop-window 切换画布的唯一入口。

```rust
match event {
    WindowEvent::Resized(new_size) => {
        // new_size 已经是物理像素（winit 0.30 inner_size 默认物理像素）
        ffi.resize_canvas(new_size.width as i32, new_size.height as i32)?;
        // 不需要重建 DComp visual / target —— DComp 自动追踪 swap chain
        // ResizeBuffers 之后的新 back buffer
    }
    WindowEvent::RedrawRequested => {
        // 拿当前画布尺寸（resize_canvas 之后已更新）
        let mut cw: i32 = 0;
        let mut ch: i32 = 0;
        ffi.begin_frame(0.0, 0.0, win_w, win_h, &mut cw, &mut ch)?;
        ui::draw_frame(&state, cw, ch);
        ffi.end_frame()?;
    }
    _ => {}
}
```

**Debounce 选择**：拖窗口连续产生 100+ Resized 事件（每个 px 一次），每次都 resize_canvas
会卡顿（每次 ResizeBuffers 约 100us~1ms）。两个选项：

- **方案 A（推荐 v1.0）**：直接每次 Resized 都调 `resize_canvas`。renderer 内部
  same-size short-circuit，但 size 真的每次都不同；卡顿明显但代码简单
- **方案 B（v1.1 优化）**：用 `std::time::Instant` 做 16ms debounce —— Resized 时
  保存目标尺寸，下一帧 RedrawRequested 时如果距上次 resize > 16ms 才真正调。
  避免拖动过程中的 ResizeBuffers 风暴

v1.0 选 A，简单胜过早优化；v1.1 看实测卡顿再决定。

### 4.3 业务画图百分比布局示范

```rust
// ui.rs::draw_frame
pub fn draw_frame(state: &UiState, canvas_w: i32, canvas_h: i32) {
    let cw = canvas_w as f32;
    let ch = canvas_h as f32;

    // 列表占左侧 20% 宽
    let list_w = cw * 0.20;
    // 状态栏占底部 4% 高
    let status_h = ch * 0.04;
    // 列表行高 = 画布高 4%（4K 屏自然变大，720p 屏自然小，物理观感一致）
    let row_h = ch * 0.04;
    // 字号 = 列表行高的 60%
    let font_size = row_h * 0.60;

    // 渲染列表背景
    state.ffi.fill_rect(0.0, 0.0, list_w, ch - status_h, ...);
    // 渲染列表项 ...
    // 渲染状态栏 ...
    // 当前 demo 渲染区 = 剩余区域
    let demo_x = list_w;
    let demo_y = ch * 0.05;  // 顶部 ExpectedSummary 5% 高
    let demo_w = cw - list_w;
    let demo_h = ch - status_h - ch * 0.05;
    state.current_demo.render(t, demo_x, demo_y, demo_w, demo_h);
}
```

**屏幕比例不一致自然适配**：

- 4:3 显示器 1024×768：列表 204px 宽，demo 渲染区 820×740，hello 文本永远在 demo 区中央
- 16:9 1920×1080：列表 384px，demo 1536×1037
- 21:9 3440×1440：列表 688px，demo 2752×1383（左右更宽，上下不变）

业务永远在"中央 demo 区"画 hello world，相对位置和相对大小永远一致。
没有变形、没有 letterbox、没有"DPI scale 假象"。

---

## 5. UI 渲染：immediate-mode self-rendered（百分比布局）

每帧由 `ui::draw_frame(&state, canvas_w, canvas_h)` 完成。state：

```rust
struct UiState {
    cursor_pos: Option<(f32, f32)>,    // 客户区物理像素（同画布坐标系）
    selected_demo: usize,
    hovered_demo: Option<usize>,
    perf: PerfStats,                   // 上一帧 renderer_get_perf_stats
    last_error: Option<String>,
    canvas_w: i32, canvas_h: i32,      // 上一帧 begin_frame 出参 —— 用于命中测试
}
```

### 5.0 布局常量（以画布尺寸为基准的比例）

```rust
const LIST_W_RATIO: f32 = 0.20;          // 列表宽 = 画布宽 20%
const STATUS_H_RATIO: f32 = 0.04;        // 状态栏高 = 画布高 4%
const HEADER_H_RATIO: f32 = 0.05;        // 顶部 ExpectedSummary 高 = 5%
const ROW_H_RATIO: f32 = 0.04;           // 列表每行 = 画布高 4%
const FONT_TITLE_RATIO: f32 = 0.024;     // 普通文字字号 = 画布高 2.4%
const FONT_HEADER_RATIO: f32 = 0.030;    // group header 字号 = 3%
const TOOLTIP_PAD_RATIO: f32 = 0.008;    // tooltip 内边距 = 0.8%
```

参考画布 1920×1080：列表 384，状态栏 43，行高 43，普通字号 26。
4K 屏 3840×2160 自动翻倍；720p 屏 1280×720 也按比例缩小。物理观感保持一致。

### 5.1 列表渲染

左侧 `canvas_w * LIST_W_RATIO` × `canvas_h - status_h - header_h`，每行
`canvas_h * ROW_H_RATIO` 高：背景 fill_rect 半透明深底；当前选中行 fill_rounded_rect
高亮蓝；hover 行 stroke_rounded_rect 灰边框；group header（"v0.6"/"Phase 1"/"Phase 2"/
"Errors"）字号 = `canvas_h * FONT_HEADER_RATIO`；普通行 draw_text demo.title 字号
= `canvas_h * FONT_TITLE_RATIO`。

### 5.2 Tooltip 渲染

hover 行的 `.hover_hint` 多行文本 → 该行右侧渲染：fill_rounded_rect 暗底 +
stroke_rounded_rect 边 + 多行 draw_text。x 超出画布右沿则左移防越界（`x_max =
canvas_w - tooltip_w`）。tooltip 内边距 = `canvas_h * TOOLTIP_PAD_RATIO`。

### 5.3 命中测试

`input::on_cursor_moved((x, y))`：
- `list_w = state.canvas_w as f32 * LIST_W_RATIO`
- `header_h = state.canvas_h as f32 * HEADER_H_RATIO`
- `status_h = state.canvas_h as f32 * STATUS_H_RATIO`
- `row_h = state.canvas_h as f32 * ROW_H_RATIO`
- 点 `(x, y)` 落在 `[0, list_w] × [header_h, canvas_h - status_h]` 内 → 列表区，按 `(y - header_h) / row_h` 算 hovered_demo
- 否则 → hovered_demo = None

`input::on_mouse_click((x, y), Left)`：在列表区点击 → selected_demo = hovered_demo，触发 demo 切换。

> **重要**：`UiState.canvas_w/h` 必须从**上一帧的 begin_frame 出参**取（而不是
> winit 当前的 inner_size），因为命中测试坐标系必须跟 demo 渲染时的画布坐标系
> 一致。winit Resized 事件可能在帧之间到，导致 cursor_pos 跟 canvas size
> 不同步——以最近一次 begin_frame 为准。

### 5.4 顶部 ExpectedSummary 文本

每帧渲染区域 `(list_w, 0, canvas_w - list_w, header_h)` 用 fill_rect 暗底 +
draw_text `current_demo.expected_summary()` 居中。

### 5.5 状态栏

每帧渲染 `(0, canvas_h - status_h, canvas_w, status_h)` 暗底 + draw_text 段落：

```
FPS 59.8 | render 142us | present 89us | frames 1234 | canvas 1920×1080 | last_status 0
```

`canvas WxH` 段是**当前画布尺寸的实时读数**——拖窗口验证时直接看这里数字跟着变。


---

## 6. Demo trait

```rust
pub trait Demo {
    fn title(&self) -> &str;
    fn group(&self) -> &str;
    fn hover_hint(&self) -> &str;
    fn expected_summary(&self) -> &str;

    fn setup(&mut self, ffi: &RendererFfi) -> Result<(), RenderError> {
        let _ = ffi;
        Ok(())
    }

    fn render(&mut self, ffi: &RendererFfi, t: f32, canvas_w: f32, canvas_h: f32);

    fn teardown(&mut self, ffi: &RendererFfi) {
        let _ = ffi;
    }
}
```

`RendererFfi` = libloading 加载的 30 个 fn ptr 的安全包装；提供 `ffi.fill_rect(...)` 等方法，内部 unsafe 调函数指针。

---

## 7. Demo gallery 全清单（25 个，覆盖 30 ABI + 画布行为）

> 注：所有 demo 内坐标用画布百分比，下面 expected_summary 中"中央 X×Y"等绝对像素是
> 参考画布 1920×1080 下的视觉感受；4K / 720p 屏画面比例自动跟随。

### 7.1 v0.6 基础(5 个)

| 序 | title | 覆盖 ABI | expected_summary |
|---|---|---|---|
| 1 | Clear / 紫色清屏 | renderer_clear | 整屏纯紫色（半透明） |
| 2 | FillRect / 蓝色矩形 | renderer_fill_rect | 中央 ~21%×~19% 蓝色实心矩形 |
| 3 | DrawText / Hello World | renderer_draw_text | 中央 "Hello, Overlay!"，字号 = 画布高 4.4% |
| 4 | Viewport / 4 corners | begin_frame 不同 vp | 4 个角各画 ~5% 边长红方块 |
| 5 | PerfStats | renderer_get_perf_stats | 状态栏数字跳动 |

### 7.2 Phase 1 矢量图元（8 个）

| 序 | title | 覆盖 ABI | expected_summary |
|---|---|---|---|
| 6 | Lines / 4 dash styles | renderer_draw_line | 4 条竖线 stroke=4：solid/dash/dot/dash_dot；**4 根都清楚可辨**（验证 dot 渲染 bug 修复，dashCap=ROUND） |
| 7 | Polyline / 三角与折线 | renderer_draw_polyline | 闭合三角 + 开口锯齿折线 |
| 8 | StrokeRect / 3 widths | renderer_stroke_rect | 同尺寸矩形 stroke=1/3/8 对比 |
| 9 | RoundedRect / radii | fill+stroke_rounded_rect | rx=0/4/16/32/64 + 椭圆角 rx≠ry |
| 10 | Ellipse | fill+stroke_ellipse | 正圆 / 椭圆 / 同心 fill+stroke |
| 11 | Clip / 嵌套 | push+pop_clip_rect | 双层嵌套：超大圆只在内 clip 区可见 |
| 12 | Transform / 旋转缩放 | set+reset_transform | 3 矩形：原位 / 平移 / 旋转+缩放 |
| 13 | StateMachineErrors | begin/end 状态机违例 | 故意双 begin → in-canvas 显示 last_error |

### 7.3 Phase 2 bitmap（9 个）

| 序 | title | 覆盖 ABI | expected_summary |
|---|---|---|---|
| 14 | CreateTexture / 无 update | renderer_create_texture | 64×64 全黑（D2D 默认零初始化） |
| 15 | UpdateTexture (BGRA8) | create + update_texture | 棋盘格放大 |
| 16 | UpdateTexture (RGBA8) | create + update_texture | **同棋盘格但喂 RGBA**，验证 swizzle |
| 17 | Interp / nearest vs linear | draw_bitmap interp | 4×4 上采样左半像素化、右半平滑 |
| 18 | SrcRect / 子矩形 | draw_bitmap src_rect | 取四象限分别放大 |
| 19 | OpacityFade | draw_bitmap opacity | 5 张同 bitmap，opacity 0.2 → 1.0 |
| 20 | LoadFromMemory / 嵌入 PNG | renderer_load_bitmap_from_memory | 显示 64×64 PNG 原图 |
| 21 | BitmapLifecycle | create → destroy → 再用 | 1 秒后 bitmap 消失，调 draw 静默跳过 |
| 22 | ResourceLimit / 1024 slot | 循环 create | 状态栏显示 "alloc=1024 last=-9" |

### 7.4 错误码探针（2 个）

| 序 | title | 覆盖 | expected_summary |
|---|---|---|---|
| 23 | DecodeFail / random bytes | load_bitmap_from_memory + 16 字节 | "status=-10 (DECODE_FAIL)" |
| 24 | LoadFromFile missing | load_bitmap_from_file 不存在路径 | "status=-11 (IO)" |

### 7.5 画布管理（v0.7 新加，1 个）

| 序 | title | 覆盖 ABI | expected_summary |
|---|---|---|---|
| 25 | Canvas Resize / 画布跟随 | resize_canvas + begin_frame 出参 | 渲染区**正中央**画 "RESIZE ME" 文字 + 一个画布尺寸 20% × 20% 的方框；拖动窗口边缘 resize → 文字与方框始终在中央 + 状态栏 canvas 数字实时跟随 + 画面物理像素始终锐利（4K 屏拉大 → 仍清晰，不模糊） |

**累计 25 demo**：覆盖 30 ABI 中的全部命令型与资源型 ABI + v0.7 新加的画布管理；
create / destroy / set_log_callback / get_swapchain 是启动 / 关闭流程必走，不需要
单独 demo。`renderer_resize`（v0.6 老 ABI）由 demo 25 隐式压测（每次 resize 都
间接触发 swap chain ResizeBuffers）。

---

## 8. 用户流程

1. 双击 `desktop-window-monitor.exe` → 窗口打开（默认 1280×720，可自由 resize）
2. 默认选中第 1 个 demo（Clear）
3. 渲染区显示该 demo 的画面 + 顶部 ExpectedSummary 文本 + 状态栏画布尺寸数字
4. 鼠标移到列表某项 → 该项右侧弹 tooltip
5. 单击或方向键切换 demo → 自动 teardown + setup
6. 拖动窗口边缘 resize → host 收 `WindowEvent::Resized` → `resize_canvas`，画面跟着重建（4K 屏拉大保持锐利，720p 缩小自然降清晰度）；UI 元素全部按新画布尺寸百分比重布局
7. 切到 demo 25 "Canvas Resize" 验证画布跟随效果
8. 关闭窗口 → teardown 当前 demo + renderer_destroy + 释放 DComp

---

## 9. 实施步骤（给 AI / 后续开发者按序执行）

> **前置**：renderer 端必须先实现 v0.7 ABI（`begin_frame` 出参 + `renderer_resize_canvas`）+ 通过单元测试，才进入下面 step 1。

1. **0.5 天**：Cargo.toml workspace 加 member；`monitors/desktop-window/Cargo.toml`，winit 窗口能弹出（resizable，min 480×320）
2. **1 天**：`ffi.rs` 用 libloading 加载 renderer.dll，定义 31 个 fn ptr（含 `resize_canvas` 和带出参的 `begin_frame`），写一组单元测试（最小：create + begin + clear + end + resize_canvas + begin 拿新尺寸 + destroy）
3. **1 天**：`dcomp.rs` 把 swap chain 挂到 HWND，看到一帧紫色清屏；连接 `WindowEvent::Resized` → `resize_canvas`；状态栏显示当前画布尺寸验证 resize
4. **1 天**：`ui.rs` immediate-mode 列表 + tooltip + 命中测试，**全部按 (canvas_w, canvas_h) 百分比布局**；`input.rs` 处理 winit event；`monitor.rs` 60Hz tick
5. **1 天**：实现 v0.6 + Phase 1 共 13 个 demo（每个 demo render 接收 `(t, demo_x, demo_y, demo_w, demo_h)` 百分比坐标）
6. **1 天**：实现 Phase 2 + 错误码探针共 11 个 demo（含嵌入 PNG）
7. **0.5 天**：实现 demo 25 Canvas Resize；状态栏 / FPS / last_error；自测 25 个 demo 都对得上 expected_summary，**重点验 demo 25 拖窗口画布跟随**
8. **1 天**：widget 端对齐改造（v0.7 实施关键步）
   - **8a** `RendererPInvoke.cs`：升级 begin_frame 签名（5 参 → 6 参，加 `out int canvasW, out int canvasH` 或 `IntPtr*`）；加 `renderer_resize_canvas` 包装
   - **8b** `CompositionPump.cs::DrawDebugContent`：把现在写死 1280×720 坐标改为 `(canvas_w, canvas_h)` 百分比布局；中央 Hello text + 渐变带按比例画
   - **8c** 删除 `DrawPrimitivesShowcase` / `DrawPhase2Showcase` / `Phase2BitmapShowcase`（约 12 slot 的测试 fixture，desktop-window 25 个 demo 已覆盖）
   - **8d** `CompositionPump.cs::OnRenderSizeChanged`（XAML SizeChanged）→ 调 `renderer_resize_canvas(physicalW, physicalH)`（widget 是否真要 ADAPTIVE 见 §13）
   - **8e** 重打 widget MSIX 验证：在 Game Bar 里能正常显示新版 hello text；resize widget 大小（旋转 / Game Bar 自适应）后画面物理像素跟随

**累计 ~7 天**（比 v0 草稿 +0.5 天，因为 step 8 widget 改造范围扩展）。

---

## 10. 验收标准

- [ ] `cargo build --release -p desktop-window-monitor` 成功；产物 < 2 MB
- [ ] 双击 .exe 能启动；列表 25 个 demo 全部可见
- [ ] hover 任一项弹 tooltip
- [ ] 切到 Lines 4 dash styles **4 根都清楚可见**（dot bug 修复验证）
- [ ] 切到 UpdateRgba8 视觉与 UpdateBgra8 完全一致（swizzle 验证）
- [ ] 切到 Interp 左半像素化、右半平滑
- [ ] 切到 Clip 双层嵌套效果对
- [ ] 切到 BitmapLifecycle 1 秒后图消失但程序不崩
- [ ] 切到 ResourceLimit 状态栏 last_status=-9
- [ ] 切到 DecodeFail 状态栏 last_status=-10
- [ ] **切到 Canvas Resize → 拖窗口边缘**：方框与 "RESIZE ME" 文字始终在中央；状态栏 canvas 数字实时更新；4K 屏拉到全屏画面仍锐利不模糊（点对点验证）
- [ ] **窗口缩到最小 480×320**：列表与 demo 区都不挤没，tooltip 不越界
- [ ] **窗口拉到 4K 全屏**：UI 元素物理观感跟 720p 时保持比例（不漂移）
- [ ] 关窗 → 任务管理器看进程消失
- [ ] widget 端：`RendererPInvoke.cs` 升级 v0.7 begin_frame 后 widget 仍能 build + 安装；`DrawDebugContent` 改百分比布局后中央 Hello text 永远居中（在 Game Bar 中改 widget 大小验证）
- [ ] widget 端：删 showcase 后 widget MSIX 重打成功，Game Bar 里仍能装 + 显示
- [ ] `cargo test --release` 全部测试通过（含新增 resize_canvas 单元测试）

---

## 11. 不在范围 / 未来扩展

### 11.1 v1.1（本 spec 之外）

- 多 monitor 同进程：开两个窗口看同一画面 / 不同画面
- "画布档位"切换 demo：用户面板选 720p / 1080p / 4K，host 调 `resize_canvas` 验证档位切换效果
- 屏幕坐标固定 vs 监视器坐标固定模式切换 demo（v1.0 默认监视器坐标固定）
- 自定义内容：从外部 .json / Lua 加载"画面定义"
- Resize debounce（v1.0 是直接每 Resized 都 resize_canvas，v1.1 加 16ms 合并避免拖窗口卡顿）

### 11.2 v0.7 收尾另开 ticket

- `renderer_debug_readback_pixels` ABI（仅 debug，让自动化测试做 golden 比对）
  - painter-abi-v0.7 第 6 节"已知缺口"
  - 加上后 desktop-window 可加"自动化模式"：跑完 24 个 demo readback + golden
- v0.8+：路径渐变、视频、屏幕捕获 各自加新 demo 分组

### 11.3 永远不做（与 renderer 定位冲突）

- **输入事件 ABI**：renderer 是 sink，输入由 monitor 层处理。已在 painter-abi-v0.7 spec 1.3 节"不在范围"标注
- 多语言原生 binding：保留 C ABI，社区按 ABI 各自包

---

## 12. 实施时的几个隐藏坑

1. **DCompositionCreateDevice 需要 IDXGIDevice**：desktop-window 自己创建一个 D3D11 device（最低 feature level 即可），不跟 renderer 共享 device（renderer 内部 device 是私有的）
2. **winit 0.30+ event loop 是 ApplicationHandler 模式**：在 `window_event` 收 `RedrawRequested` 时调 monitor.tick（不要在 `about_to_wait` 里 spin）
3. **HiDPI**：winit `inner_size()` 已经是物理像素；begin_frame 直接传物理像素 viewport
4. **DComp visual.SetContent**：renderer_get_swapchain 返的 raw IUnknown ptr 必须先 `IUnknown::from_raw(...).cast::<IDXGISwapChain1>()`，再 SetContent
5. **嵌入 PNG**：`include_bytes!("../assets/test-image.png")` 编译期嵌入
6. **last_error 拉取频率**：每 30 帧（半秒）拉一次，不要每帧 PInvoke
7. **demo 切换 setup/teardown**：必须 try-catch / Result 包好；某 demo 崩不应卡死整个 gallery
8. **窗口最小尺寸**：列表 ~20% + tooltip 防越界 → min 480×320；winit `set_min_inner_size`
9. **renderer.dll 路径**：libloading 默认按 PATH / exe 同目录搜；build.rs 拷贝 `target/release/renderer.dll` 到 desktop-window 的 target 目录确保启动找得到
10. **resize_canvas 不能在 begin/end_frame 之间调**：会返 RENDERER_ERR_FRAME_HELD（-6）。host 在帧外调（typically winit `WindowEvent::Resized` 在 RedrawRequested 之外）
11. **DComp visual 不需要重建**：`renderer_resize_canvas` 内部 ResizeBuffers 后 swap chain 还是同一个 IDXGISwapChain1 实例，DComp visual.SetContent 之前绑定的引用仍有效
12. **CreateSwapChainForComposition 不能传 W=0/H=0**：DXGI 1.2 起对 composition swap chain 的限制（HWND swap chain 才允许 0/0 自动 match）；resize_canvas 必须传具体值，否则 RENDERER_ERR_INVALID_PARAM
13. **首帧 begin_frame 出参**：renderer_create 之后第一次 begin_frame 出参 = create 时传的 initial_w/h（没有"未初始化画布"状态）
14. **百分比布局浮点取整**：列表行高 `canvas_h * 0.04` 在 720p 是 28.8px，可能产生 0.8px 抖动；用 `floor/round` 量化到整数像素，并保持文字 baseline 一致

---

## 13. 已审议的几个决策

### 13.1 为什么列表 / tooltip 用 renderer 自己画（不是 OS 控件）

- **dogfood 强度高**：每秒 60 次 stroke_rect / fill_rect / fill_rounded_rect / draw_text，列表 24 行 + tooltip 多行 + 状态栏 = 真实 UI 场景压测；renderer 任何性能 / 视觉 bug 立刻暴露
- **DComp visual + GDI 控件视觉层冲突**：Win32 ListView / Button 是 GDI 绘制（位图层），跟 DComp visual（合成层）在同一窗口客户区有 z-order 与透明度问题
- **跨 monitor 一致性**：widget 端也是用 renderer 自己画 UI；desktop-window 这么做让两个 monitor 在 dogfood 上一致
- **代码量并未爆炸**：immediate-mode UI 在 game dev 里普遍，list + tooltip + status bar 估算 200-300 行 Rust

### 13.2 为什么用 libloading 而不是 cargo workspace 内部 use renderer crate

- renderer.dll 是 cdylib，社区 monitor 走 C ABI；desktop-window 必须验证这条路径
- workspace `extern crate renderer` 直调 Rust API 等于绕过 C ABI，下游 C# / Python 错了察觉不到
- libloading 多 0.5 ms 启动开销，运行性能不变

**libloading 性能注意（实施时务必遵守）**：

`Library::get(b"name\0")` 内部走 `GetProcAddress`（每次几 us）。**不要在每次调用 ABI 时
都 `lib.get()`**——那是错误用法。正确做法 = 启动时一次性 eager-load 30 个 fn ptr 缓存到
`RendererFfi` struct 里，运行时调用直接 `(self.fill_rect)(...)`，等价于 `CALL [reg]`
间接调用，**与 C# `[DllImport]` 同等性能**（甚至比 DllImport 首次延迟解析更可控）：

```rust
pub struct RendererFfi {
    _lib: Library,
    create: unsafe extern "system" fn(i32, i32, *mut *mut c_void) -> i32,
    fill_rect: unsafe extern "system" fn(*mut c_void, f32, f32, f32, f32, f32, f32, f32, f32) -> i32,
    // ... 30 个 fn ptr，启动时一次性 .get() 缓存
}
```

不需要任何 libloading 配置参数；上述设计就是最优路径。维护者如果手痒把 fn ptr 改回
"每次 lib.get()" 模式，应当 review 时拒掉。

### 13.3 为什么 DComp 不复用 widget 的 ICompositorInterop

- ICompositorInterop 是 WinRT 接口，需要 WinRT activation context（XAML 进程自带，winit 进程没有）
- desktop-window 是纯 Win32 进程，DComp 直挂 HWND 是更原生的路径
- 两条路径殊途同归（都是 D2D → swap chain → Present → 合成器拉新内容上屏），只是接入点不同

### 13.4 为什么 25 个 demo 而不是 1 个 mega demo

- mega demo = 现在 widget 里的 showcase——信息密度高但难逐项验证
- 25 个独立 demo，每个只验证一组相关 ABI；hover tooltip 知道每个的预期；某个挂了立刻定位

### 13.5 为什么不把 widget 的 RendererPInvoke.cs 转成共享 binding

- C# 端 P/Invoke 包装跟 Rust 端 libloading 包装是两个语言两份代码，本来就该独立
- 跨语言"共享" = 增加耦合：未来加新 ABI 时两份要同步改；每个 monitor 各自维护自己的 binding 反而清晰

### 13.6 为什么 desktop-window 上线后**删掉** widget 端的 showcase（不保留作冗余）

widget 端的 `DrawPrimitivesShowcase` / `DrawPhase2Showcase` / `Phase2BitmapShowcase` 是
phase 1/2 临时塞进生产 host 的测试 fixture。desktop-window 上线后**删掉**，理由：

1. **职责干净**：widget 是 production monitor，应该只画"用户配置的内容"（HUD / 视频 / 屏幕捕获 overlay 等）。showcase 是测试 fixture，混在生产代码里反 KISS
2. **冗余无收益**：desktop-window 25 个 demo 覆盖比 widget 12 slot 全；widget 端是低保真子集，反成了"两份测试代码同步维护"的负担
3. **未来扩展路径**：以后加 phase 3 视频 / phase 4 capture 的 demo，**只往 desktop-window 加**；widget 不跟。如果真要"在 Game Bar 里也直观看 ABI 行为"，正确做法是做独立的 `demo-widget` monitor（用户从 widget 列表里选启用），不是把 production widget 兼当 demo host

**保留**：widget 中央的 `DrawDebugContent` 主体（透明清屏 + 渐变带 + 屏幕中心 Hello text + 副信息），这是 phase 5+ 用户开始装真正业务画面前的"hello world 占位"，不是测试 fixture。**注意 v0.7 起这个保留代码也要按 (canvas_w, canvas_h) 百分比重写**（不再写死 1280×720 坐标），跟 desktop-window 业务编码风格一致。

### 13.7 为什么 desktop-window 默认"画布跟随物理像素"，widget 也是

调研业界（Bevy / OBS / Direct2D 官方）后的结论（详见 painter-abi-v0.7 §10.8）：

- **业务坐标 = 渲染分辨率 = 物理像素**：业务画 (640, 360) 永远是画布 (640, 360) 像素，
  没有"逻辑坐标系"中间层。这意味着画布尺寸变了，业务画的内容相对位置必须用百分比
  自己换算
- **画布尺寸跟物理像素**：用户的窗口是 1080p 还是 4K，业务永远在物理像素上画图，
  4K 屏永远清晰，720p 屏自然降清晰度。**没有"业务坐标系不变"的假象**
- **拒绝了"逻辑画布固定 1280×720 + 内核自动缩放"方案**：屏幕长宽比不一致时
  （21:9 / 4:3），固定逻辑画布要么变形要么留黑边，没有合理选择
- **业务想要 Bevy `FixedVertical` 等 ScalingMode**：用 `set_transform` 自己做缩放矩阵，
  ABI 不替业务做选择

widget 跟 desktop-window 都用同一套机制：
- **现状**：widget 创建 1280×720 + 不调 resize_canvas + DComp 自动拉伸 = 业务坐标固定，4K widget 模糊
- **v0.7 改造后**：widget 创建初始 1280×720 + SizeChanged 时调 `resize_canvas` 跟物理像素 + 业务画图百分比 = widget 任何大小都物理像素清晰

如果 widget 维护方暂时不想改 ADAPTIVE 也行：v0.7 ABI 完全兼容"不调 resize_canvas"
的 v0.6 行为（创建后画布永远是初始尺寸，DComp 拉伸）。只是 widget 业务画图代码
依然要改用 `(canvas_w, canvas_h)` 百分比，因为 begin_frame 出参是必传的（v0.7 签名变了）。

### 13.8 为什么不引入 canvas_mode 枚举

详细论证见 painter-abi-v0.7 §10.8。简短版：

- mode 是 host 调用模式即可表达的策略（调或不调 resize_canvas），不是内核状态
- 引入 mode 枚举 = 把决策提前到"创建时就要选好"，反而限制了运行时切换的灵活性
- Bevy 的 ScalingMode（FixedVertical 等）属于业务层 Camera/Projection，不属于
  渲染引擎 ABI 层。我们对齐这个分层


---

## 14. 与 painter-abi-v0.7 的对应

本 spec 与 painter-abi v0.7 是**双向驱动**关系：

- **本 spec 驱动 ABI 改动**：v0.7 起加 §2.6 画布管理 ABI（`renderer_resize_canvas`
  + `begin_frame` 出参 + 新错误码 `-14`）就是为了 desktop-window 的"画布跟随物理像素"
  和"业务百分比布局"需求 —— 同时升级 widget 路径，两个 monitor 一致
- **本 spec 是消费方**：除画布管理外，所有 30 个绘图 / bitmap / 视频 / 捕获 ABI
  都是 painter-abi 单方向定义、desktop-window 用 demo 验证

v0.7 进入 phase 3-5 时本 spec 第 7 节加新 demo 分组：

```
src/demos/phase3_video.rs       (本地视频 5 demo)
src/demos/phase4_capture.rs     (屏幕捕获 4 demo)
src/demos/phase5_path_gradient.rs (Path + 渐变 6 demo)
```

不需要重打基础设施。

### ABI 引用一览（v0.7 desktop-window 消费的 31 个 ABI）

| 章节 | ABI | demo 序号 |
|---|---|---|
| painter-abi §2.6 | `renderer_create` / `renderer_destroy` / `renderer_get_swapchain` / `renderer_set_log_callback` / `renderer_get_perf_stats` / `renderer_last_error_string` / **`renderer_resize_canvas`** / **`renderer_begin_frame`（出参版）** / `renderer_end_frame` | 启动关闭流程 + 25（resize_canvas） |
| painter-abi §2.3.1 | `renderer_draw_line` / `renderer_draw_polyline` | 6, 7 |
| painter-abi §2.3.2 | `renderer_stroke_rect` / `renderer_fill_rounded_rect` / `renderer_stroke_rounded_rect` | 8, 9 |
| painter-abi §2.3.3 | `renderer_fill_ellipse` / `renderer_stroke_ellipse` | 10 |
| painter-abi §2.3.4 | `renderer_fill_path` / `renderer_stroke_path` | （phase 5 后加 demo 26+）|
| painter-abi §2.4 | `renderer_push_clip_rect` / `renderer_pop_clip` / `renderer_set_transform` / `renderer_reset_transform` | 11, 12 |
| painter-abi §2.5 | `renderer_fill_rect_gradient_linear` / `renderer_fill_rect_gradient_radial` | （phase 5 后加 demo 27+）|
| painter-abi §3.3 | `renderer_load_bitmap_from_file` / `renderer_load_bitmap_from_memory` | 20, 24 |
| painter-abi §3.3.2 | `renderer_create_texture` / `renderer_update_texture` / `renderer_destroy_bitmap` | 14, 15, 16, 21, 22 |
| painter-abi §3.4 | `renderer_draw_bitmap` | 17, 18, 19, 23 |
| painter-abi §1.4 错误码 | `RENDERER_ERR_RESOURCE_LIMIT (-9)` / `DECODE_FAIL (-10)` / `IO (-11)` / **`CANVAS_RESIZE_FAIL (-14)`** | 22, 23, 24 |

总共 31 个 ABI（含画布管理新增 1 个）。
