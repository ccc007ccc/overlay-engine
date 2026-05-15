# desktop-window-monitor

`desktop-window-monitor` 是 overlay 栈的独立桌面监视器进程。它连接 `core-server`，接收 Core 通过 DComp shared surface 传来的 Canvas 内容，并在普通 Win32 窗口中呈现。

## 核心语义

v1.0 的显示模型分成三层：

- **App**：业务进程，创建 Canvas 并提交渲染命令，例如 `demo-app`。
- **Core**：`core-server`，负责接收 App 命令、维护 shared surface，并向各 Monitor 分发 World / MonitorLocal 内容。
- **Monitor**：显示端，例如本包的 desktop-window monitor 或后续 Game Bar widget。

同一 Canvas 同时支持两类坐标空间：

- **World / canvas-space**：使用 logical canvas 坐标，受 viewport transform 影响。窗口像观察窗一样查看同一张全局画布的不同区域。
- **MonitorLocal**：使用当前 monitor 的本地坐标，不随 viewport 移动。适合状态角标、边框、FPS 条等每个监视器独立的元素。

```text
push_space(World)          # 默认：logical canvas -> viewport -> monitor
  draw_grid / draw_axis
pop_space()

push_space(MonitorLocal)   # 当前 monitor local px/DIP，不应用 viewport offset
  draw_status_badge / draw_border
pop_space()
```

Core 对同一个 Canvas 给每个 attached Monitor 出帧时，会分别解析 World 与 MonitorLocal 命令。这样同一画布里可以既有固定在逻辑画布上的元素，也有固定在每个监视器窗口上的元素。

## 关键链路

```text
App:
  RegisterApp -> CreateCanvas -> SubmitFrame

Core:
  CreatePresentationManager
  DCompositionCreateSurfaceHandle -> NT HANDLE
  CreatePresentationSurface(handle)
  AddBufferFromResource(texture)
  DuplicateHandle 到 Monitor 进程
  SetBuffer + Present

Monitor:
  RegisterMonitor
  recv CanvasAttached / MonitorLocalSurfaceAttached
  dcomp.CreateSurfaceFromHandle(handle)
  visual.SetContent(wrapper IUnknown)
  visual.SetTransform(logical/render scale + negative viewport origin)
  target.SetRoot(visual)
  dcomp.Commit()
```

## 运行

### Release 安装后

正式 release 中，Desktop Window Monitor 是可选组件。选择该组件时，安装器会在安装目录写入：

```ini
Launch=desktop-window-monitor.exe
```

`core-server.exe` 启动后会读取同目录的 `config.ini`，并自动拉起 `desktop-window-monitor.exe`。因此普通用户只需要从开始菜单或桌面快捷方式启动 overlay-engine，不需要手动单独启动 monitor。

### 开发模式

```powershell
cargo build --release -p desktop-window-monitor

# Terminal 1: Core
.\target\release\core-server.exe

# Terminal 2: Monitor
.\target\release\desktop-window-monitor.exe

# Terminal 3: Demo App
.\target\release\demo-app.exe
```

预期：desktop-window monitor 弹出普通 Win32 窗口，显示 `demo-app` 提交的全局画布内容，并在客户区左上角显示 MonitorLocal 的 cyan 徽章/FPS 条。

## 验证点

- 拖动窗口时，World 内容表现为“固定在屏幕/逻辑画布上”，窗口像观察窗。
- 多个 monitor 窗口同时存在时，每个窗口客户区左上角应独立显示 MonitorLocal 内容。
- 改 App render resolution 时，网格坐标位置与比例应保持一致，只是清晰度不同。

## 已验证

- `DCompositionCreateSurfaceHandle` + `DuplicateHandle` 跨进程传递可用。
- `IPresentationManager::CreatePresentationSurface` 绑定 NT handle 可用。
- `dcomp.CreateSurfaceFromHandle + visual.SetContent` 在普通 Win32 进程可用。
- Win32 monitor 侧具备 viewport transform 所需 API：`visual.SetTransform2(Matrix3x2)`。

## 后续

- App 关闭后的 Monitor 生命周期清退由 `canvas-monitor-lifecycle` 后续 Change-D 实现。
- 单实例 + 多窗口模型由后续 Change-E 实现。
- Game Bar widget lifecycle 接入不在本阶段范围内。
