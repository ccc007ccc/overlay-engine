# spike: DComp 跨进程 surface 共享

v1.0 server-mode 的第一个技术 spike（按 `docs/v1.0-server-bootstrap.md` §9.2）。
验证 producer 进程把渲染内容通过 NT handle 跨进程共享给 consumer 进程的可行性。

## 关键发现（更正 bootstrap doc §4.1 的描述）

bootstrap 原文："Core 端创建 IDXGISwapChain... DCompositionCreateSurfaceHandle... DuplicateHandle"
—— 把 swap chain 和 surface handle 当成关联对象，**不准确**。

实际正确链路：

```
Producer (Core 进程)：
  ┌────────────────────────────────────────────────────────────┐
  │ D3D11CreateDevice(BGRA_SUPPORT)                            │
  │   → CreatePresentationFactory(d3d) → IPresentationFactory  │
  │   → factory.CreatePresentationManager → IPresentationMgr   │
  │ DCompositionCreateSurfaceHandle(ALL_ACCESS, NULL)          │
  │   → NT HANDLE                                              │
  │ manager.CreatePresentationSurface(handle)                  │
  │   → IPresentationSurface（写一端，绑到 handle）            │
  │ d3d.CreateTexture2D(BGRA8, RT, NTHANDLE | KEYEDMUTEX)      │
  │   → ID3D11Texture2D（必须带这两个 flag，否则 E_INVALIDARG） │
  │ manager.AddBufferFromResource(texture)                     │
  │   → IPresentationBuffer                                    │
  │ keyed_mutex.AcquireSync(0)                                 │
  │ d3d.ClearRenderTargetView(rtv, color)                      │
  │ keyed_mutex.ReleaseSync(0)                                 │
  │ surface.SetBuffer(buffer)                                  │
  │ manager.Present()                                          │
  └────────────────────────────────────────────────────────────┘
              │
              │ DuplicateHandle(handle, consumer_pid)  ← 只传 NT HANDLE 即可
              ▼
Consumer (Widget 进程)：
  ┌────────────────────────────────────────────────────────────┐
  │ DCompositionCreateDevice2 → IDCompositionDesktopDevice     │
  │ dcomp.CreateSurfaceFromHandle(handle)                      │
  │   → IUnknown（**wrapper**，不是 IDCompositionSurface！）   │
  │ visual.SetContent(wrapper IUnknown)  ← 直接传，不 cast      │
  │ target.SetRoot(visual); dcomp.Commit()                     │
  └────────────────────────────────────────────────────────────┘
```

**关键陷阱**：
1. `IDCompositionSurface::BeginDraw` 只能在同进程 D2D 路径用；跨进程 producer 写内容**必须**走 CompositionSwapchain (`IPresentationManager`)。
2. `dcomp.CreateSurfaceFromHandle` 返回的 `IUnknown` **不实现** `IDCompositionSurface`（`E_NOINTERFACE`）。它是个 wrapper，只能透传给 `visual.SetContent`。
3. `AddBufferFromResource` 接受的 D3D11 资源**必须**带 `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`，否则 `E_INVALIDARG`。
4. KEYED_MUTEX 同步：producer 写之前 `AcquireSync(0)`，写完 `ReleaseSync(0)`；compositor 端由 PresentationManager 内部 sync，不需要 widget 显式 acquire。

## 跑这个 spike

```powershell
cargo build --release -p spike-dcomp-xproc

# Terminal 1
.\target\release\spike-producer.exe

# Terminal 2
.\target\release\spike-consumer.exe
```

预期：consumer 弹出 300×300 标准 Win32 窗口，左上 256×256 区域显示**红色**（producer 渲染的 ClearRenderTargetView 内容），剩余区域系统默认背景。窗口 5 秒后自动关闭。

## 现已验证 ✅

- `DCompositionCreateSurfaceHandle` + `DuplicateHandle` 跨进程传递 OK
- `IPresentationManager::CreatePresentationSurface` 绑定 NT handle OK
- `dcomp.CreateSurfaceFromHandle + visual.SetContent` 在普通 Win32 进程 OK
- IPC 协议 (named pipe, 4 字节 PID + 8 字节 HANDLE) OK
- 端到端：producer 红色 ClearRTV → consumer visual tree 显示 OK

## 还没验证 ❌（v1.0 第二个 spike）

- Consumer 换成 **UWP widget**（AppContainer 沙盒）能否同样 work
- `Windows.UI.Composition.Compositor` (WinRT) 通过 `ICompositorInterop::CreateCompositionSurfaceForHandle`
  接收 NT handle 是否走通
- AppContainer 进程接收来自非 AppContainer producer 的 DuplicateHandle 是否需要特殊 ACL
- ThreadPoolTimer 后台线程 + DComp commit 是否仍 modal-safe

下一步：把 spike consumer 改成 widget plugin，验证完整 UWP 路径。
