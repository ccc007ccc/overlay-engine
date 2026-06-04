# 多 App 单 Monitor 与 Xbox Game Bar Compositor 计划

本文定义 overlay-engine 后续支持“多个 App 在同一个 Monitor / Xbox Game Bar 单窗口中同时绘制”的长期路线。当前开发阶段不保留旧协议兼容，因此下面按长期最优模型设计，不为早期 `RegisterMonitorV2` / `auto_attach` 路径预留迁移负担。

## 背景

当前已支持：

```text
App -> Canvas -> N Monitor
```

不支持：

```text
N App -> N Layer / VirtualWindow -> 1 Monitor
```

Xbox Game Bar 的关键约束是：用户只能打开一个 Overlay Widget 实例，不能像 Desktop Window Monitor 那样由 Core 为每个 App 启动一个新窗口。因此 Game Bar 场景必须支持一个 Monitor 内承载多个 App 的可视内容。

## 总体决策

采用 Core compositor，Monitor 保持 thin consumer。

```text
App A ─ Canvas A ─ Layer A ┐
App B ─ Canvas B ─ Layer B ├─ MonitorScene(GameBar) ─ CompositeOutputSurface ─ Game Bar Widget
App C ─ Canvas C ─ Layer C ┘
```

不要采用以下路线：

- 不让 Game Bar widget 自己管理跨 App z-order / clip / focus / authorization。
- 不让 Desktop monitor 和 Game Bar widget 各自实现一套 compositor policy。
- 不恢复全局 ownerless auto attach；短期只允许 App 用 `StartMonitor(GameBar)` 显式绑定当前空闲 Widget。
- 不让多个 App 直接并发写同一张 Canvas 作为默认方案。

理由：Core 是唯一能同时看到 App ownership、Canvas、Monitor、IPC lifecycle 和 GPU resources 的地方。合成策略放在 Core 才能保持 Desktop 与 Game Bar 行为一致。

## 概念模型

### MonitorScene

一个 MonitorScene 表示某个 Monitor 当前显示的 scene。

字段草案：

```text
MonitorScene {
  scene_id: u32
  monitor_id: u32
  output_mode: CompositeSurface | LayerSurfaces
  output_surface: Option<SurfaceHandle>
  layers: Vec<LayerId>
  focused_layer: Option<LayerId>
  metrics: MonitorMetrics
}
```

Game Bar 默认使用 `CompositeSurface`：Core 输出一张最终合成 surface，Widget 只挂载这一张。Desktop monitor 可先使用同样模式，后续如需减少 Core 合成成本，再支持 `LayerSurfaces` 多 child visual 模式。

### Layer

Layer 是把某个 App 的 Canvas 放入某个 MonitorScene 的显示实体。

```text
Layer {
  layer_id: u32
  scene_id: u32
  owner_app_id: u32
  source_canvas_id: u32
  z_order: i32
  visible: bool
  opacity: f32
  clip_rect: Option<Rect>
  transform: Matrix3x2
  hit_test: HitTestPolicy
  lifecycle_state: Active | Hidden | Closing
}
```

Layer 必须有明确 owner。只有 owner App 或 Core policy 可以修改它。

### VirtualWindow

VirtualWindow 是 Layer 的窗口化策略对象，主要用于 Game Bar 单窗口内模拟多个 App 子窗口。

```text
VirtualWindow {
  window_id: u32
  layer_id: u32
  title: String
  bounds: Rect
  min_size: Size
  max_size: Option<Size>
  resizable: bool
  movable: bool
  focusable: bool
  chrome: None | Minimal | Standard
}
```

VirtualWindow 不直接拥有 surface；它只描述 Layer 在 MonitorScene 内的交互和布局。

### MonitorMetrics

Monitor 必须反馈真实显示尺寸，Core 才能正确创建 composite output surface。

```text
MonitorMetrics {
  monitor_id: u32
  logical_w: u32
  logical_h: u32
  physical_w: u32
  physical_h: u32
  dpi_scale: f32
  safe_area: Rect
}
```

## 协议草案

下一代协议可以直接引入新 opcode，不保留旧 `RegisterMonitorV2` 兼容。命名先用语义，不在本文固定 opcode 数值。

### Monitor 注册与 metrics

```text
RegisterMonitor(capabilities)
MonitorRegistered(monitor_id, scene_id)
UpdateMonitorMetrics(monitor_id, logical_w, logical_h, physical_w, physical_h, dpi_scale)
MonitorMetricsChanged(monitor_id, ...)
```

capabilities 至少包含：

```text
kind: DesktopWindow | GameBar
max_layers: u32
supports_composite_surface: bool
supports_layer_surfaces: bool
supports_input_feedback: bool
manual_lifecycle: bool
```

Game Bar：

```text
supports_composite_surface = true
supports_layer_surfaces = false   # 初期推荐
manual_lifecycle = true
max_layers = policy limit
```

Desktop Window：

```text
supports_composite_surface = true
supports_layer_surfaces = optional future
manual_lifecycle = false when Core-started
```

### Layer 生命周期

```text
CreateLayer(request_id, canvas_id, monitor_id, z_order, flags)
LayerCreated(request_id, layer_id, status)
UpdateLayer(layer_id, z_order, visible, opacity, clip_rect, transform)
DestroyLayer(layer_id)
LayerDestroyed(layer_id, reason)
```

规则：

- `CreateLayer` 调用方必须拥有 `canvas_id`。
- `monitor_id` 必须存在且允许该 App attach。
- `UpdateLayer` 调用方必须拥有 `layer_id`。
- App 断开时 Core 自动销毁该 App 的所有 Canvas 和 Layer，并重新合成受影响 scene。

### VirtualWindow 生命周期

```text
CreateVirtualWindow(request_id, layer_id, title, bounds, flags)
VirtualWindowCreated(request_id, window_id, status)
UpdateVirtualWindow(window_id, title?, bounds?, state?)
DestroyVirtualWindow(window_id)
```

VirtualWindow 可以作为 Layer 的扩展能力。第一阶段可先只实现 Layer，不实现窗口 chrome；Game Bar 仍可显示多个固定 layer。

### Composite output

```text
MonitorCompositeAttached(monitor_id, scene_id, surface_handle, logical_w, logical_h, render_w, render_h)
MonitorCompositeResized(monitor_id, scene_id, surface_handle, ...)
MonitorCompositeDetached(monitor_id, scene_id, reason)
```

Monitor 收到后只做：

```text
CreateCompositionSurfaceForHandle / CreateSurfaceFromHandle
CreateSurfaceBrush
CreateSpriteVisual
SetContent
Commit
```

## Core 合成策略

### 推荐第一阶段：Core composite output surface

Core 为每个 MonitorScene 创建一张 composite output surface。每帧：

1. 收集 scene 中 visible layers。
2. 按 z_order 排序。
3. 对每个 Layer 采样 source canvas surface 或最新 frame buffer。
4. 应用 clip / opacity / transform。
5. 渲染到 MonitorScene output surface。
6. Present output surface。

优点：

- Game Bar widget 最简单，只挂一张 surface。
- Desktop 和 Game Bar 行为一致。
- z-order / clip / opacity / transform / focus 都在 Core 一处实现。
- 权限与生命周期集中。

代价：

- Core 需要实现 GPU-side composition pass。
- 多 scene / 多 layer 时 GPU 成本集中在 Core。
- 需要更好的 metrics 和 performance counters。

### 第二阶段：可选 LayerSurfaces

当 Desktop monitor 需要更低延迟或更少 Core composite pass 时，可以让 Monitor 直接挂多个 layer surface：

```text
root visual
  ├─ Layer A visual
  ├─ Layer B visual
  └─ Layer C visual
```

但即使走 LayerSurfaces，z-order 和权限仍由 Core 下发，Monitor 只执行 visual tree 指令，不自行决策。

Game Bar 初期不走 LayerSurfaces，避免 UWP/WinUI visual tree 内承担太多 policy 和边界条件。

## 权限与安全边界

必须保留并加强当前 ownership 约束：

- App 只能为自己拥有的 Canvas 创建 Layer。
- App 只能更新自己拥有的 Layer / VirtualWindow。
- Monitor 不能主动查询或 attach 任意 Canvas。
- Game Bar 是手动生命周期 Monitor，不主动启动 Core，也不替 App 绕过授权。
- Core 不应接受 ownerless global attach；Game Bar 的临时单 Canvas 绑定必须由 App 显式请求，且只绑定空闲 Widget。

新增授权策略建议：

```text
AttachPolicy {
  monitor_id
  app_identity
  allowed: bool
  scope: Once | Session | Persistent
}
```

早期可先做开发模式 allow-all，但必须显式标记为 dev policy；release 模式至少应有 per-monitor allowlist 或用户确认入口。

## 与当前实现的关系

当前 `CanvasAttached` 可以理解为“单 layer 的早期形态”：

```text
CanvasAttached(canvas_id, surface_handle, ...)
```

下一阶段不应继续在它上面叠加复杂语义，而应引入 Layer / MonitorScene：

```text
LayerAttached(layer_id, owner_app_id, canvas_id, surface_handle, z_order, ...)
```

当前 MonitorLocal 也应演进为 scene overlay / layer 类型，而不是每帧对所有 MonitorLocal surface 盲目 replay。

## 实施阶段

### Phase 1：基础协议与状态模型

- 新增 `MonitorScene` / `Layer` / `VirtualWindow` 数据结构。
- 新增 layer create/update/destroy 协议。
- 增加 ownership 测试：App A 不能创建/更新 App B 的 Layer。
- 保持当前 App -> Canvas -> Monitor 路径可作为开发 smoke，但不为旧协议做长期兼容承诺。

### Phase 2：Monitor metrics

- Desktop monitor 上报 client size / DPI。
- Game Bar widget 上报 host element size / rasterization scale。
- Core 根据 metrics 创建 scene output surface。
- resize 时重新发送 composite surface handle。

### Phase 3：Core composite output

- 为每个 MonitorScene 创建 `SceneResources`。
- 实现 layer 排序、clear、transform、opacity、clip。
- 先支持 FillRect / bitmap / texture composition 的最小路径。
- 增加 frame time、drop、present outcome metrics。

### Phase 4：Game Bar 多 App 验证

- 一个 Game Bar Widget 挂载一个 composite output surface。
- 两个 demo App 同时创建 Layer。
- Core 在同一 Game Bar scene 内按 z_order 合成。
- App 退出时只移除自己的 Layer，不影响其他 App。

### Phase 5：VirtualWindow 交互

- 添加 virtual window bounds / title / focus。
- Game Bar 提供简单窗口列表或布局 UI。
- 输入反馈由 Monitor 上报给 Core，再由 Core 路由到目标 App。

## 测试矩阵

最低验证：

1. App A + App B 同时连接 Core，各自创建 Canvas。
2. 两者分别在同一个 Game Bar MonitorScene 创建 Layer。
3. z_order 改变后画面顺序正确。
4. 隐藏 App A layer 不影响 App B。
5. App A 断开后 App B 仍在同一 Monitor 中显示。
6. Game Bar 关闭后 Core 清理该 MonitorScene 的 output surface，不销毁 App Canvas。
7. Core device-lost 后能重建 scene output 并重新发送 surface handle。
8. 非 owner App 更新 Layer 被拒绝。

## 当前不做

- 不实现多个 App 直接并发写同一 Canvas。
- 不让 Game Bar widget 自己变成 compositor。
- 不恢复旧 auto attach。
- 不把 Desktop monitor 的多 HWND 模型套到 Game Bar 上。
- 不在没有 metrics/device-lost/observability 的情况下直接堆完整 virtual window 系统。
