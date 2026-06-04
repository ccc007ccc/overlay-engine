# Release 构建与安装器

本文档面向维护者，说明如何生成 overlay-engine release staging 和图形安装器。

## 前置环境

- Rust toolchain
- Python 3 + Pillow，用于从 `assets/icons/` 设计源生成 `.ico` 和 MSIX PNG assets
- Visual Studio / MSBuild，包含 UWP/MSIX workload
- Windows SDK `signtool.exe`
- 可选：Inno Setup 6，用于生成 `Setup.exe`
- 正式 release 推荐使用可信代码签名证书；内部测试可用 `-SignMode Dev`

## 图标资产

图标源文件位于 `assets/icons/`：Core 使用白色窗口 + 黑色齿轮，Desktop Window Monitor 使用蓝色窗口，Xbox Game Bar Widget 使用绿色窗口 + Xbox 风格中心符号。`scripts/build-release.ps1` 会先运行 `scripts/generate-icons.py`，生成：

- `core-server/resources/overlay-core.ico`
- `monitors/desktop-window/resources/overlay-desktop-monitor.ico`
- `monitors/game-bar-widget/Assets/*.png`

Core 和 Desktop 的 `.ico` 会通过 Windows resource 嵌入 exe；Game Bar PNG assets 会进入 MSIX。

## 生成 release staging

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "scripts/build-release.ps1" `
  -Configuration Release `
  -Platform x64 `
  -Version 0.1.3 `
  -SignMode Dev
```

输出目录：

```text
dist\overlay-engine-0.1.3-x64\
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

安装 Inno Setup 后运行。发布时显式传入版本和 staging 目录，避免安装器误读旧 payload：

```powershell
iscc.exe /DAppVersion=0.1.3 /DStageDir="..\dist\overlay-engine-0.1.3-x64" "installer/overlay-engine.iss"
```

输出：

```text
dist\overlay-engine-0.1.3-x64-Setup.exe
```

安装器提供这些组件/任务：

- Core Server：必选
- Desktop Window Monitor：可选
- Xbox Game Bar Widget：可选
- 开机自启：可选
- 桌面快捷方式：可选
- 开始菜单目录：可选

安装器长期职责边界：Inno 负责文件复制、快捷方式和 Windows 卸载入口；PowerShell 后端负责生成运行脚本、写 Core 定位注册表、写自启项、安装 Widget MSIX，并写 `install-state.json`。Inno 调用后端时传入 `-InstallHost Inno`，避免重复创建快捷方式或重复注册卸载项。

## 直接安装 staging

不生成 Inno 安装器时，可以直接运行 PowerShell 后端：

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

## 安装写入项

release 安装会写入当前用户范围的 Core 定位信息，供 App 在 Core 未运行时主动唤起已安装 Core：

```text
HKCU:\Software\overlay-engine\Core
  InstallDir = <安装目录>
  CoreExe    = <安装目录>\core-server.exe
  Version    = <安装版本>
```

开机自启仍使用当前用户 `Run` 项，但命令经 `powershell.exe -WindowStyle Hidden` 调用安装目录下的 `Start-overlay-engine.ps1`，避免登录时显示 console 窗口。`Start-overlay-engine.ps1` 会检查同一安装目录下的 `core-server.exe` 是否已经运行，已运行时不会重复启动。`Stop-overlay-engine.ps1` 只停止安装目录内的 Core/Desktop monitor 进程，避免误杀相邻目录或开发目录里的进程。

安装还会写 `install-state.json`，记录 `installHost`、shortcut/uninstall owner、MSIX `PackageFullName`、publisher、dev cert thumbprint 等卸载所需状态。卸载 Widget 时优先使用记录的 `PackageFullName` 精确移除，缺失时才回退到包名 `OverlayWidget`。内部 dev cert 包安装时，证书信任步骤会自动 UAC 提权；提权子进程只导入证书，`Add-AppxPackage` 仍在原当前用户上下文执行。

## 卸载验证

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\overlay-engine\scripts\uninstall.ps1" `
  -Release `
  -InstallDir "$env:LOCALAPPDATA\Programs\overlay-engine" `
  -RemoveWidget
```

验证卸载后：

- `core-server.exe` 和 `desktop-window-monitor.exe` 已停止。
- 当前用户 `Run` 自启项 `overlay-engine` 已删除。
- `HKCU:\Software\overlay-engine\Core` 已删除。
- 桌面和开始菜单快捷方式已删除。
- Game Bar MSIX 已卸载。
- 安装目录已删除。

## Smoke test

1. 安装 Core + Desktop Window Monitor 后，从开始菜单启动 `Start overlay-engine`。
2. 确认 core-server 读取安装目录下的 `config.ini`，且不会自动拉起 `desktop-window-monitor.exe`。
3. 用 app 发送 `ListMonitorTypes` 能看到 Desktop Window Monitor 能力；发送 `StartMonitor` 后才出现 Desktop 窗口，app 退出后窗口关闭。
4. 安装 Game Bar widget 后，按 `Win+G` 打开 Xbox Game Bar，确认能打开 `Overlay Widget`；Core 不负责唤起或关闭它。
5. 停止 Core 后运行 app，确认 app 可通过 `HKCU:\Software\overlay-engine\Core` 中的 `CoreExe` 隐藏唤起已安装 Core，并连接 `\\.\pipe\overlay-core`。
6. 选择开机自启后，注销/重新登录，确认当前用户 `Run` 自启项触发并隐藏启动 core-server，不出现控制台窗口。
7. 卸载后确认没有残留自启项、Core 定位注册表、快捷方式、MSIX 和安装目录。
