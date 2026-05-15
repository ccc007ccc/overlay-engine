# Release 构建与安装器

本文档面向维护者，说明如何生成 overlay-engine release staging 和图形安装器。

## 前置环境

- Rust toolchain
- Visual Studio / MSBuild，包含 UWP/MSIX workload
- Windows SDK `signtool.exe`
- 可选：Inno Setup 6，用于生成 `Setup.exe`
- 正式 release 推荐使用可信代码签名证书；内部测试可用 `-SignMode Dev`

## 生成 release staging

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "scripts/build-release.ps1" `
  -Configuration Release `
  -Platform x64 `
  -Version 0.1.1 `
  -SignMode Dev
```

输出目录：

```text
dist\overlay-engine-0.1.1-x64\
  app\
    core-server.exe
    desktop-window-monitor.exe
    renderer.dll
  widget\
    OverlayWidget_<version>_x64.msix
    Dependencies\x64\*.appx
  scripts\
    install.ps1
    uninstall.ps1
    game-bar-widget-install.ps1
  manifest.json
```

构建脚本使用 allowlist，只复制正式组件。以下文件不得进入 staging：

- `demo-app.exe`
- `demo-consumer.exe`
- `desktop-demo-producer.exe`
- `demo-producer.exe`
- `spike-*.exe`
- `diag-*.exe`

## 生成图形安装器

安装 Inno Setup 后运行：

```powershell
iscc.exe "installer/overlay-engine.iss"
```

输出：

```text
dist\overlay-engine-0.1.1-x64-Setup.exe
```

安装器提供这些组件/任务：

- Core Server：必选
- Desktop Window Monitor：可选
- Xbox Game Bar Widget：可选
- 开机自启：可选
- 桌面快捷方式：可选
- 开始菜单目录：可选

## 直接安装 staging

不生成 Inno 安装器时，可以直接运行 PowerShell 后端：

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

## 卸载验证

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\overlay-engine\scripts\uninstall.ps1" `
  -Release `
  -InstallDir "$env:LOCALAPPDATA\Programs\overlay-engine" `
  -RemoveWidget
```

验证卸载后：

- `core-server.exe` 和 `desktop-window-monitor.exe` 已停止。
- 计划任务 `overlay-engine Core` 已删除。
- 桌面和开始菜单快捷方式已删除。
- Game Bar MSIX 已卸载。
- 安装目录已删除。

## Smoke test

1. 安装 Core + Desktop Window Monitor 后，从开始菜单启动 `Start overlay-engine`。
2. 确认 core-server 读取安装目录下的 `config.ini`，且不会自动拉起 `desktop-window-monitor.exe`。
3. 用 app 发送 `ListMonitorTypes` 能看到 Desktop Window Monitor 能力；发送 `StartMonitor` 后才出现 Desktop 窗口，app 退出后窗口关闭。
4. 安装 Game Bar widget 后，按 `Win+G` 打开 Xbox Game Bar，确认能打开 `Overlay Widget`；Core 不负责唤起或关闭它。
5. 选择开机自启后，注销/重新登录，确认 `overlay-engine Core` 计划任务触发并只启动 core-server。
6. 卸载后确认没有残留计划任务、快捷方式、MSIX 和安装目录。
