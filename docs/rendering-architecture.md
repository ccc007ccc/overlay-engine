# 渲染架构评估

本文记录当前 overlay-engine 的图形渲染主线、已验证链路、短板和长期路线。结论面向当前开发阶段：不保留旧协议兼容，优先选择长期最优路线。

## 结论

当前主线是正确方向：

```text
Core 进程
  -> D3D11 / D2D
  -> IPresentationManager / IPresentationSurface
  -> DCompositionCreateSurfaceHandle
  -> DuplicateHandle 到 Monitor 进程
  -> Desktop HWND / Xbox Game Bar Widget visual tree
```

这条路线比以下旧路线更适合作为长期基础：

- in-process `renderer.dll` 由 host 自己加载并渲染。
- CPU readback / `WriteableBitmap`。
- 让 Game Bar widget 自己承担主渲染或主合成策略。
- 为 Desktop monitor 和 Game Bar widget 分别实现不同 compositor policy。

原因：Core 进程可以集中维护 GPU 设备、资源、IPC ownership、surface 分发和后续 compositor 状态；Monitor 端只需要消费 Core 输出的 DComp surface，平台差异收敛在 visual-tree 挂载层。

## 当前实现链路

### Core 侧

当前核心代码在 `core-server/src/renderer/dcomp.rs` 和 `core-server/src/server_task.rs`。

`CoreDevices` 持有：

```text
ID3D11Device
IDCompositionDesktopDevice
Mutex<RenderContextGuard>
  -> ID3D11DeviceContext
  -> D2DEngine
```

`CanvasResources` 表示一张 World surface：

```text
render_w / render_h
IPresentationManager
IPresentationSurface
DComp NT handle
N 个 IPresentationBuffer
N 个 ID3D11Texture2D
N 个 ID3D11RenderTargetView
```

`PerMonitorResources` 表示一个 `(canvas_id, monitor_id)` 的 MonitorLocal surface。它与 World surface 使用同样的 presentation buffer 轮转模式，但生命周期独立：Monitor 断开时只释放自己的 MonitorLocal surface，不影响 World 或其他 Monitor。

### IPC / ownership

当前稳定模型是：

```text
App -> Canvas -> N Monitor
```

关键约束：

- Canvas 由 App 创建并拥有。
- `SubmitFrame` 只能写调用方拥有的 Canvas。
- Monitor 注册时带 `owner_app_id` 和 `target_canvas_id`。
- `AttachMonitor` 必须通过 ownership 校验：App 拥有 Canvas、App 拥有 Monitor、Monitor 的 target canvas 与请求一致。
- 旧 `auto_attach` 和 ownerless global attach 已删除；Game Bar ownerless manual monitor 只能被 App 通过 `StartMonitor(GameBar)` 显式绑定到自己的 Canvas。
- `RegisterMonitorV2` 已合并到唯一 `RegisterMonitor`；未知 opcode 是 fatal protocol error。

这使当前系统从“全局 monitor 自动附着所有 canvas”的早期模型，收紧为显式 App ownership 模型。

### Desktop Window Monitor

`monitors/desktop-window/src/bin/monitor.rs` 当前是 thin monitor：

```text
CanvasAttached
  -> IDCompositionDesktopDevice::CreateSurfaceFromHandle
  -> CreateVisual
  -> visual.SetContent(world surface)
  -> target.SetRoot(root visual)
```

收到 `MonitorLocalSurfaceAttached` 后追加一层 top visual：

```text
root visual
  ├─ World visual
  └─ MonitorLocal visual
```

World visual 使用 viewport transform；MonitorLocal visual 固定在 monitor 客户区坐标。

### Xbox Game Bar Widget

`monitors/game-bar-widget/Native/OverlayPump.cs` 当前也是 thin monitor：

```text
ElementCompositionPreview.GetElementVisual(host)
  -> root ContainerVisual
  -> world OverlayLayer
  -> monitor-local OverlayLayer
```

收到 `CanvasAttached` 后调用 `ICompositorInterop.CreateCompositionSurfaceForHandle` 挂载 World surface；收到 `MonitorLocalAttached` 后挂载 MonitorLocal surface。Widget 自身只负责连接、挂载、viewport transform 和状态展示，不负责主 compositor policy。

## 已验证的关键技术点

- `DCompositionCreateSurfaceHandle` 创建 DComp surface NT handle 可用。
- `IPresentationManager::CreatePresentationSurface(handle)` 可绑定该 handle。
- Core 可通过 `DuplicateHandle` 把 handle 复制给 Monitor 进程。
- Desktop Win32 进程可用 `IDCompositionDesktopDevice::CreateSurfaceFromHandle` 消费 handle。
- Game Bar widget 可用 `ICompositorInterop.CreateCompositionSurfaceForHandle` 消费 handle。
- Presentation buffer 轮转使用 `BUFFER_COUNT = 3`，降低 DWM 持有 buffer 时的卡顿概率。

## 当前短板

### 1. 还不是多 App compositor

当前 Monitor 只挂载一张 Canvas World surface 和一张可选 MonitorLocal surface。它不是 scene graph，也没有 layer / z-order / clip / opacity / transform / focus / virtual window lifecycle。

因此当前只能表达：

```text
App A -> Canvas A -> Monitor X
```

还不能表达：

```text
App A -> Layer 1 ┐
App B -> Layer 2 ├─ Monitor X
App C -> Layer 3 ┘
```

### 2. Render context 全局串行

`CoreDevices.render_ctx` 用 `Mutex<RenderContextGuard>` 包住 D3D immediate context 和 D2D context。这是正确的安全保护，因为 immediate context 不能被多个提交并发使用；但长期看它会把所有 App / Canvas / Monitor 的 submit 串行化。

短期可接受，长期需要把“安全串行”演进成“可调度串行”：

- 按 Canvas / scene 分队列。
- 可观测每个 App 的渲染耗时。
- 对慢 App 做预算、降帧或隔离。

### 3. MonitorLocal replay 成本线性增长

`PUSH_SPACE(MonitorLocal)` 区间会 replay 到每个 attached monitor 的 `PerMonitorResources`。这让每个 Monitor 都能得到独立左上角 HUD，但成本是 `O(monitor_count * local_command_count)`。

这适合作为当前修复，但不是复杂 HUD / 多窗口 compositor 的最终模型。后续应把 MonitorLocal 视为 layer 类型或 scene overlay，而不是每帧盲目 replay 所有本地命令。

### 4. Monitor 尺寸反馈不足

`PerMonitorResources` 当前用 canvas logical size clamp 到 4096 作为默认大小，因为协议还没有可靠的 monitor client size / DPI / resize feedback。

长期需要 Monitor 主动报告：

```text
MonitorMetrics / MonitorSizeChanged
  monitor_id
  logical_w / logical_h
  physical_w / physical_h
  dpi_scale
  visible_rect
```

Core 再按真实 monitor 尺寸创建或 resize monitor-local / composite output surface。

### 5. Device-lost 只识别，未完整恢复

当前 `present_manager` 已把 `DXGI_ERROR_DEVICE_REMOVED` / `DXGI_ERROR_DEVICE_RESET` / `DXGI_ERROR_DEVICE_HUNG` 分类为 `DeviceLost`，但日志仍是“rebuild required (not yet implemented)”。

长期必须补完整恢复：

1. 暂停接收新 frame 或进入 drop 模式。
2. 重建 D3D11 / D2D / DComp / presentation resources。
3. 重建每个 CanvasResources 和 PerMonitorResources。
4. 向 Monitor 重新发送 surface handles。
5. 保留或重建 bitmap/resource table。

### 6. 观测指标仍不足

已有 rolling render duration warning，但长期还需要：

- 每 App / Canvas / Monitor frame time。
- acquire timeout / dropped frame 计数。
- present outcome 计数。
- surface recreate / device-lost 计数。
- IPC queue depth / shared-memory ring usage。

这些指标决定后续 compositor 是否能稳定承载多个 App。

### 7. `rust-renderer` 是 legacy 路线，不是当前主路径

release staging 当前仍复制 `renderer.dll`，所以不能盲删 `rust-renderer`。但长期主线已经是 Core-side renderer：底层可复用旧 D2D / WIC / bitmap 逻辑，架构上不应回到 in-process DLL。

建议后续分两步处理：

1. 明确标注 `rust-renderer` 为 legacy / historical 或迁移源。
2. 等 Core-side renderer 完成同等能力后，从 release staging 和 installer 中移除 `renderer.dll`。

## 长期路线

长期模型应演进为：

```text
N App
  -> Canvas
    -> Layer / VirtualWindow
      -> MonitorScene
        -> MonitorOutputSurface
          -> Desktop HWND / Game Bar Widget
```

职责边界：

- App：创建 Canvas、提交 frame、声明想显示的 layer / virtual window。
- Core：维护 scene、layer ownership、z-order、clip、opacity、transform、focus、权限、生命周期和最终 surface 输出。
- Monitor：thin consumer，只挂载 Core 给出的 surface 或 layer visual，不做跨 App policy。
- Game Bar Widget：仍然是 single-instance thin monitor，不能成为主 compositor。

## 当前阶段建议

1. 保留 Core-side D3D11/D2D + PresentationManager + DComp handle 主线。
2. 不再恢复 legacy `RegisterMonitorV2` / `auto_attach` / ownerless global attach。
3. 不让 Game Bar widget 承担多 App 合成策略。
4. 下一阶段新增 scene/layer 协议和状态模型，而不是在 Monitor 端堆特殊逻辑。
5. 在实现 compositor 前先补 Monitor metrics、device-lost recovery 和基础 metrics，否则多 App 后问题会被放大。
