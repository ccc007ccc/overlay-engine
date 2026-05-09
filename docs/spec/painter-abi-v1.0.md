# Overlay Engine v1.0 Architecture & ABI Specification

**Status**: DRAFT  
**Date**: May 2026  

v1.0 marks a complete paradigm shift for the Overlay Engine: moving from an in-process rendering DLL to a **standalone OS process (Core)** using **DirectComposition (DComp) Cross-Process Surface Sharing**.

This enables multiple consumers (e.g., Xbox Game Bar widgets, desktop windows) to display canvases created and managed by arbitrary external producer applications, without either side paying the overhead of CPU readbacks or inter-process pixel copies.

---

## 1. Core Architecture

### 1.1 The Triad Model

The system consists of three distinct actors:

1. **Core Process (Server)**:
   - A standalone OS process holding the `ID3D11Device` and `IDCompositionDesktopDevice`.
   - Owns the Direct2D rendering context and maintains the state machine.
   - **Is mechanism, not policy**: It routes commands to canvases and distributes surface handles to consumers, but does not decide what gets displayed where.
2. **Producers (Clients)**:
   - The application(s) that want to draw overlays.
   - **Owns the canvas**: They dictate the logical size, render resolution, and coordinate spaces.
   - Connect to Core via IPC, send rendering commands via a shared-memory ringbuffer.
3. **Consumers (Monitors)**:
   - Display hosts like the Game Bar widget (`monitors/game-bar-widget`) or a standard window (`monitors/desktop-window`).
   - **Completely passive**: They register themselves with Core and wait to be handed an NT handle (`HANDLE`) to an `IDCompositionSurface`.
   - They attach this handle to their local visual tree. They only report their window physical geometry back to Core.

### 1.2 IPC and Data Transport

To achieve 60fps+ rendering with low latency, communication is split:

- **Control Plane**: Named pipe (`\.\pipe\overlay-core`) for low-frequency events (connect, canvas creation, surface handle duplication, consumer attachment).
- **Data Plane**: Shared Memory (Memory Mapped Files) for high-frequency data.
  - **Command Ringbuffer**: Producers encode drawing commands (opcodes + structs) into a shared buffer. IPC is only used to send a signal ("render frame N up to offset X").
  - **Bitmap Resources**: Large pixel blobs (images, video frames) are written to shared memory.

---

## 2. Canvas and Viewport Semantics

Unlike v0.7 where the canvas size implicitly matched the monitor size, v1.0 strictly decouples them to support game-like arbitrary resolutions and correct "observation window" panning.

### 2.1 Dimensions

A canvas has two distinct sizes:
- **Logical Size** (e.g., `2560x1440`): The coordinate system the Producer draws in. Usually matches the target game's or physical screen's resolution.
- **Render Resolution** (e.g., `1280x720`): The actual size of the D3D11 texture backing the DComp surface. Lowering this increases performance/reduces VRAM at the cost of blurriness, but *does not* affect the scale or placement of drawn elements.

### 2.2 Coordinate Spaces (World vs MonitorLocal)

A single canvas can contain elements that behave differently when the Consumer window moves. Producers wrap rendering commands in space blocks:

- **World Space** (Default): 
  - Bound to the Logical Canvas. 
  - When the Consumer moves its window, the viewport shifts, acting as a camera looking at a fixed canvas.
  - *Use case*: Crosshairs, minimaps, ESP boxes bound to game world coordinates.
- **MonitorLocal Space**:
  - Bound to the Consumer's physical window client area.
  - 0,0 is always the top-left of the Consumer window, regardless of where the window is on the screen.
  - *Use case*: FPS counters, status badges, widget borders.

*Core handles this automatically*: When rendering a canvas for Consumer A, Core applies a transform matrix (incorporating scaling and Consumer A's negative viewport offset) for World commands, and an identity-like transform for MonitorLocal commands.

---

## 3. Rendering ABI (Command Ringbuffer)

### 3.1 Serialization Format

Commands are serialized into the shared memory ringbuffer sequentially. All numbers are Little-Endian.

Every command follows this header:
```c
struct CmdHeader {
    uint16_t opcode;
    uint16_t payload_len;
}
```

### 3.2 Opcodes

*This maps the v0.7 P/Invoke commands to wire format.*

**State & Flow**
- `0x0001` PUSH_SPACE (uint8 space_type)
- `0x0002` POP_SPACE
- `0x0003` SET_TRANSFORM (float m11, m12, m21, m22, m31, m32)
- `0x0004` RESET_TRANSFORM
- `0x0005` PUSH_CLIP_RECT (float x, y, w, h)
- `0x0006` POP_CLIP

**Primitives**
- `0x0101` CLEAR (float r, g, b, a)
- `0x0102` FILL_RECT (float x, y, w, h, float r, g, b, a)
- `0x0103` STROKE_RECT (float x, y, w, h, float stroke_w, float r, g, b, a)
- `0x0104` FILL_ROUNDED_RECT (float x, y, w, h, float rx, ry, float r, g, b, a)
- `0x0105` STROKE_ROUNDED_RECT (float x, y, w, h, float rx, ry, float stroke_w, float r, g, b, a)
- `0x0106` FILL_ELLIPSE (float cx, cy, rx, ry, float r, g, b, a)
- `0x0107` STROKE_ELLIPSE (float cx, cy, rx, ry, float stroke_w, float r, g, b, a)
- `0x0108` DRAW_LINE (float x0, y0, x1, y1, float stroke_w, float r, g, b, a, int32 dash_style)

**Complex Geometry**
- `0x0201` DRAW_POLYLINE (uint32 count, float stroke_w, float r, g, b, a, uint8 closed, [float x, y]...)
- `0x0202` FILL_PATH (uint32 bytes_len, float r, g, b, a, [byte]...)
- `0x0203` STROKE_PATH (uint32 bytes_len, float stroke_w, float r, g, b, a, int32 dash_style, [byte]...)

**Gradients**
- `0x0301` FILL_RECT_GRADIENT_LINEAR (float x, y, w, h, float sx, sy, ex, ey, uint32 stop_count, [float offset, r, g, b, a]...)
- `0x0302` FILL_RECT_GRADIENT_RADIAL (float x, y, w, h, float cx, cy, rx, ry, uint32 stop_count, [float offset, r, g, b, a]...)

**Resources & Text**
- `0x0401` DRAW_TEXT (float x, y, float size, float r, g, b, a, uint32 text_len, [utf8]...)
- `0x0402` DRAW_BITMAP (uint32 handle, float sx, sy, sw, sh, float dx, dy, dw, dh, float opacity, int32 interp_mode)

---

## 4. DComp Surface Sharing Mechanics (Implementation Detail)

Based on the v1.0 spike, the correct cross-process DirectComposition pipeline is:

1. **Producer Side**:
   - `CreatePresentationFactory` -> `IPresentationManager`
   - `DCompositionCreateSurfaceHandle` -> `HANDLE`
   - `manager.CreatePresentationSurface(HANDLE)` -> `IPresentationSurface`
   - `CreateTexture2D` with `D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_DISPLAYABLE`
   - `manager.AddBufferFromResource(texture)`
   - Rendering: Draw to texture -> `d3d_ctx.Flush()` -> `surface.SetBuffer()` -> `manager.Present()`

2. **IPC**:
   - Call `DuplicateHandle` on the NT HANDLE targeting the Consumer's PID.

3. **Consumer Side**:
   - `dcomp_device.CreateSurfaceFromHandle(dup_handle)` -> Wrapper `IUnknown`
   - *CRITICAL*: Do not cast to `IDCompositionSurface` (yields `E_NOINTERFACE`).
   - Pass wrapper directly to `visual.SetContent()`.
   - Manage viewport mappings via `visual.SetTransform()`.

