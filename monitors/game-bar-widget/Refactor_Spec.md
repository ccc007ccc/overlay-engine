# Xbox Game Bar Monitor Refactoring Spec

## 1. Overview
The Xbox Game Bar widget (`monitors/game-bar-widget`) needs to be fully updated to align with the new `core-server` IPC architecture (Task 3.3/3.4 of the `animation-and-viewport-fix` spec). 

Currently, the Game Bar widget connects to `\\.\pipe\overlay-core`, sends a registration message, reads exactly one `CanvasAttached` message, and then stops listening on the pipe. It uses `ICompositorInterop::CreateCompositionSurfaceForHandle` to mount the DComp surface, which is fundamentally the correct approach for UWP, but the IPC and lifecycle management are severely outdated.

## 2. Goals
*   **Continuous IPC Loop**: Maintain a continuous background loop to receive incoming messages from the Core Server (`AppDetached`, `MonitorLocalSurfaceAttached`, etc.) instead of reading just once.
*   **MonitorLocal Surface Support**: Implement support for the new `MonitorLocal` surface. When `MonitorLocalSurfaceAttached` is received, create a second `SpriteVisual` and layer it on top of the World `SpriteVisual`.
*   **Clean Separation**: Remove legacy in-process rendering fallback paths (`RendererPInvoke.cs`, `CompositionPump.cs` with ThreadPoolTimer). The Monitor must act *only* as a thin client for the Core Server's surfaces.

## 3. Architecture & Implementation Plan

### 3.1 IPC Communication Loop (`ExternalSurfacePump.cs`)
*   Change the `TryConnect` mechanism from a blocking one-shot read to spawning an async `Task` or continuous thread that reads the pipe.
*   **Message Decoding**: Implement a robust protocol decoder matching `core-server/src/ipc/protocol.rs`:
    *   `Opcode 0x0002`: `RegisterMonitor` (Sent to server)
    *   `Opcode 0x0005`: `CanvasAttached` (Received from server - World Surface)
    *   `Opcode 0x0006`: `AppDetached` (Received from server - Cleanup needed)
    *   `Opcode 0x0007`: `MonitorLocalSurfaceAttached` (Received from server - Per-monitor Surface)

### 3.2 Visual Tree Construction (WinUI Composition)
Currently, `ExternalSurfacePump` mounts one `SpriteVisual` directly to the `_hostElement`. 
To support `MonitorLocal`, the visual tree should be updated to a layered approach:
```text
HostElement (FrameworkElement)
 └── ContainerVisual (Root)
      ├── SpriteVisual (World Surface)
      └── SpriteVisual (MonitorLocal Surface)
```
*   **World Surface**: Offset inversely to the widget's screen coordinates (so it acts as a viewport into the global canvas).
*   **MonitorLocal Surface**: Fixed at `Offset = (0, 0)` so it anchors perfectly to the widget's top-left corner, preserving per-monitor overlays (like FPS counters).

### 3.3 Lifecycle and UWP Suspend/Resume
Game Bar widgets can be pinned or unpinned. When the overlay menu is closed and the widget is unpinned, it might be suspended.
*   **Disconnect**: If the pipe breaks or `AppDetached` is received, destroy the `ICompositionSurface` and Visuals, and attempt reconnection.
*   **Visibility**: When the widget's `VisibilityChanged` fires (e.g., hidden), pause unnecessary polling (like window rect updates), but keep the pipe connected to listen for `CanvasAttached`.

### 3.4 Cleanup
*   Delete or deprecate `RendererPInvoke.cs`, `CompositionPump.cs`, and `clrcompression.dll` dependencies. The Game Bar widget should no longer host any Rust D3D/D2D engine code directly; it should only depend on the OS APIs (`kernel32.dll`, `Windows.UI.Composition`).

## 4. Risks & Considerations
*   **UWP Sandbox**: The named pipe `\\.\pipe\overlay-core` currently requires no special ACLs because desktop apps run as the same user. The Game Bar widget is a UWP app container. We must ensure the `overlay-core` pipe is created by the Server with an SDDL that allows `ALL APPLICATION PACKAGES` (or the specific UWP capability) to read/write, otherwise the connection will yield `Access Denied`. (Need to check if the current C# code successfully connects in testing).
*   **Window Bounds Tracking**: The Game Bar widget is an overlay that can be moved. The `ExternalSurfacePump.UpdateVisualTransform` currently polls `ScreenInterop.TryGetWindowScreenRect` to offset the World surface. This polling must be efficient and tied to the `CompositionTarget.Rendering` event to prevent jitter.