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

默认安装到：

```text
%LocalAppData%\Programs\overlay-engine
```

安装 Xbox Game Bar Widget 后，按 `Win+G` 打开 Xbox Game Bar，在小组件列表中打开并 pin `Overlay Widget`。

## PowerShell 安装 staging

维护者或内部测试可以直接安装 release staging：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "dist/overlay-engine-0.1.1-x64/scripts/install.ps1" `
  -Release `
  -SourceDir "dist/overlay-engine-0.1.1-x64" `
  -InstallDir "$env:LOCALAPPDATA\Programs\overlay-engine" `
  -Components Core,DesktopMonitor,GameBarWidget `
  -AutoStart `
  -CreateDesktopShortcut `
  -CreateStartMenu
```

只安装 Core + Desktop Window Monitor：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "dist/overlay-engine-0.1.1-x64/scripts/install.ps1" `
  -Release `
  -SourceDir "dist/overlay-engine-0.1.1-x64" `
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

卸载会清理安装器创建的计划任务、快捷方式、Game Bar MSIX 和安装目录。默认保留用户数据；如需删除本地数据，添加 `-RemoveUserData`。

## 构建 release

维护者构建流程见 [`docs/release-build.md`](docs/release-build.md)。
