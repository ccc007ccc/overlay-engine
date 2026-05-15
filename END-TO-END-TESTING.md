# End-to-End Testing — `animation-and-viewport-fix`

这份文档说明怎么肉眼验证 `animation-and-viewport-fix` 这次修复。
tasks.md 里所有自动化测试都是结构化/编解码层面的,**不启动真实进程**,
所以跑过 ≠ 你本地启动就能看到效果。要真的看到修复,按这里的步骤来。

## 修复前后各自应该看到什么

### 缺陷 A — 动画停滞

- **不动任何窗口**,盯住屏幕中间那个橙色方块
- **修复前**:橙色块在启动瞬间位置固定,不随时间推移;只有你拖动/resize
  窗口的瞬间它才"蹦"一下新位置
- **修复后**:橙色块按 `sin(t)` 连续左右滑动,**不需要任何窗口事件触发**

### 缺陷 B — MonitorLocal 内容不跟随 monitor 窗口

修复后的 `demo-app`(本次已改)把左上 cyan 色块 + FPS 条放进
`PUSH_SPACE(MonitorLocal) .. POP_SPACE` 区间,其余元素(中心十字、四个角
色块、动态橙色块)留在 World 空间。用 `demo-app --desktop-monitors 2`
启动至少 2 个 Desktop monitor 后拖到屏幕不同位置,盯两件事:

| 元素 | 空间 | 预期 |
|---|---|---|
| 中心十字、右上黄/左下粉/右下白、动态橙色块 | World | 两个窗口是同一张全局画布的两个"视口",元素在画布上的位置共享 |
| 左上 cyan 色块 + FPS 条 | **MonitorLocal** | **两个窗口各自的客户区左上角独立出现**,与窗口屏幕位置无关 |

- **修复前**:cyan 色块 + FPS 条只在全局 (10,10) 附近那一张 surface 上,
  大概率一个 monitor 都看不到,或者只在某一个 monitor 的角落勉强出现
- **修复后**:两个 monitor 的左上角都各自**独立地**显示这对元素

---

## Release 安装 smoke（发布前）

Release 包不发布 `demo-app`、`demo-consumer` 或诊断/实验程序；demo 只用于开发验证。发布前至少跑一次安装路径，确认普通用户入口不是手动启动 monitor。

### 构建 staging

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "scripts/build-release.ps1" `
  -Configuration Release `
  -Platform x64 `
  -Version 0.1.1 `
  -SignMode Dev
```

确认 `dist\overlay-engine-0.1.1-x64\app` 中只有正式组件，不能出现 `demo-*.exe`、`spike-*.exe` 或 `diag-*.exe`。

### Core + Desktop monitor

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "dist/overlay-engine-0.1.1-x64/scripts/install.ps1" `
  -Release `
  -SourceDir "dist/overlay-engine-0.1.1-x64" `
  -Components Core,DesktopMonitor `
  -CreateDesktopShortcut `
  -CreateStartMenu
```

验证点：

1. 从开始菜单或桌面快捷方式启动 `Start overlay-engine`。
2. `core-server.exe` 从安装目录读取 `config.ini`。
3. 只启动 Core 时不会出现 `desktop-window-monitor.exe` 或 Desktop 窗口。
4. App 请求 Desktop monitor 后才启动 `desktop-window-monitor.exe`，App 退出后 Desktop monitor 自动关闭。
5. 卸载后 `core-server.exe`、`desktop-window-monitor.exe`、快捷方式、开始菜单目录和安装目录都被删除。

### Core + Game Bar widget

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "dist/overlay-engine-0.1.1-x64/scripts/install.ps1" `
  -Release `
  -SourceDir "dist/overlay-engine-0.1.1-x64" `
  -Components Core,GameBarWidget `
  -CreateStartMenu
```

验证点：

1. 按 `Win+G` 打开 Xbox Game Bar。
2. 小组件列表里能看到并打开 `Overlay Widget`。
3. Game Bar widget 能连接已启动的 core-server。
4. 卸载时 MSIX 被移除；如果是内部 dev cert 测试包，只有显式传 `-RemoveCert` 才删除导入的证书。

### AutoStart 与完整卸载

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "dist/overlay-engine-0.1.1-x64/scripts/install.ps1" `
  -Release `
  -SourceDir "dist/overlay-engine-0.1.1-x64" `
  -Components Core,DesktopMonitor,GameBarWidget `
  -AutoStart `
  -CreateDesktopShortcut `
  -CreateStartMenu
```

验证点：

1. 当前用户计划任务 `overlay-engine Core` 存在。
2. 注销/重新登录后只自动启动 core-server，工作目录仍是安装目录。
3. `config.ini` 生效，Core 能报告 Desktop/Game Bar monitor 能力，但不会自动拉起 Desktop monitor。
4. 执行卸载后计划任务、快捷方式、开始菜单、MSIX、安装目录和卸载注册表项都被清理。

---

## 启动步骤

### 前置

先确认构建通过:

```powershell
cargo build --workspace
```

### 终端 1 — Core 服务

```powershell
cargo run -p core-server --bin core-server
```

等它打印:

```
Core Server listening on \\.\pipe\overlay-core
```

### 终端 2 — demo-app 请求两个 Desktop monitor

```powershell
cargo run -p core-server --bin demo-app -- --desktop-monitors 2 --window-mode bordered
```

demo-app 会:

1. 打印屏幕分辨率、已连接、已创建画布。
2. 打印 Core 返回的 Desktop/Game Bar monitor catalog。
3. 通过 `StartMonitor` 请求两个 Desktop monitor，并等待 `StartMonitorResult` 返回实际 monitor id。
4. 之后两个 Desktop monitor 窗口应该开始显示内容。

可选窗口模式验证：

```powershell
cargo run -p core-server --bin demo-app -- --desktop-monitors 1 --window-mode borderless
cargo run -p core-server --bin demo-app -- --desktop-monitors 1 --window-mode fullscreen
cargo run -p core-server --bin demo-app -- --desktop-monitors 1 --click-through
```

### 观察清单

| # | 观察点 | 缺陷 A 现象(修复前) | 缺陷 A 预期(修复后) |
|---|---|---|---|
| 1 | 屏幕中心十字和动态橙色块 | 画面卡住 | 橙色块持续 `sin(t)` 滑动 |
| 2 | 完全不碰任何窗口 10 秒 | 画面冻结 | 画面连续推进 |
| 3 | 拖动任一窗口一下 | 画面蹦一帧后又卡住 | 画面本来就在动,拖动无影响 |

| # | 观察点 | 缺陷 B 现象(修复前) | 缺陷 B 预期(修复后) |
|---|---|---|---|
| 4 | monitor A 窗口左上角 | 看不到 cyan 色块/FPS 条 | **能看到 cyan 色块 + FPS 条** |
| 5 | monitor B 窗口左上角 | 看不到 cyan 色块/FPS 条 | **独立能看到 cyan 色块 + FPS 条** |
| 6 | 右上黄色块(World) | 两窗口按各自 viewport 看到同一画布位置 | 同左(World 语义不变) |
| 7 | 中心十字(World) | 同上 | 同左(World 语义不变) |

如果 #4 和 #5 同时满足 → 缺陷 B 修复确认。
如果 #1 和 #2 满足 → 缺陷 A 修复确认。

---

## 如果还是"啥也没修复"

### Case 1: 画面仍然冻结(缺陷 A 还在)

最可能原因是 DWM 没有按预期 retire `IPresentationBuffer`,导致
`acquire_available_buffer` 拿不到空闲 buffer。Core 终端窗口会开始打印:

```
SubmitFrame: canvas=1 frame=XXX dropped — all 2 buffers busy after 16ms
```

如果看到大量这种日志,说明轮转握手没在你环境里生效。
可以试:

- 把 `BUFFER_COUNT` 从 2 调到 3:编辑 `core-server/src/renderer/dcomp.rs`
  第一个 `pub const BUFFER_COUNT: usize = 2;` 改成 `3`,重新编译
- 关闭全屏游戏模式 / 硬件加速 GPU 计划程序,避免 DWM 进入
  independent-flip(这种状态下 buffer 轮转语义更严)

### Case 2: cyan 色块/FPS 条一个窗口都看不到(缺陷 B 还在)

可能原因:

1. 你启动的是旧版 monitor 二进制(没走到任务 3.5 的双 visual 挂载)。
   确认 `cargo build -p desktop-window-monitor` 出的 monitor 是最新的
2. Core 终端日志里应该有 `mounted dual visual tree (World + MonitorLocal)`
   这行 —— 没有说明 monitor 没收到 `MonitorLocalSurfaceAttached`
3. `attach_monitor` 里 `PerMonitorResources::new` 失败了,会打印
   `MonitorLocal surface not created` —— 查看 Core 终端窗口

### Case 3: monitor 收不到任何帧(两个问题都在)

- 检查 demo-app 打印的 monitor catalog 是否包含可用的 Desktop Window Monitor。
- 检查 `StartMonitorResult` 是否为 `Ok`，并返回了实际 monitor id。
- 确认启动顺序是 Core → app；Desktop monitor 应由 app 的 `StartMonitor` 请求拉起。

---

## 自动化测试 vs 端到端测试的差距(诚实交代)

| 测试层面 | 覆盖 | 不覆盖 |
|---|---|---|
| `cargo test -p core-server --test preservation` | 协议编解码 bit-identical、decoder 8 opcode 行为不变、`ServerState` 多 monitor 生命周期 | 真实 D3D11/DComp/DWM 合成,真实 IPC |
| `cargo test -p core-server --test bug_condition_exploration` | `CanvasResources.buffers.len() >= 2` 结构、decoder 接受 PUSH_SPACE、软件模拟每个 monitor (10,10) 应该变绿 | DWM 是否真 retire buffer,monitor 窗口真实像素是否推进 |
| `cargo test -p core-server --lib server_task` | 空间栈 pre-scan、rolling-duration 窗口 | 真实 GPU 命令提交 |
| **本文档端到端测试** | DWM + DComp + IPC + 双 monitor + 动态 app | — |

端到端这一层在自动化里非常难做(需要桌面环境、DWM 合成时间、可视化比对),
所以交给人工眼睛验证是合理分工 —— 但这意味着"测试都过了"不等于
"启动后就能肉眼看到效果",必须按本文档跑一次。
