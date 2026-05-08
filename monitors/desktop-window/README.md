# spike: DComp 跨进程 surface 共享

v1.0 server-mode 的第一个技术 spike（按 `docs/v1.0-server-bootstrap.md` §9.2）。
验证 producer 进程把渲染内容通过 NT handle 跨进程共享给 consumer 进程的可行性，并验证“逻辑画布 / 渲染分辨率 / consumer viewport”三者解耦。

## 核心语义

v1.0 不能把 canvas 固定成某几个分辨率，也不能让内容跟 consumer 窗口走。正确模型是：

- **logical canvas**：业务坐标系，默认对应屏幕/虚拟桌面物理坐标；尺寸由 producer 创建 canvas 时指定，可任意比例/任意大小。
- **render resolution**：producer 实际渲染 texture 分辨率，可任意设置；只影响清晰度/模糊度，不改变业务坐标位置。
- **consumer viewport**：consumer 窗口在屏幕上的 client 矩形；移动窗口时只是改变 viewport origin，看到同一张 logical canvas 的不同区域。

映射公式：

```text
render_pixel -> logical = render_pixel * (logical_size / render_size)
logical -> consumer_window = logical - viewport_origin

DComp transform:
  M11 = logical_w / render_w
  M22 = logical_h / render_h
  M31 = -viewport_x
  M32 = -viewport_y
```

因此可以像游戏一样：把同一个 2560×1440 logical canvas 用 1280×720、960×540、3840×2160 等任意 render resolution 输出；位置和比例保持正确，只有清晰/模糊程度变化。

## 坐标空间：同一画布必须同时支持 world 与 monitor-local

所有 monitor（desktop-window / Game Bar widget / 后续其他宿主）都必须支持同一 canvas 里混合两类元素：

- **world/canvas-space 元素**：使用 logical canvas 坐标，受 viewport transform 影响。例：坐标轴、屏幕中心十字、跟物理屏幕位置绑定的 HUD。
- **monitor-local 元素**：使用当前 monitor/consumer 的本地坐标，不随 viewport 移动。例：每个监视器自己的状态角标、边框、fps 文本、允许/拒绝提示。

实现上不能让 consumer 自己“画策略内容”；Core/producer 仍然决定画什么。正确模型是在命令流中标记坐标空间：

```text
push_space(World)          # 默认：logical canvas -> viewport -> monitor
  draw_grid / draw_axis
pop_space()

push_space(MonitorLocal)   # 当前 consumer local px/DIP -> monitor，不应用 viewport offset
  draw_status_badge / draw_border
pop_space()
```

Core 对同一个 canvas 给每个 attached consumer 出帧时，需要用该 consumer 的 viewport 和实际尺寸分别解析这两类命令。这样同一画布里可以既有“固定在物理屏幕/逻辑画布上”的元素，也有“固定在每个监视器窗口上”的元素。

## 关键发现（更正 bootstrap doc §4.1）

bootstrap 原文：“Core 端创建 IDXGISwapChain... DCompositionCreateSurfaceHandle... DuplicateHandle”——把 swap chain 和 surface handle 当成关联对象，**不准确**。

实际正确链路：

```text
Producer (Core 进程)：
  D3D11CreateDevice(BGRA_SUPPORT)
  CreatePresentationFactory(d3d) -> IPresentationFactory
  factory.CreatePresentationManager() -> IPresentationManager
  DCompositionCreateSurfaceHandle(ALL_ACCESS, NULL) -> NT HANDLE
  manager.CreatePresentationSurface(handle) -> IPresentationSurface
  d3d.CreateTexture2D(BGRA8, SHADER_RESOURCE | RENDER_TARGET,
      SHARED | SHARED_NTHANDLE | SHARED_DISPLAYABLE) -> ID3D11Texture2D
  manager.AddBufferFromResource(texture) -> IPresentationBuffer
  UpdateSubresource / D2D / D3D render into texture
  surface.SetBuffer(buffer)
  manager.Present()

Consumer (Monitor/Widget 进程)：
  DuplicateHandle 接收 NT HANDLE
  dcomp.CreateSurfaceFromHandle(handle) -> IUnknown wrapper
  visual.SetContent(wrapper IUnknown)   # 直接传，不 cast
  visual.SetTransform(logical/render scale + negative viewport origin)
  target.SetRoot(visual)
  dcomp.Commit()
```

## 关键陷阱

1. `IDCompositionSurface::BeginDraw` 只能在同进程 D2D 路径用；跨进程 producer 写内容必须走 CompositionSwapchain (`IPresentationManager`)。
2. `dcomp.CreateSurfaceFromHandle` 返回的 `IUnknown` 不实现 `IDCompositionSurface`（否则 `E_NOINTERFACE`）；它是 wrapper，只能透传给 `visual.SetContent`。
3. `AddBufferFromResource` 的 D3D11 texture flags 要跟官方 CompositionSwapchain 示例一致：
   - `BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET`
   - `MiscFlags = D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_DISPLAYABLE`
   - 之前尝试 `SHARED_NTHANDLE | SHARED_KEYEDMUTEX` 会导致内容不按预期显示。
4. `Flush()` 很重要：producer 写完 texture 后要 flush D3D command queue，再 `SetBuffer + Present`。

## 跑这个 spike

```powershell
cargo build --release -p desktop-window-monitor

# Terminal 1：默认 logical=当前主屏物理分辨率，render=logical（点对点）
.\target\release\desktop-demo-producer.exe

# Terminal 1：也可以手动指定任意 logical/render 分辨率
.\target\release\desktop-demo-producer.exe 2560 1440 960 540
.\target\release\desktop-demo-producer.exe 3840 2160 1280 720

# Terminal 2
.\target\release\desktop-window-consumer.exe
```

预期：consumer 弹出普通 Win32 窗口，显示一张坐标网格/轴线。

验证点：

- 拖动窗口时，网格应表现为“固定在屏幕/逻辑画布上”，窗口像观察窗。
- 最大化窗口时，不应该只是左上固定 256×256 小块；应显示 logical canvas 的对应 viewport。
- 改 producer render resolution（例如 960×540 / 1280×720 / 3840×2160）时，网格坐标位置与比例应保持一致，只是清晰度不同。

## 现已验证 ✅

- `DCompositionCreateSurfaceHandle` + `DuplicateHandle` 跨进程传递 OK
- `IPresentationManager::CreatePresentationSurface` 绑定 NT handle OK
- `dcomp.CreateSurfaceFromHandle + visual.SetContent` 在普通 Win32 进程 OK
- IPC 协议 (named pipe, 4 字节 PID + 24 字节 handle+metadata payload) OK
- Win32 consumer 侧具备 viewport transform 所需 API：`visual.SetTransform2(Matrix3x2)`

## 还没验证 ❌（v1.0 第二个 spike）

- Consumer 换成 UWP widget（AppContainer 沙盒）能否同样 work
- `Windows.UI.Composition.Compositor` 通过 `ICompositorInterop::CreateCompositionSurfaceForHandle` 接收 NT handle 是否走通
- AppContainer 进程接收来自非 AppContainer producer 的 DuplicateHandle 是否需要特殊 ACL
- 把 v0.7 widget 里为修拉伸 bug 临时改掉的“观察窗语义”恢复回来，同时保留 host element 物理像素映射修复
