# overlay-engine

overlay-engine 是 Windows overlay 渲染栈，由一个必选的 Core Server 和可选 Monitor 组成。

## Release 组件

- **Core Server**：必选，负责 IPC、渲染和 surface 分发。
- **Desktop Window Monitor**：可选，普通 Win32 窗口 Monitor。
- **Xbox Game Bar Widget**：可选，Xbox Game Bar 小组件。

`demo-app`、`demo-consumer` 和诊断/实验程序不属于 release 包。

## Monitor 生命周期

- 后台常驻进程只有 `core-server.exe`。
- Desktop Window Monitor 安装后作为可用能力写入 `config.ini`，不会随 Core 自启；App 通过 Core IPC 查询能力并按需启动窗口数量，App 断开后 Core 会关闭这些 Desktop monitor。
- Xbox Game Bar Widget 只能由用户按 `Win+G` 手动打开；Core 只暴露它的可用性和单实例限制，不负责唤起或关闭。

## 安装

正式发布建议使用 `overlay-engine-<version>-x64-Setup.exe` 图形安装器。安装器支持：

- Core Server 必选安装。
- Desktop Window Monitor 可选安装。
- Xbox Game Bar Widget 可选安装。
- 可选开机自启。
- 可选桌面快捷方式。
- 可选开始菜单目录。

安装流程会写入当前用户注册表 `HKCU:\Software\overlay-engine\Core`：

- `InstallDir`：Core 安装目录。
- `CoreExe`：`core-server.exe` 的完整路径。
- `Version`：安装版本。

App 连接 `\\.\pipe\overlay-core` 失败且 pipe 不存在时，会读取这些注册表值并隐藏唤起已安装的 `core-server.exe`；如果 Core 已经在线，则直接连接，不重复启动。

默认安装到：

```text
%LocalAppData%\Programs\overlay-engine
```

安装 Xbox Game Bar Widget 后，按 `Win+G` 打开 Xbox Game Bar，在小组件列表中打开并 pin `Overlay Widget`。内部 dev cert 测试包会在信任证书步骤触发 UAC；管理员权限只用于导入证书，MSIX 仍安装到当前用户。

## PowerShell 安装 staging

维护者或内部测试可以直接安装 release staging：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "dist/overlay-engine-0.1.3-x64/scripts/install.ps1" `
  -Release `
  -SourceDir "dist/overlay-engine-0.1.3-x64" `
  -InstallDir "$env:LOCALAPPDATA\Programs\overlay-engine" `
  -Components Core,DesktopMonitor,GameBarWidget `
  -AutoStart `
  -CreateDesktopShortcut `
  -CreateStartMenu
```

只安装 Core + Desktop Window Monitor：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "dist/overlay-engine-0.1.3-x64/scripts/install.ps1" `
  -Release `
  -SourceDir "dist/overlay-engine-0.1.3-x64" `
  -Components Core,DesktopMonitor
```

## 卸载

优先使用 Windows “设置 → 应用”里的 overlay-engine 卸载项，或开始菜单中的 `Uninstall overlay-engine`。

也可以手动执行：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\overlay-engine\scripts\uninstall.ps1" `
  -Release `
  -InstallDir "$env:LOCALAPPDATA\Programs\overlay-engine" `
  -RemoveWidget
```

卸载会清理安装器创建的自启项、快捷方式、Game Bar MSIX、Core 安装位置注册表和安装目录。默认保留用户数据；如需删除本地数据，添加 `-RemoveUserData`。

## 图标

Core、Desktop Window Monitor 和 Xbox Game Bar Widget 共用同一个窗口模板图标体系：Core 是白色主题和黑色齿轮，Desktop 是蓝色主题，Game Bar 是绿色主题。SVG 源文件位于 `assets/icons/`，release 构建会通过 `scripts/generate-icons.py` 生成 Windows `.ico` 和 Game Bar MSIX PNG assets。

## 构建 release

维护者构建流程见 [`docs/release-build.md`](docs/release-build.md)。

## 架构文档

- [`docs/rendering-architecture.md`](docs/rendering-architecture.md)：当前 Core-side 渲染主线、瓶颈和长期路线。
- [`docs/multi-app-compositor-plan.md`](docs/multi-app-compositor-plan.md)：多 App 单 Monitor 与 Xbox Game Bar 单窗口 compositor 计划。
