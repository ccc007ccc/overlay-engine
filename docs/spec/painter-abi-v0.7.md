# Painter ABI v0.7 — 矢量图元扩展 + 资源系统

> **状态**：ACCEPTED（用户已审阅 7 个开放问题，2026-05）
> **作者**：Claude (drafted) / 用户审定
> **日期**：2026-05
> **基线**：v0.6 DComp（lib.rs 顶部历史段）
> **目标**：把 Painter 从「3 个命令」扩到「能画绝大多数 2D HUD + 视频 overlay」

## 0. 决策摘要 (TL;DR)

| 项 | 决策 |
|----|------|
| Painter 新命令 | 12 个矢量图元 + transform/clip + 渐变 |
| Painter 内部实现 | `enum DrawCmd` + `match`，为未来命令流铺路（决策 10.5） |
| Bitmap 资源 | u32 handle id，`BITMAP_SLOT_CAPACITY=1024` 常量化（决策 10.4），`renderer_destroy_bitmap` 显式释放 |
| 外部纹理 | RGBA8 / NV12，`renderer_update_texture` 每帧 push |
| 本地视频 | Media Foundation IMFSourceReader → 共享 D3D11 device → `renderer_video_present_frame` 转 bitmap handle |
| 视频命名空间 | `video_open_file` 后缀显式（决策 10.2），为 v0.8+ `_url` / `_stream` 预留 |
| 屏幕捕获 | Windows.Graphics.Capture（WinRT）→ D3D11 surface → 同 bitmap handle，区域裁剪走 `src_rect`（决策 10.3） |
| Path opcode | 仅绝对坐标（0x01-0x05），0x80+ 留 SVG 相对坐标（决策 10.1） |
| 资源句柄统一 | 三种来源（load/external/video/capture）输出**同一种** `BitmapHandle`，painter 只认 handle，不关心来源 |
| **画布管理** | `renderer_resize_canvas` 显式 host 调（WM_SIZE 触发），`renderer_begin_frame` 新增 2 个出参告诉业务当前画布尺寸；**不引入 canvas_mode 枚举**——host 调用模式即策略（决策 10.8） |
| ABI 兼容 | v0.6 老接口绝大多数保留；**`renderer_begin_frame` 签名破坏性变更**（加 2 个出参），其他 v0.6 ABI 不动 |
| 实施分期 | 5 个 phase（2+3 合并），每 phase commit + tag（决策 10.6 / 10.7） |

## 1. 背景与范围

### 1.1 v0.6 现状

```rust
renderer_clear(rgba)
renderer_fill_rect(x, y, w, h, rgba)
renderer_draw_text(utf8, ..., x, y, size, rgba)
```

只有这三个命令 + begin/end frame。底层 D2D device context 完整可用，painter.rs 已经管理 brush 池。

### 1.2 v0.7 加什么

**矢量图元（Painter 扩展）**：
- 线条：`draw_line` / `draw_polyline`
- 矩形：`stroke_rect` / `fill_rounded_rect` / `stroke_rounded_rect`
- 椭圆：`fill_ellipse` / `stroke_ellipse`
- 路径：`fill_path` / `stroke_path`（任意几何）
- 渐变：`fill_rect_gradient_linear` / `fill_rect_gradient_radial`
- 状态：`push_clip_rect` / `pop_clip` / `set_transform` / `reset_transform`

**资源系统**（3 种来源，1 个 handle 类型）：
- Bitmap from PNG/JPG file
- Bitmap from memory bytes
- External texture（业务方 push RGBA / NV12）
- Video frame（MF 解码 → 共享 surface）
- Screen/Window capture frame（WGC → 共享 surface）

→ 全部输出 `BitmapHandle`（u32），painter 只有一个 `draw_bitmap(handle, src_rect, dst_rect, opacity)`。

**画布管理**（v0.7 新加）：

`renderer_resize_canvas` + `renderer_begin_frame` 出参 → 让 host 在窗口 resize 时显式
切画布；让业务每帧拿到当前画布尺寸做百分比布局。详细见 §2.6。

### 1.3 画布与渲染分辨率

v0.7 起明确「画布」语义——**画布 = D2D render target 像素数 = swap chain back buffer
像素数 = 业务画图坐标系**，三位一体不分层。这意味着：

1. 业务调 `fill_rect(640, 360, 200, 200, ...)` 时，(640, 360) 是**画布像素坐标**
   （不是 logical pixel，不是 world unit，不是 DIP）
2. 画布尺寸由 host 决定：创建时给初始尺寸（`renderer_create`）；运行时通过
   `renderer_resize_canvas` 显式改
3. 画布尺寸变了 → 内核内部 ResizeBuffers + 重建 D2D bitmap target；业务画的内容
   按**新尺寸**重新解释（因此业务必须每帧调 `begin_frame` 拿当前画布尺寸）

不引入 mode 枚举（不像 Bevy `ScalingMode` / Unity `Canvas Scaler`）。host 想要的所有
策略都通过「调或不调 `resize_canvas`」实现：

| host 想要的行为 | host 怎么做 |
|---|---|
| **画布跟随物理像素**（点对点，desktop-window 默认） | `winit::WindowEvent::Resized` → `resize_canvas(physical_w, physical_h)` |
| **画布固定**（widget v0.6 行为，DComp 自动拉伸到 visual size） | 创建后**不调** `resize_canvas`，画布永远 = 初始尺寸 |
| **用户面板选档位**（720p / 1080p / 4K） | 用户改设置 → host 调 `resize_canvas(选中档位)` |
| **保持长宽比 letterbox** | host 算等比矩形 → `resize_canvas(等比 w, 等比 h)` + DComp visual.Size 留黑边 |

业务侧应对画布变化的策略（**业务责任**，不是 ABI 责任）：

- **百分比布局**：`fill_rect(canvas_w * 0.4, canvas_h * 0.4, canvas_w * 0.2, canvas_h * 0.2, ...)`
- **纵向固定**（Bevy `FixedVertical` 风格）：内部记 `logical_h = 720`，每帧算
  `scale = canvas_h / logical_h`，调 `set_transform(scale, 0, 0, scale, 0, 0)` 后
  在 logical 坐标里画图
- **绝对像素**（不自适应）：业务自己接受"窗口大就画面占比小"

ABI 不替业务做选择。

### 1.4 不在范围

- 3D / mesh / shader 自定义（要做就是另一个 renderer）
- 音频（视频是「画面」，声音由业务方走 MediaPlayer 自己播）
- 字幕渲染（用 draw_text 自己合成）
- 视频编码 / 录制（renderer 是 sink，不是 source）
- 画布 mode 枚举（FIXED / ADAPTIVE / STEPPED）—— host 调用模式即策略（决策 10.8）
- DPI scale_factor 暴露 ABI——host 自己调 OS API（GetDpiForWindow），不污染 ABI

## 2. ABI 设计

### 2.1 命名约定

| 前缀 | 含义 |
|------|------|
| `renderer_*` | C ABI 入口（`extern "system"`） |
| `renderer_<verb>_<noun>` | 动作 + 对象，e.g. `renderer_load_bitmap_from_file` |
| `out_*` 参数 | 输出指针，调用方分配 |
| 颜色参数 | premultiplied float [0,1]，rgb ≤ a |
| 坐标参数 | canvas-space pixel，f32 |

### 2.2 错误码扩展

```rust
// v0.6 老的保留
RENDERER_OK                  =  0
RENDERER_ERR_INVALID_PARAM   = -1
RENDERER_ERR_DEVICE_INIT     = -2
RENDERER_ERR_SWAPCHAIN_INIT  = -3
RENDERER_ERR_THREAD_INIT     = -4
RENDERER_ERR_NOT_ATTACHED    = -5
RENDERER_ERR_FRAME_HELD      = -6
RENDERER_ERR_FRAME_ACQUIRE   = -7

// v0.7 新增
RENDERER_ERR_RESOURCE_NOT_FOUND  = -8   // bitmap/video handle 失效
RENDERER_ERR_RESOURCE_LIMIT      = -9   // slot table 满（默认 1024 个）
RENDERER_ERR_DECODE_FAIL         = -10  // 图片/视频解码失败
RENDERER_ERR_IO                  = -11  // 文件读取失败
RENDERER_ERR_UNSUPPORTED_FORMAT  = -12  // 编码格式不支持
RENDERER_ERR_CAPTURE_INIT        = -13  // WGC 初始化失败 / 系统不支持
RENDERER_ERR_CANVAS_RESIZE_FAIL  = -14  // resize_canvas 时 ResizeBuffers / 重建 D2D target 失败
```

### 2.3 Painter 矢量图元 ABI

#### 2.3.1 线条

```rust
/// 画一条直线。stroke_width 是 canvas-space 像素。
/// dash_style: 0=solid, 1=dash, 2=dot, 3=dash_dot
renderer_draw_line(
    h: *mut Renderer,
    x0: f32, y0: f32,
    x1: f32, y1: f32,
    stroke_width: f32,
    r: f32, g: f32, b: f32, a: f32,
    dash_style: i32,
) -> RendererStatus

/// 折线。points 是连续 [x0,y0,x1,y1,...] 数组，count = 点数（不是 float 数）。
/// closed=1 时首尾相接。
renderer_draw_polyline(
    h: *mut Renderer,
    points: *const f32,
    point_count: i32,
    stroke_width: f32,
    r: f32, g: f32, b: f32, a: f32,
    closed: i32,
) -> RendererStatus
```

#### 2.3.2 矩形

```rust
/// 矩形描边（v0.7）。已有 fill_rect 不变。
renderer_stroke_rect(
    h: *mut Renderer,
    x: f32, y: f32, w: f32, h_: f32,
    stroke_width: f32,
    r: f32, g: f32, b: f32, a: f32,
) -> RendererStatus

/// 圆角矩形。radius_x / radius_y 不同 = 椭圆角。
renderer_fill_rounded_rect(
    h: *mut Renderer,
    x: f32, y: f32, w: f32, h_: f32,
    radius_x: f32, radius_y: f32,
    r: f32, g: f32, b: f32, a: f32,
) -> RendererStatus

renderer_stroke_rounded_rect(
    h: *mut Renderer,
    x: f32, y: f32, w: f32, h_: f32,
    radius_x: f32, radius_y: f32,
    stroke_width: f32,
    r: f32, g: f32, b: f32, a: f32,
) -> RendererStatus
```

#### 2.3.3 椭圆

```rust
renderer_fill_ellipse(
    h: *mut Renderer,
    cx: f32, cy: f32, rx: f32, ry: f32,
    r: f32, g: f32, b: f32, a: f32,
) -> RendererStatus

renderer_stroke_ellipse(
    h: *mut Renderer,
    cx: f32, cy: f32, rx: f32, ry: f32,
    stroke_width: f32,
    r: f32, g: f32, b: f32, a: f32,
) -> RendererStatus
```

#### 2.3.4 路径（任意几何）

路径用「命令缓冲」编码，业务一次性给一个 byte 流，Rust 端解码喂给 `ID2D1PathGeometry`。

```c
// path opcode（v0.7：仅绝对坐标）：
// 0x01 MOVE_TO    f32 x, f32 y
// 0x02 LINE_TO    f32 x, f32 y
// 0x03 BEZIER     f32 x1, y1, x2, y2, x3, y3   (cubic)
// 0x04 ARC        f32 x, y, rx, ry, rotation, large_arc(0/1), sweep(0/1)
// 0x05 CLOSE
//
// reserved 区间（见 10.1）：
// 0x06-0x7F   v0.8+ 绝对坐标新增（如二次贝塞尔、平滑曲线）
// 0x80-0xFF   v0.8+ 相对坐标 / SVG 兼容变体
//
// v0.7 实现遇到任何 >= 0x06 的 opcode → 返 RENDERER_ERR_UNSUPPORTED_FORMAT，
// 不静默崩溃，让未来加新 opcode 时老二进制有明确报错。
```

```rust
renderer_fill_path(
    h: *mut Renderer,
    path_bytes: *const u8,
    path_len: i32,
    r: f32, g: f32, b: f32, a: f32,
) -> RendererStatus

renderer_stroke_path(
    h: *mut Renderer,
    path_bytes: *const u8,
    path_len: i32,
    stroke_width: f32,
    r: f32, g: f32, b: f32, a: f32,
    dash_style: i32,
) -> RendererStatus
```

**为什么 byte 流而不是单条命令**：减少 P/Invoke 次数；一个复杂图标可能 50+ 顶点，逐条调用是 50 次 marshalling。byte 流一次过。

### 2.4 状态命令

```rust
/// 推一个矩形 clip，配对 pop_clip。栈结构。
renderer_push_clip_rect(
    h: *mut Renderer,
    x: f32, y: f32, w: f32, h_: f32,
) -> RendererStatus

renderer_pop_clip(h: *mut Renderer) -> RendererStatus

/// 设置 2D 仿射变换。3x2 矩阵：[m11, m12, m21, m22, dx, dy]
/// 等同 D2D Matrix3x2F。后续命令应用这个变换。
renderer_set_transform(
    h: *mut Renderer,
    matrix: *const f32,  // 6 个 float
) -> RendererStatus

/// 重置成 identity（v0.6 内部默认的 viewport translate）。
renderer_reset_transform(h: *mut Renderer) -> RendererStatus
```

### 2.5 渐变

线性渐变和径向渐变都通过「填充矩形 + gradient stop 数组」表达。stop 数组：连续 `[offset, r, g, b, a, offset, r, g, b, a, ...]`，offset ∈ [0,1]。

```rust
renderer_fill_rect_gradient_linear(
    h: *mut Renderer,
    x: f32, y: f32, w: f32, h_: f32,
    start_x: f32, start_y: f32,
    end_x: f32, end_y: f32,
    stops: *const f32,
    stop_count: i32,
) -> RendererStatus

renderer_fill_rect_gradient_radial(
    h: *mut Renderer,
    x: f32, y: f32, w: f32, h_: f32,
    center_x: f32, center_y: f32,
    radius_x: f32, radius_y: f32,
    stops: *const f32,
    stop_count: i32,
) -> RendererStatus
```

### 2.6 画布管理（v0.7 新增）

#### 2.6.1 概念契约

见 §1.3：画布 = D2D render target = swap chain back buffer = 业务画图坐标系（三位一体）。
画布尺寸由 host 决定（创建时初始尺寸；运行时通过 `renderer_resize_canvas` 显式改）。
**没有 mode 枚举**——host 调用模式即策略（决策 10.8）。

#### 2.6.2 `renderer_create` 语义微调（签名不变）

```rust
/// 创建 renderer。第二、三参数是**初始**画布尺寸；
/// host 之后可通过 renderer_resize_canvas 任意改。
renderer_create(
    canvas_w: i32, canvas_h: i32,
    out_handle: *mut *mut Renderer,
) -> RendererStatus
```

签名跟 v0.6 字节级一致；仅文档语义从「不可变 canvas」放宽为「初始 canvas，
之后可通过 resize_canvas 改」。v0.6 业务（widget v0.6）不调 resize_canvas，
画布永远是初始尺寸——v0.6 行为完全保留。

#### 2.6.3 `renderer_resize_canvas`

```rust
/// 改画布尺寸。内部触发 IDXGISwapChain1::ResizeBuffers + 重建 D2D bitmap target。
///
/// 调用时机：host 在 WM_SIZE / winit::WindowEvent::Resized / 用户改设置面板时调。
/// **不要每帧调** —— ResizeBuffers 是重分配 GPU 缓冲（约 100us~1ms），每帧调
/// 性能损失明显（Microsoft 官方文档明确禁止 per-frame resize）。
///
/// 同尺寸（new_w/new_h 与当前画布相等）调用是 no-op，内部 short-circuit，零开销。
///
/// 返：
/// - 0 OK
/// - -1 INVALID_PARAM   new_w 或 new_h ≤ 0
/// - -14 CANVAS_RESIZE_FAIL  ResizeBuffers / 重建 D2D bitmap 失败（含 device-lost）
/// - -6 FRAME_HELD     当前 frame 在 begin/end 之间（不允许 resize）
renderer_resize_canvas(
    h: *mut Renderer,
    new_w: i32, new_h: i32,
) -> RendererStatus
```

线程契约：跟其他 ABI 一致——单线程 / 外部互斥。host 应在与 begin/end_frame 同一线程
调用，且不能在 begin_frame 之后、end_frame 之前调（返 FRAME_HELD）。

#### 2.6.4 `renderer_begin_frame` 签名变更（破坏性）

v0.6：
```rust
renderer_begin_frame(h, vp_x, vp_y, vp_w, vp_h) -> RendererStatus
```

v0.7：
```rust
/// 开始一帧。内部 SetTarget(bitmap) + BeginDraw + reset transform/clip。
///
/// `out_canvas_w` / `out_canvas_h`：本帧画布尺寸（业务画图坐标系上限）。
/// 业务每帧应基于这两个值做百分比布局——画布尺寸可能在每次 begin_frame 之间
/// 因 host 调 resize_canvas 而变。
///
/// 允许 NULL：业务不需要画布尺寸（比如硬编码绝对坐标）时可传 NULL，跳过写出参。
///
/// `vp_x / vp_y / vp_w / vp_h`：viewport（DComp visual / swap chain 在物理屏上的
/// 位置和大小，物理像素）。语义跟 v0.6 一致。
renderer_begin_frame(
    h: *mut Renderer,
    vp_x: f32, vp_y: f32, vp_w: f32, vp_h: f32,
    out_canvas_w: *mut i32,    // v0.7 新增；NULL 允许
    out_canvas_h: *mut i32,    // v0.7 新增；NULL 允许
) -> RendererStatus
```

**破坏性变更说明**：v0.7 起所有 begin_frame 调用方（widget P/Invoke、未来的
desktop-window FFI、社区 monitor）必须传 6 个参数（4 个 viewport + 2 个出参）。
v0.6 调用方传 5 个参数会 stack misalignment。这是**故意破坏**——见 §8 兼容承诺。

#### 2.6.5 host 调用约定

**winit / Win32 host 推荐流程**（desktop-window 实施）：

```rust
// 创建：初始画布跟 winit 创建窗口的物理像素一致
let physical = window.inner_size();
let h = ffi.create(physical.width as i32, physical.height as i32)?;

// 事件循环
match event {
    WindowEvent::Resized(new_size) => {
        ffi.resize_canvas(new_size.width as i32, new_size.height as i32)?;
    }
    WindowEvent::RedrawRequested => {
        let (cw, ch) = ffi.begin_frame(0.0, 0.0, win_w, win_h)?;
        // 业务用 cw / ch 百分比布局
        ffi.fill_rect(cw as f32 * 0.5 - 100.0, ch as f32 * 0.5 - 100.0, 200.0, 200.0, ...);
        ffi.end_frame()?;
    }
}
```

**XAML / WinRT host 推荐流程**（widget 改造路径）：

```csharp
// SizeChanged 事件
private void OnWidgetSizeChanged(object sender, SizeChangedEventArgs e) {
    var dpi = ScreenInterop.GetDpiForWindow(...);
    int physicalW = (int)(e.NewSize.Width * dpi / 96.0);
    int physicalH = (int)(e.NewSize.Height * dpi / 96.0);
    RendererPInvoke.renderer_resize_canvas(_handle, physicalW, physicalH);
}
```

如果 host **不**调 resize_canvas，画布永远是 `renderer_create` 时的尺寸，
DComp 自动按 visual.Size 拉伸——这就是 v0.6 widget 的行为，v0.7 对兼容这条路径。

## 3. Bitmap 资源系统

### 3.1 设计目标

- **单一 handle 类型**：业务方拿到 `u32 BitmapHandle`，不需要知道是文件、内存、视频还是捕获
- **显式生命周期**：每个 `*_open` / `*_load` 配一个 `*_destroy`；GC 不靠
- **跨帧持久**：bitmap 在 destroy 前一直可用，painter 可以反复 draw
- **GPU 资源限制**：默认 1024 个 slot，超过返 `RENDERER_ERR_RESOURCE_LIMIT`

### 3.2 Handle 生成

```rust
// 内部 slot table:
struct ResourceTable {
    slots: Vec<Option<Resource>>,    // index = handle id
    free_list: Vec<u32>,             // 回收队列
    generation: Vec<u16>,            // 防 ABA：handle 高 16 位 = generation
}

// BitmapHandle 编码：
// bits [0..16]   slot index (max 65535)
// bits [16..32]  generation counter (slot 重用时 +1)
// 0 = invalid handle (业务可以零初始化)
```

ABA 保护：destroy 后 slot 进 free_list，重新分配时 generation +1，老 handle 拿过来 generation 不匹配 → `RENDERER_ERR_RESOURCE_NOT_FOUND`。

### 3.3 Bitmap 加载

#### 3.3.1 从文件 / 内存

```rust
/// PNG/JPG/BMP/GIF/WEBP 通过 WIC 解码。
/// 解码后立刻上传 GPU（ID2D1Bitmap），返回 handle。
renderer_load_bitmap_from_file(
    h: *mut Renderer,
    utf8_path: *const u8, path_len: i32,
    out_handle: *mut u32,
) -> RendererStatus

renderer_load_bitmap_from_memory(
    h: *mut Renderer,
    bytes: *const u8, byte_len: i32,
    out_handle: *mut u32,
) -> RendererStatus

renderer_get_bitmap_size(
    h: *mut Renderer,
    bm: u32,
    out_w: *mut u32, out_h: *mut u32,
) -> RendererStatus

renderer_destroy_bitmap(h: *mut Renderer, bm: u32) -> RendererStatus
```

#### 3.3.2 外部纹理（业务 push 帧）

```rust
/// 创建一个空纹理，业务后续用 update 喂数据。
/// format: 0=BGRA8, 1=RGBA8, 2=NV12（视频常见）
renderer_create_texture(
    h: *mut Renderer,
    width: u32, height: u32, format: i32,
    out_handle: *mut u32,
) -> RendererStatus

/// 上传一帧像素。stride = 每行字节数（含 padding）。
/// NV12 时 bytes 是 Y plane + UV plane 连续布局，stride 是 Y plane 的；
/// UV plane stride = stride（NV12 的特性，UV 半分辨率但相同 stride）。
renderer_update_texture(
    h: *mut Renderer,
    tex: u32,
    bytes: *const u8, byte_len: i32,
    stride: i32,
) -> RendererStatus
```

### 3.4 Bitmap 渲染

```rust
/// 把 bitmap 画到 canvas。dst_rect 决定输出区域，src_rect 决定 bitmap 中哪块。
/// src_rect 全 0 = 整个 bitmap。
/// opacity ∈ [0,1] 整体透明度。
/// interp_mode: 0=nearest, 1=linear（默认）, 2=cubic
renderer_draw_bitmap(
    h: *mut Renderer,
    bm: u32,
    src_x: f32, src_y: f32, src_w: f32, src_h: f32,
    dst_x: f32, dst_y: f32, dst_w: f32, dst_h: f32,
    opacity: f32,
    interp_mode: i32,
) -> RendererStatus
```

## 4. 视频管线

### 4.1 本地视频文件（Media Foundation）

#### 设计

- 一个 video 实例 = 一个 `IMFSourceReader` + 一个共享 `BitmapHandle`
- 业务调 `renderer_video_open` → 拿到 `VideoHandle`（独立 id 空间）
- 业务每帧调 `renderer_video_present_frame(video_handle)`，内部解一帧到 GPU 共享纹理，返回**当前可用的 BitmapHandle**
- 业务用 `renderer_draw_bitmap(bitmap_handle, ...)` 把这一帧画上去
- 业务自己驱节奏（业务知道想什么时候播；renderer 只负责"给我下一帧"）

#### ABI

> v0.7 仅本地文件入口，命名带 `_file` 后缀为 v0.8+ 预留 `_url` / `_stream` 命名空间（见 10.2）。

```rust
renderer_video_open_file(
    h: *mut Renderer,
    utf8_path: *const u8, path_len: i32,
    out_video_handle: *mut u32,
) -> RendererStatus

/// 视频元数据：duration_ms、宽高、帧率
#[repr(C)]
struct VideoInfo {
    duration_ms: u64,
    width: u32,
    height: u32,
    fps_num: u32,    // 帧率分数 fps = num/den
    fps_den: u32,
}

renderer_video_get_info(
    h: *mut Renderer,
    video: u32,
    out_info: *mut VideoInfo,
) -> RendererStatus

renderer_video_seek(
    h: *mut Renderer,
    video: u32,
    time_ms: u64,
) -> RendererStatus

/// 解一帧到 GPU 纹理，返回该帧的 BitmapHandle。
/// 同一 video 重复调用，每次返回的 BitmapHandle 都**一样**（内部循环用同一个纹理）。
/// 业务用完后**不要** destroy 这个 bitmap handle —— 由 video_close 统一回收。
///
/// out_eof = 1 表示视频已结束，本帧仍是最后一帧（业务自行决定循环 / 停止）。
renderer_video_present_frame(
    h: *mut Renderer,
    video: u32,
    out_bitmap: *mut u32,
    out_eof: *mut i32,
) -> RendererStatus

renderer_video_close(h: *mut Renderer, video: u32) -> RendererStatus
```

#### 实现

```
+-----------------+      +-------------------+
| IMFSourceReader |      | D3D11Device       |
| (MF 解码)        |<---->| (与 painter 共享)  |
+-----------------+      +-------------------+
        |
        v
+--------------------+
| IMFSample          |
| → IMFMediaBuffer   |
| → ID3D11Texture2D  |  ← 共享 D3D11 device，无 CPU readback
+--------------------+
        |
        v
+----------------+
| ID2D1Bitmap1   |  ← 同一 D2D device context 直接 wrap
| (slot table 里) |
+----------------+
        |
        v
  draw_bitmap()
```

关键：MF 配 `MF_SOURCE_READER_D3D_MANAGER` + 我们的 D3D11 device → 解码出来的 NV12 surface 直接是 D3D11 资源，零拷贝转 D2D bitmap。

#### 不做

- 音频解码（业务自己用 MediaPlayer 同步播 audio）
- 复杂 av sync（业务自己时钟驱动，renderer 不管"现在该是哪一帧"）
- DRM 受保护内容

### 4.2 屏幕 / 窗口捕获（Windows.Graphics.Capture）

#### 设计

WGC 是 Win10 1803+ 的标准 API（屏幕录制都用这个），返回 `Direct3D11CaptureFrame` = `IDirect3DSurface` = D3D11 texture。同样可以零拷贝转 D2D bitmap。

```rust
// target 类型
const RENDERER_CAPTURE_TARGET_PRIMARY_MONITOR: i32 = 0;
const RENDERER_CAPTURE_TARGET_MONITOR_BY_INDEX: i32 = 1;
const RENDERER_CAPTURE_TARGET_HWND:             i32 = 2;
```

```rust
renderer_capture_open(
    h: *mut Renderer,
    target_type: i32,
    target_param: u64,         // monitor index 或 HWND（u64 跨 32/64）
    cursor_enabled: i32,
    out_capture_handle: *mut u32,
) -> RendererStatus

/// 拉最新一帧到 GPU 纹理。同 video_present_frame，反复调返回同一 BitmapHandle。
/// 没有新帧（capture 还没产出）时仍返回上一帧 + status RENDERER_OK。
renderer_capture_present_frame(
    h: *mut Renderer,
    capture: u32,
    out_bitmap: *mut u32,
) -> RendererStatus

renderer_capture_get_size(
    h: *mut Renderer,
    capture: u32,
    out_w: *mut u32, out_h: *mut u32,
) -> RendererStatus

renderer_capture_close(h: *mut Renderer, capture: u32) -> RendererStatus
```

#### 平台限制

- WGC 需要 Win10 1903+（更老返 `RENDERER_ERR_CAPTURE_INIT`）
- 抓窗口时若窗口最小化或被占满则帧不更新
- 部分受保护窗口（DRM 视频、UAC dialog）会出黑帧（OS 限制）
- HDR 显示器抓出来是 16 位浮点 RGB；painter 当前 8bpp，需要 tone-map（v0.7 简单做：直接 clamp）

## 5. C# P/Invoke 层

每个 ABI 在 `monitors/game-bar-widget/Native/RendererPInvoke.cs` 加 `[DllImport]` 包装。Bitmap handle 在 C# 用 `struct BitmapHandle { public uint Value; }` 包一下，避免误传普通 uint。

```csharp
public readonly struct BitmapHandle : IEquatable<BitmapHandle>
{
    public readonly uint Value;
    public bool IsValid => Value != 0;
    // ...
}

public readonly struct VideoHandle { public readonly uint Value; }
public readonly struct CaptureHandle { public readonly uint Value; }
```

提供 `IDisposable` 包装类（`RendererBitmap` / `RendererVideo` / `RendererCapture`），析构时调 `*_destroy`。

## 6. 实施分期 & 验收

> Phase 2+3 已合并（决策 10.6），节省的 0.5 天投入 Phase 4（视频）。
> 每个 phase 完成 → 跑测试 → commit → tag `v0.7-phase{N}`（决策 10.7）。

### Phase 1: Painter 矢量图元（半天）

**交付**：
- `painter.rs` 加 11 个 `cmd_*` 方法 + brush/strokestyle 缓存
- 内部用 `enum DrawCmd { ... }` + `match` 派发（决策 10.5），为未来命令流铺路
- `lib.rs` 加 11 个 `extern "system" fn renderer_*`
- `RendererPInvoke.cs` 同步 11 个 `[DllImport]`
- 单元测试 ≥ 5 个 golden（offscreen render → readback → 像素比对）
- 1 个 demo widget 命令组合：圆角血条 + 椭圆雷达

**通过判据**：
- `cargo test --release` 全绿
- `build-all.ps1 -Configuration Release` 成功
- Game Bar widget 实际渲染 demo 画面
- `git tag v0.7-phase1`

### Phase 2: Bitmap 资源系统 + 外部纹理（合并，1 天）

**交付**：
- `resources.rs`（新模块）：
  - `const BITMAP_SLOT_CAPACITY: usize = 1024`（决策 10.4）
  - slot table + ABA generation
  - 三种来源（file/memory/external texture）共享同一套 handle
- WIC 解码 → ID2D1Bitmap（PNG/JPG/BMP/GIF/WEBP）
- `create_texture` / `update_texture`（BGRA8 / RGBA8 完整；NV12 仅 Y plane）
- `draw_bitmap` ABI
- 测试：load → draw → destroy → handle 失效；CPU checkerboard upload → 像素比对

**通过判据**：
- 在 widget 显示一张 PNG
- 业务能把 CPU 生成的 RGBA 帧画上去
- ABA：destroy 后老 handle 返 `RENDERER_ERR_RESOURCE_NOT_FOUND`
- `git tag v0.7-phase2`

### Phase 3: 本地视频（2 天，含 0.5 天 buffer）

**交付**：
- `mediafoundation.rs`：MF init + IMFSourceReader 包装
- D3D Manager 共享 device 配置
- NV12 → BGRA shader 或 D2D NV12 effect
- `video_open_file` / `get_info` / `seek` / `present_frame` / `close` 全套（决策 10.2）
- 视频解码出的 D3D11 surface → bitmap slot（复用 Phase 2 的 handle 系统）
- 测试：1 秒 mp4 走完，pixel 抽样

**通过判据**：
- widget 内播一段 .mp4，30s 不崩
- `git tag v0.7-phase3`

### Phase 4: 屏幕捕获（1 天）

**交付**：
- `wgc.rs`：WinRT WGC 包装
- 主显示器 / 窗口 / monitor by index 三种 target
- `capture_open / present_frame / get_size / close`
- 复用 Phase 2 的 bitmap handle 系统

**通过判据**：
- widget 显示桌面镜像，30s 60fps 不掉
- `git tag v0.7-phase4`

### Phase 5: Path + 渐变（半天）

**交付**：
- path opcode 解码器（仅 0x01-0x05，0x06+ 返 unsupported）
- `ID2D1PathGeometry` 构建
- linear/radial gradient brush 缓存（避免每帧 alloc）
- spec 状态从 ACCEPTED → SHIPPED

**通过判据**：
- 用 path 画一个 SVG 风格 logo + 渐变背景
- `git tag v0.7-phase5`

### 总工程量

| Phase | 内容 | 估算 | 累计 |
|-------|------|------|------|
| 1 | Painter 矢量图元 | 0.5 天 | 0.5 |
| 2 | Bitmap + 外部纹理（合并） | 1.0 天 | 1.5 |
| 3 | 本地视频 | 2.0 天 | 3.5 |
| 4 | 屏幕捕获 | 1.0 天 | 4.5 |
| 5 | Path + 渐变 | 0.5 天 | 5.0 |

约 **5 天**，按 phase 分别提交，每个 phase 独立可上。

## 7. 风险与对策

| 风险 | 影响 | 对策 |
|------|------|------|
| MF D3D Manager 初始化在某些 GPU 上失败（老 Intel HD） | video 不可用 | 检测失败 → fallback 到 CPU readback 路径（`MF_READWRITE_DISABLE_CONVERTERS=0` + 软解） |
| WGC 在 Game Bar widget 进程里被沙箱限制 | capture 拿不到帧 | 上线前在 Game Bar 实测；不行就退化为只支持 standalone host |
| Bitmap slot table 满 1024 | 长跑泄漏 | 加 `renderer_get_resource_stats` ABI 让业务监控，文档警示 |
| NV12 → BGRA 转换性能 | 视频卡顿 | 用 D2D `D2D1_NV12_PLANES_EFFECT` 或自写 PS shader |
| 老 v0.6 业务代码与新 transform/clip 状态污染 | 老接口画面错位 | begin_frame 进入时强制 reset 所有状态（clip 栈清空、transform = viewport identity） |
| 32+ 个新 ABI 导致 P/Invoke 表臃肿 | C# 端大量 boilerplate | 用 T4 / 源生成器统一生成（v0.8 可优化，v0.7 手写一遍） |
| begin_frame 签名变更破坏 widget v0.6 P/Invoke | widget v0.6 到 v0.7 升级时直接 crash（stack misalignment） | 实施时 widget P/Invoke 必须跟 renderer 同 phase 升级；CI 加 ABI binary 兼容性检查（`cargo-public-api` / 手写 abi 版本号） |
| host 在 begin/end_frame 之间调 resize_canvas | D2D bitmap target 还在 SetTarget 状态，重建会脏 | resize_canvas 进入时检测帧状态，返 FRAME_HELD（错误码 -6） |
| 用户连续拖窗口产生 100+ WM_SIZE，每个都 ResizeBuffers | 拖动卡顿 | host 端做 debounce（连续 resize 合并到停下后那一次）；renderer 端 same-size short-circuit 减少不必要的 ResizeBuffers |

## 8. 与 v0.6 的兼容

**保留不变（字节级一致）**：

- `renderer_create` / `renderer_destroy` / `renderer_resize` / `renderer_get_swapchain`
- `renderer_set_log_callback` / `renderer_get_perf_stats` / `renderer_last_error_string`
- `renderer_clear` / `renderer_fill_rect` / `renderer_draw_text` / `renderer_end_frame`
- 错误码 0..-7 含义不变；新错误码 -8 起

**破坏性变更（v0.7 起）**：

- `renderer_begin_frame` 签名加 2 个出参（`out_canvas_w` / `out_canvas_h`）。所有调用方
  必须更新，传 5 个参数会 stack misalignment 崩溃。这是项目前期对架构良好基础的
  必要投入（决策 10.8）。

  迁移方法：调用方原本的 5 参调用改为 6 参，新增的 2 个出参允许传 NULL（不需要
  画布尺寸时跳过写出参；推荐 desktop-window 之类的新 monitor 都传非 NULL，
  以便每帧拿当前画布尺寸做百分比布局）。

**新加（v0.7 only）**：

- `renderer_resize_canvas`（§2.6.3）—— host 在 WM_SIZE 触发，**不应每帧调**
- 全部矢量图元 / bitmap / 视频 / 捕获 ABI（§2.3-2.5、§3、§4）

**行为变更（向前兼容）**：

- `begin_frame` 内部加一行：清空 clip 栈、reset transform 到 viewport translate。
  这是新行为但向前兼容（v0.6 业务从不 push clip / set transform，所以观察不到差异）

**widget 端迁移**（v0.7 实施 step）：

widget 用的是 v0.6 P/Invoke。v0.7 实施时 widget 必须同步更新：

1. `RendererPInvoke.cs` 的 `renderer_begin_frame` 签名改 6 参（加 `out IntPtr canvasW, out IntPtr canvasH` 或 `int*` 双指针）
2. 调用点改成接收画布尺寸（如果 widget 业务画图改百分比布局 → 用之；如果保持
   v0.6 写死坐标 → 传 IntPtr.Zero / nullptr 跳过出参）
3. 加 `renderer_resize_canvas` 的 P/Invoke 包装（widget 是否真的调它取决于
   widget 是否要"画布跟随物理像素"——见 desktop-window/SPEC §13）

## 9. 文档与 changelog

- 本文档为权威 spec，落地后更新到 `docs/spec/painter-abi-v0.7.md`（DRAFT 标志改 ACCEPTED）
- 每个 phase 完成后在 `lib.rs` 顶部历史段加一行
- v0.6 → v0.7 的迁移指南：**无需迁移**，纯新增

## 10. 已决策（用户审定 2026-05）

7 个长远判断的最终结论。每条都明文写出"v0.7 不做 / 怎么留扩展位"，
确保 v0.8+ 不需要破坏性变更。

### 10.1 Path opcode：不支持 SVG 相对坐标，但留扩展位

**决策**：v0.7 只实现绝对坐标 opcode（0x01-0x05）。

**理由**：SVG 相对坐标的真正价值是「直接粘贴 SVG path data」——这是工具链问题，不是渲染问题。
真要接 SVG 资产时，转换应在 C# 层做（一个预处理函数），不应污染 ABI。

**扩展位约定**：

```
0x01-0x7F   绝对坐标 opcode（v0.7 起始：MOVE_TO/LINE_TO/BEZIER/ARC/CLOSE）
0x80-0xFF   reserved（v0.8+ 用于相对坐标变体 / 二次贝塞尔 / 平滑曲线等）
```

实现时强制校验 `opcode >= 0x80` → 返 `RENDERER_ERR_UNSUPPORTED_FORMAT`，
让未来加新 opcode 时老二进制有明确报错而非静默崩溃。

### 10.2 Video URL / HTTP 流：不做，但命名空间预留

**决策**：v0.7 仅本地文件，函数命名带 `_file` 后缀。

**理由**：HTTP 流引入缓冲、重连、带宽估算这些完全不同的生命周期问题。
塞进 v0.7 工期翻倍且测试面爆炸。

**命名空间预留**：

```rust
// v0.7 实现：
renderer_video_open_file(path, ...)         -> VideoHandle

// v0.8+ 预留（不实现，但 API 形状已定）：
renderer_video_open_url(url, ...)           -> VideoHandle  // 同 handle 类型
renderer_video_open_stream(reader_cb, ...)  -> VideoHandle  // pull-mode 业务回调
```

三个 open_* 输出**同一种** `VideoHandle`，后续 `present_frame/seek/close` 不区分来源。
业务代码切换时只改 open 调用。

### 10.3 Capture 区域抓取：不做

**决策**：WGC 是「整窗口/整显示器」粒度，区域裁剪在 `draw_bitmap` 的 `src_rect` 做。

**理由**：与 bitmap 处理方式一致，不为一个使用场景引入第二种裁剪机制。
未来若需硬件级区域（减带宽场景）是独立优化 phase，不影响 ABI 形状。

### 10.4 Bitmap slot 上限 1024：够，但要常量化

**决策**：定义 `const BITMAP_SLOT_CAPACITY: usize = 1024`，所有引用都走它。

**理由**：1024 对 HUD overlay 完全够（复杂 UI ~几十张）。长远风险不是数量，
是「这数字散落各处」。常量化让未来调整只改一处。

实施位置：`rust-renderer/src/renderer/resources.rs`（Phase 2 创建该模块时）。

### 10.5 Painter 批量命令流：不做，但内部用 enum + match

**决策**：v0.7 不暴露命令流入口，但 painter 内部绘制逻辑用 `enum DrawCommand { ... }`
+ `match` 派发，不直接堆 `if let`。

**理由**：

- 命令流的真实价值是「录制/回放/序列化」（debug 工具、跨进程 UI 状态），
  不是减少 P/Invoke（现代 .NET P/Invoke 开销极小）。
- 但命令流 schema 一旦对外暴露就难改。v0.7 ABI 还在成形期，过早固化破坏性大。
- 内部 enum 形式：等 v0.8+ 真要做命令流时，enum 直接是天然序列化对象，
  现在的设计不会浪费。

实施约束：painter.rs 里业务级操作用如下结构

```rust
enum DrawCmd {
    Clear { rgba: [f32; 4] },
    FillRect { x: f32, y: f32, w: f32, h: f32, rgba: [f32; 4] },
    DrawText { text: String, x: f32, y: f32, size: f32, rgba: [f32; 4] },
    StrokeRect { ... },
    DrawLine { ... },
    // ...
}

impl Painter {
    fn execute(&mut self, cmd: &DrawCmd) -> Result<()> {
        match cmd { ... }
    }
}
```

每个 `renderer_*` ABI 入口构造 `DrawCmd` → 调 `execute`。
今天看是「绕一道弯」，但避免 v0.8 大重构。

### 10.6 Phase 顺序：不变，Phase 2+3 合并

**决策**：原 Phase 2（bitmap from file/memory）和 Phase 3（外部纹理）合并为一个 Phase 2，
省下的 0.5 天投入 Phase 4（视频是工期风险最高的）。

**理由**：两者共享同一套 handle / slot table，分开提交 handle 层要写两遍，
合并后测试覆盖更完整。

调整后的工期表见第 6 节。

### 10.7 提交粒度：每 phase commit + tag

**决策**：每个 phase 完成 → 跑测试 → commit → 打 git tag `v0.7-phase{N}`。

**理由**：

- **回归点丢失**：phase 4 出 MF bug 要能 git bisect 到 phase 3 干净状态
- **Review 粒度**：500 行 diff vs 150 行 diff 的 review 质量天壤之别
- **心理安全感**：每 phase 提交后"这部分是好的"，出问题心理负担小

执行约束：

```
phase 完成 → cargo test --release（必须全绿）→ git commit -m "feat(painter): phase N - <主题>" → git tag v0.7-phaseN
```

### 10.8 画布管理：不引入 mode 枚举，host 调用模式即策略

**决策**：v0.7 加 `renderer_resize_canvas` + `renderer_begin_frame` 出参，
**不**加 `canvas_mode` 枚举（不区分 FIXED / ADAPTIVE / STEPPED 等内部状态）。

**调研背景**（2026-05）：调研 Bevy / wgpu / OBS Studio / Direct2D 官方文档：

- **Bevy**：`OrthographicProjection::ScalingMode` 6 种（`WindowSize` / `Fixed` /
  `FixedVertical` / `FixedHorizontal` / `AutoMin` / `AutoMax`）—— 但放在 Camera
  层，wgpu 层完全不管，wgpu Surface configure 永远收物理像素。
- **OBS**：分 Base Resolution（编辑画布）vs Output Resolution（编码输出）两层。
  我们项目是 sink（直接上屏，无编码），不需要这层抽象。
- **Direct2D 官方**（[Walbourn / MS Docs](https://walbourn.github.io/care-and-feeding-of-modern-swapchains/)）：明确警告**不要每帧 ResizeBuffers**，
  应在 WM_SIZE 响应中调（约 100us~1ms 量级，per-frame 有显著性能损失）。

**结论**：

1. mode 枚举 = 内核状态。但 host 想要的所有行为（画布跟随 / 画布固定 / 用户面板
   选档位 / 长宽比 letterbox）都可以通过「在某个事件里调或不调 resize_canvas」实现。
   mode 枚举是多余的状态。
2. ScalingMode（FixedVertical 等）是**业务策略**，应在 host 业务代码里实现
   （用 `set_transform` + 缩放矩阵），不是 ABI 责任。把 ScalingMode 塞 ABI
   会复刻 Bevy 的复杂度但拿不到 Bevy 的好处。
3. 「每帧自动检测 viewport 变化 + 内部 resize」违反 Microsoft 官方推荐。
   显式 `resize_canvas(WM_SIZE)` 才是对的。

**业务侧 ScalingMode 实现示范**（如果业务需要 FixedVertical 720 等价行为）：

```rust
let (cw, ch) = ffi.begin_frame(0.0, 0.0, win_w, win_h, ...)?;
let logical_h = 720.0;
let scale = ch as f32 / logical_h;
ffi.set_transform(scale, 0.0, 0.0, scale, 0.0, 0.0);
let logical_w = cw as f32 / scale;
ffi.fill_rect(logical_w * 0.5 - 100.0, 360.0 - 100.0, 200.0, 200.0, ...); // 永远在 720 高的中央
```

`set_transform` 已经在 §2.4 提供，业务自由组合。

**未来扩展位**：如果 v0.8+ 真的发现需要 mode 枚举（比如内核要做 GPU 负载感知
自动降分辨率），那时再加 `renderer_create_ex(canvas_w, canvas_h, mode_flags)`，
flags 字段 0 = 当前 v0.7 行为。现在不预留，避免过度设计。

任意 phase 测试不绿 → 不 commit / 不 tag，先修。
