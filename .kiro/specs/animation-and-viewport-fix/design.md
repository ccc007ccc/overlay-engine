# Animation and Viewport Fix — Bugfix Design

## Overview

`v1.0-server` 架构在接入 `desktop-window` 与 Game Bar widget 两类 Consumer 后,暴露出两个相互独立但 root cause 都落在 Core 共享 surface 管线上的渲染缺陷:

- **缺陷 A (动画停滞)**:Producer 稳定以 ~120Hz 调 `SubmitFrame`,Core 也确实走到 `Present`,但 Consumer 窗口内画面停留在旧帧,仅当窗口事件触发 DWM 重合成时才短暂推进。症状对应"一张 shared DComp surface 在单 buffer 状态下被 DWM 长期持有,新帧 `SetBuffer` + `Present` 没有真正交付新内容"的经典 pipeline 饿死模式。
- **缺陷 B (窗口局部 UI 不跟随 Consumer)**:Producer 只能在一个**所有 Consumer 共享的全局 surface**上绘制,(10,10) 写出去的 FPS 条就固定落在该 surface 的 (10,10),与 Consumer 窗口位置无关;命令协议也缺少"切换到 per-Consumer 局部空间"的原语。

修复策略(高层):

1. **针对缺陷 A** — 在 `CanvasResources` 中引入多 buffer(≥2) 轮转 + 正确的 buffer-available 等待/调度,让 Producer 每次 `SubmitFrame` 都写到一个"当前不被 DWM 占用"的 buffer,`Present` 之后该 buffer 真正成为新的显示内容。保持"Producer 提交速率远高于刷新率时允许丢中间帧,但不得完全冻结"。
2. **针对缺陷 B** — 把"一张 Canvas 对应一张全局共享 surface"的渲染模型拆成**两层空间**:
   - **World 空间**(沿用现有语义):Producer 绘制到 Canvas 的全局 surface,所有 Consumer 按自己的 viewport 透视同一张世界画布,现有未感知空间概念的 Producer 仍按此解释。
   - **MonitorLocal 空间**(新增):Producer 发 `PUSH_SPACE(MonitorLocal)` / `POP_SPACE` 原语后,Core 把这段命令**在每个 attach 的 Consumer 自己的 per-Consumer surface 上独立重放**,坐标以 Consumer 客户区左上为原点。
   - 在命令协议层新增两个 opcode(Core 内部决策,线上消息格式 / 既有 opcode 不动)。
   - Consumer 侧 attach 流程追加接收一个 "per-Consumer MonitorLocal surface handle",客户端原有的 World surface 使用路径完全不变。

本 design 不展开 `PUSH_SPACE`/`POP_SPACE` 的精确二进制编码与 per-Consumer surface 的 handle 传输细节(留给 tasks 阶段落到具体 `cmd_decoder` / `protocol` / `server.rs::attach_consumer` 改动)。本 design 只约束修复必须满足的不变量、可观察行为、保留行为,并给出 root cause 假设与 fix/preservation checking 的验证路径。

## Glossary

- **Bug_Condition (C)**:触发任一缺陷的输入条件。本 spec 中 C 是"缺陷 A 的 C_A"与"缺陷 B 的 C_B"的并集(两者 disjoint,后文分别形式化)。
- **Property (P)**:当 C 成立时,修复后系统应展示的行为。P_A = "动画在无窗口事件时也按 vsync 节拍推进";P_B = "MonitorLocal 内容在每个 Consumer 客户区独立出现在 (10,10)"。
- **Preservation**:当 C 不成立时必须与修复前**逐位**相同或逐行为相同的部分。包括:World 空间渲染路径、现有控制平面消息(`RegisterProducer` / `RegisterConsumer` / `CreateCanvas` / `AttachConsumer` / `CanvasAttached` / `SubmitFrame`)的线上格式与字段、`desktop-window` / Game Bar widget Consumer 的既有接入路径。
- **Canvas**:Producer 创建的逻辑画布单位,在 Core 中对应 `core-server/src/renderer/dcomp.rs::CanvasResources`。
- **CanvasResources**:Canvas 在 Core 侧持有的 D3D / DComp 资源集合(texture / rtv / `IPresentationSurface` / `IPresentationBuffer` / `IPresentationManager` / surface handle)。当前为**单 buffer**设计,修复将引入多 buffer 轮转。
- **World space (Canvas space)**:现有坐标系,所有 Consumer 共享一张 surface,坐标相对 Canvas 逻辑尺寸 `(logical_w, logical_h)`。
- **MonitorLocal space**:新增坐标系,坐标相对某个 Consumer 的客户区左上 `(0,0)`,per-Consumer 独立渲染,不在 Consumer 之间共享像素。
- **PUSH_SPACE / POP_SPACE**:命令流新增原语,切换后续 draw 命令的目标空间。未显式 PUSH 的命令视为 World 空间(与 painter-abi-v1.0 §2.2 默认一致)。
- **Consumer viewport**:Consumer 客户区在屏幕上的矩形,由 Consumer 侧 `update_viewport`(`monitors/desktop-window/src/bin/consumer.rs`)上报/维护。本 spec 中"跟随 Consumer 窗口"指 MonitorLocal 绘制内容随 viewport 移动。
- **Frame retirement**:DComp / CompositionSwapchain 提交的帧被 DWM 合成并释放其 buffer 引用的过程。缺陷 A 的可观察症状对应 "buffer 长时间 held by DWM 未 retire"。

## Bug Details

### Bug Condition

本 spec 修复两个独立缺陷。`isBugCondition` 是两个子条件的析取。

**Formal Specification:**

```
FUNCTION isBugCondition(input)
  INPUT: input is a tuple (kind, payload)
    - kind ∈ { SubmitFrame_Tick, WindowStaticPeriod, ConsumerAttach,
               ProducerDrawAt_10_10_LocalIntent, MultiConsumerSameCanvas }
  OUTPUT: boolean

  RETURN isBugCondition_A(input) OR isBugCondition_B(input)
END FUNCTION

FUNCTION isBugCondition_A(input)
  // 缺陷 A:稳定帧率提交 + 无窗口事件 → 画面停滞
  RETURN input.kind == SubmitFrame_Tick
         AND producer_submitting_at_steady_rate(input) // e.g. ~120Hz
         AND all_attached_consumer_windows_static(input.windowState)
                // no drag/resize/minimize/restore in window W
         AND frame_id_is_strictly_increasing(input.recent_frame_ids)
         AND NOT consumer_client_area_visually_advanced(input.observationWindow)
                // pixels on Consumer client area do not change over
                // observationWindow >= 2 * display_refresh_period
END FUNCTION

FUNCTION isBugCondition_B(input)
  // 缺陷 B:Producer 意图在 Consumer 客户区局部坐标绘制
  RETURN input.kind IN { ProducerDrawAt_10_10_LocalIntent,
                         MultiConsumerSameCanvas }
         AND producer_draws_to_logical_coord(input.x, input.y)
         AND intent_is_window_local_anchor(input) // e.g. FPS bar,
                                                  // status badge,
                                                  // window border
         AND consumer_client_origin_on_screen(input.consumer) != (input.x, input.y)
         AND NOT element_visible_in_consumer_client_area(input.element, input.consumer)
END FUNCTION
```

说明:

- `isBugCondition_A` 捕获的是"Producer 稳定提交 + 窗口静止 + `frame_id` 明显递增但 Consumer 像素不变"这个**联合观测**事实 — 光看 Core 日志/Producer 日志看不到 bug(两边都"正常"),必须加上 Consumer 端像素不前进这个维度。
- `isBugCondition_B` 不依赖动画状态:它是关于**空间语义缺失**的,即使只渲染一帧静态内容也能观察到 FPS 条落在"错误"位置。
- 两个子条件可以同时成立,也可以各自独立成立(例如 Producer 只画 World 内容也会触发 A;Producer 不跑动画也能观察到 B)。

### Examples

缺陷 A 具体可观测例子:

- 启动 `core-server` + `demo-producer`(发来回移动的方块 + 随 FPS 变化宽度的条) + `desktop-window` Consumer。**不碰窗口**。预期:方块沿 x 轴来回滑动;实际:方块卡在启动后某一位置几秒甚至无限久,Core 控制台 `SubmitFrame: canvas=1 frame=60 cmds=N` 之类日志每秒打 ~2 次(`frame_id % 60 == 0`)持续刷出,Consumer 内像素**不变**。
- 同上场景,用户按住标题栏微微拖动 Consumer 窗口:拖动的一瞬间方块"蹦"到新位置,松手后又不再推进。
- 同上场景,让 Consumer 窗口被别的窗口遮挡再移开:移开时"蹦"一帧,之后静止。

缺陷 B 具体可观测例子:

- `demo-producer` 在 (10, 10) 画一个 20×4 的绿色 FPS 条。`desktop-window` Consumer 窗口被用户拖到屏幕 (800, 400)。Consumer 客户区里看不到任何绿色条,肉眼看得见的 FPS 条实际落在屏幕全局 (10, 10) 附近的 Canvas surface 区域(如果那里恰好落在某个 Consumer 的 viewport 内)。
- 起两个 `desktop-window` Consumer 同时 attach 到同一个 Canvas,分别摆在屏幕 (0, 0) 和 (1000, 500)。只有靠近屏幕原点的那个可能看到 FPS 条,远端的 Consumer 客户区完全看不到。
- Producer 希望画一个贴 Consumer 左上角的 "REC ●" 录制徽章 — 今天完全没法表达该意图,协议无 PUSH_SPACE/POP_SPACE 原语。

边界例子:

- Producer 仅用 World 空间(本次修复默认 producer 未显式声明空间 = World)→ 不触发 B,也不应被 B 的修复破坏。这是 Preservation 的核心测点。
- Producer 发 `SubmitFrame` 但 `length == 0`(空命令 batch)→ 不触发 A 的可观测症状(因为本来就没有新内容),修复也不得把这个情形变成崩溃或告警。

## Expected Behavior

### Preservation Requirements

**Unchanged Behaviors (必须与修复前一致):**

1. 控制平面消息 `RegisterProducer` / `RegisterConsumer` / `CreateCanvas` / `AttachConsumer` / `CanvasAttached` / `SubmitFrame` 的线上字节布局与字段语义,包括 `MAGIC = 0x4F56524C` / `VERSION = 1` / opcodes `0x0001..=0x0006` / 每条消息的 `payload_len`。
2. 绘制 opcode `CLEAR` / `FILL_RECT` / `STROKE_RECT` / `FILL_ROUNDED_RECT` / `STROKE_ROUNDED_RECT` / `FILL_ELLIPSE` / `STROKE_ELLIPSE` / `DRAW_LINE` 在**未显式声明空间**时的解码与渲染行为:按 World 空间解释,坐标相对 Canvas 逻辑尺寸。
3. `desktop-window` Consumer 现有接入路径:`RegisterConsumer` → 接收 `CanvasAttached` → `DCompositionCreateSurfaceFromHandle(surface_handle)` → `visual.SetContent(surface)` → `CreateTargetForHwnd` → `target.SetRoot` 整条链路不变。Consumer 代码不需要感知 Core 内部 buffer 数量从 1 变成 N。
4. Game Bar widget(UWP 进程)通过 `Windows.UI.Composition.Compositor.CreateSurfaceFromHandle` 挂 visual tree 的能力继续可用,不引入 UWP 沙盒不允许的 API(例如 Desktop-only DComp interop 之外的调用),不要求 Consumer 拿额外类型的新句柄以外的任何新 capability。
5. 多 Consumer 同 attach 一个 Canvas:每个 Consumer 独立拿到可用 surface handle、独立挂屏,互不阻塞,任一 Consumer 退出不影响其他 Consumer。
6. Canvas owner Producer 断开 IPC 时,Core 回收该 Canvas 持有的 D3D / DComp 资源,通知所有 attached Consumer(今天通过直接释放 + 将来 `CanvasDetached`),Consumer 不因悬挂引用 crash。
7. Consumer 窗口被用户移动 / resize 时,`monitors/desktop-window/src/bin/consumer.rs::update_viewport` 路径继续正确反映新的客户区屏幕坐标与 logical/render 比例;World 空间内容相对画布的锚点关系不变。
8. Producer 以远高于显示器刷新率的速率提交帧(例如每 ~8ms 一次)时,Core 不因 present 阻塞拖垮 IPC 读循环,不无限累积内部队列 — 允许丢中间帧,但不得**整个**冻结(这条同时是 Preservation 也是 Fix 的约束:修复不得换成"帧队列无界增长"之类同样糟糕的方案)。
9. Producer-Owned Canvas 语义不松动:Consumer 不能创建画布、不能列举/查询画布,只被动接收 `CanvasAttached`(及未来 `CanvasDetached`)。修复不得为绕过缺陷 B 给 Consumer 引入主动绘制 / 主动申请空间的能力。

**Scope — 完全不受本次修复影响的输入域:**

- World 空间下任何几何原语的像素输出(在像素级别与修复前 bit-identical 或视觉 indistinguishable)。
- 非 `SubmitFrame` 的所有控制消息的处理路径。
- 任一 Consumer 的 DComp visual tree 挂接、HWND target 创建、viewport 更新代码。
- Producer 侧命令 ringbuffer 的编码格式(`core-server/src/ipc/cmd_decoder.rs` 中 `CMD_CLEAR=0x0101`..`CMD_DRAW_LINE=0x0108` 这 8 个 opcode 的字节布局)。

**Note:** 修复后的期望 correct 行为(动画连续推进、MonitorLocal 内容在每个 Consumer 客户区可见)写在 Correctness Properties 一节。本节只界定**必须不变**的部分。

## Hypothesized Root Cause

### 缺陷 A — 动画停滞

阅读 `core-server/src/renderer/dcomp.rs::CanvasResources` 与 `core-server/src/server_task.rs` 的 `SubmitFrame` 处理路径,最可能的 root cause 按优先级列出:

1. **单 buffer + `AddBufferFromResource` 一次性永久绑定导致 buffer 长期被 DWM 持有**(首要假设)。
   - `CanvasResources::new` 只创建一个 `ID3D11Texture2D` + 一个 `IPresentationBuffer`(`manager.AddBufferFromResource(&texture_unk)`),Canvas 生命周期内该 buffer 实例固定。
   - `server_task.rs` 每次 `SubmitFrame` 走 `UpdateSubresource` → `Flush` → `surface.SetBuffer(&self.buffer)` → `manager.Present()` → `SleepEx(0, TRUE)` → 抽干 `GetNextPresentStatistics`。
   - `CompositionSwapchain` 的契约要求:`SetBuffer` 提交给 DWM 合成的 buffer 在 DWM "retire" 之前**应用程序不应该再写它**。单 buffer 情况下:Producer 第二次 `UpdateSubresource` 到同一 texture 时,DWM 可能还在合成第一次的内容,Present 的第二次 `SetBuffer(&buffer)`(同一实例)在 DWM 看来"没有新 buffer 可取"。因此 Present 退化为 no-op 意义上的"告诉合成器没新东西"。
   - `SleepEx(0, TRUE)` + `GetNextPresentStatistics` 抽干循环不能替代**等待 `IPresentationBuffer::GetAvailableEvent`** 这种"等某一个 buffer 变得可写再写"的正确握手;Core 现在**根本没等**,直接在 DWM 仍持有的 buffer 上覆写,然后用同一 handle 再 Present,DWM 看不到 dirty change。
   - 窗口事件触发"蹦一帧":`WM_WINDOWPOSCHANGED` 会促使 DWM 强制重合成整棵 visual tree,重合成时它被迫释放/重抓当前 buffer 快照,此时 Core 写进去的新像素恰好被 DWM 当作"新一帧"采纳,于是画面推进一次,然后又回到"Present 不推进"状态。

2. **`manager.Present()` 返回值被忽略,错误状态下 buffer 回收链路断开**。
   - `Present` 可能返回表示"buffer not yet available"或"device lost"等结果,当前代码对错误只 `eprintln!`,不进入"等 available event 再 retry"路径。

3. **`SleepEx(0, TRUE)` 不在 DComp 线程上下文**。
   - `CompositionSwapchain` 的 retirement 回调通常需要一个有 message pump 或在 APC 可达的线程;tokio task 轮询执行的线程身份不稳定,`SleepEx(0, TRUE)` 在这里对 buffer 回收帮助有限。

4. **DWM 旁路合成优化导致第一次 Present 后整个 swap chain 进入 independent flip 状态**(次要假设)。这种情况下 buffer 轮转要求更严,单 buffer 必然饿死。

### 缺陷 B — 窗口局部 UI 不跟随

读完整条 attach 链路(`ipc/server.rs::create_canvas` / `attach_consumer` / Consumer 侧 `dcomp.rs::consumer_open_surface`):

1. **架构上只有一张全局 shared surface**(根因)。
   - `CanvasResources` 就一个 `handle`。`attach_consumer` 在把它 DuplicateHandle 给各 Consumer 时,**所有 Consumer 拿到的是指向同一 DComp surface 的句柄**。
   - DComp 侧:Consumer 的 `CreateSurfaceFromHandle(h)` 只是在自己 visual tree 挂一个引用同一后备像素的 content 节点。它 **没有任何机制**根据自己的 viewport 偏移"重写"surface 里的像素位置。
   - 因此 Producer 写在 Canvas (10,10) 的条,在**该 Canvas surface 的 (10,10)**,无论哪个 Consumer 把这块 surface 挂到屏幕上,(10,10) 都是同一张全局画布的 (10,10),不是 Consumer 客户区的 (10,10)。
2. **命令协议缺表达 per-Consumer 空间的原语**。
   - 现有 `cmd_decoder` 只解码 8 个纯几何 opcode,没有"从现在起后续命令目标是 MonitorLocal 空间"的切换指令,也没有把同一条命令 broadcast 到 N 个 per-Consumer target 的机制。
3. **Core 只 per-Canvas 渲染一次而不是 per-Consumer 渲染**。
   - `server_task.rs::SubmitFrame` 的处理循环每帧对一个 `ctx` + `canvas.resources` 渲染一次。要支持 MonitorLocal,至少需要在同一帧内对"每个 attached Consumer 的 per-Consumer surface"也各自渲染一次(或用一个替代手段,如 per-Consumer overlay visual + 小 texture)。

## Correctness Properties

Property 1: Bug Condition - 动画在无窗口事件时连续推进

_For any_ 输入 input 满足 `isBugCondition_A(input)`(即 Producer 稳定 ~120Hz 提交、所有 attached Consumer 窗口静止、`frame_id` 单调递增、且观测窗口 ≥ `2 * display_refresh_period`),修复后的 Core SHALL 在该观测窗口内使 Consumer 客户区像素至少推进 `⌊ observationWindow * (display_refresh_rate / 2) ⌋` 次,**不依赖**任何窗口事件(`WM_WINDOWPOSCHANGED` / drag / resize / minimize/restore)或用户交互触发;并且当 Producer 提交速率高于刷新率时,允许丢中间帧但 SHALL NOT 整段观测窗口内像素完全不变。

**Validates: Requirements 2.1, 2.2, 2.3**

Property 2: Bug Condition - MonitorLocal 内容贴每个 Consumer 客户区左上

_For any_ 输入 input 满足 `isBugCondition_B(input)`(Producer 在 MonitorLocal 空间语义下于 (10, 10) 绘制元素,且 Consumer 客户区在屏幕上原点不在 (10, 10);或多个 Consumer 同时 attach 同一 Canvas),修复后的 Core SHALL 对每一个 attached Consumer 在其客户区 (10, 10) 逻辑像素处独立显示该元素,与各 Consumer 在屏幕上的绝对位置**无关**,且 Consumer 之间的 MonitorLocal 内容互不覆盖、互不串位;同时在 Producer 未显式声明空间时 Core SHALL 将命令视为 World 空间。

**Validates: Requirements 2.4, 2.5, 2.6**

Property 3: Preservation - 非 bug 输入行为与原实现一致

_For any_ 输入 input 满足 `NOT isBugCondition(input)`(不触发 A 也不触发 B,典型包括:Producer 只用 World 空间并且窗口在动;或者 Consumer 正在被拖动;或者控制平面消息交互;或者 Producer 提交速率极端高),修复后的 Core SHALL 产生与修复前等价的可观察结果,包括:
- 控制平面消息(`RegisterProducer` / `RegisterConsumer` / `CreateCanvas` / `AttachConsumer` / `CanvasAttached` / `SubmitFrame`)的线上字节 bit-identical 地被产生与消费;
- World 空间内任一几何原语的像素输出视觉 indistinguishable;
- `desktop-window` 与 Game Bar widget 的接入路径在 API 调用序列、句柄种类、visual 挂接方式上不变;
- 多 Consumer 互不阻塞、Producer 崩溃时资源正确回收;
- 高速率提交下既不整体冻结,也不出现无界队列 / 无界内存增长。

**Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7, 3.8, 3.9**

## Fix Implementation

### Changes Required

**假设 §Hypothesized Root Cause 判断正确**,修复落在以下几处。具体代码细节由 tasks 阶段细化;本节锁定"必须改"的表面与"必须不动"的表面。

**File**: `core-server/src/renderer/dcomp.rs`(core resource 改造)

**Struct**: `CanvasResources`

**Specific Changes**:

1. **多 buffer 轮转**:把当前单 `texture` / `rtv` / `buffer` 字段改为 `Vec<>`(长度 N ≥ 2,建议 N = 2 或 3,具体值在 tasks 阶段 benchmark 决定)。每个 buffer 对应一个独立的 `ID3D11Texture2D` + `ID3D11RenderTargetView` + `IPresentationBuffer`(通过 `manager.AddBufferFromResource` 分别挂入同一 `IPresentationManager`)。`render_w` / `render_h` / `handle` / `manager` / `surface` 保持单实例。

2. **正确的 buffer 获取握手**:每次 Producer `SubmitFrame` 时,Core 先选择一个"当前不被 DWM 持有"的 buffer(通过 `IPresentationBuffer::GetAvailableEvent` 的信号态,或 `WaitForMultipleObjects` 在 N 个 event 上选第一个 signaled;最坏情况下做有界等待,超时则丢帧,而不是无限 block 或阻塞读 IPC 的任务)。选中后再 `UpdateSubresource` / `ClearRenderTargetView` / `FillRectangle` 等写操作。

3. **Present 错误分类处理**:`manager.Present()` 的返回值按三类处理 — 成功 / 期待重试(例如"buffer not yet available"应触发下一 tick 再尝试或丢掉本帧) / fatal(device lost,走 Canvas 资源重建路径,对 Consumer 透明)。不得静默 `eprintln!` 吞掉所有错误。

4. **新增 MonitorLocal per-Consumer surface**(缺陷 B):`Canvas` 结构体(`ipc/server.rs`)引入 `per_consumer_surfaces: HashMap<consumer_id, PerConsumerResources>`。`PerConsumerResources` 结构与缩小版 `CanvasResources` 类似(自己的 texture/rtv/buffer 轮转 + 自己的 DComp surface NT handle),尺寸按 Consumer 上报的客户区大小或一个合理上限(例如 `max(consumer_client_w, consumer_client_h)` 或固定 `min(canvas_logical, 4096)`)。

**File**: `core-server/src/ipc/server.rs`

**Function**: `attach_consumer`

**Specific Changes**:

5. **扩展 attach 流程**:attach 时除了 DuplicateHandle 现有 World surface handle,新创建一个 per-Consumer MonitorLocal surface(懒创建也可),并把它的 handle 也 DuplicateHandle 给 Consumer 进程。Consumer 侧 `CanvasAttached` 消息追加该 handle 字段 — **但这破坏了 Preservation 3.1 对 `CanvasAttached` 线上格式的要求**,因此 tasks 阶段必须选择以下两种兼容方案之一:
   - (方案 α,推荐)**不改 `CanvasAttached` 消息**,新增一个 `MonitorLocalSurfaceAttached { canvas_id, consumer_id, surface_handle, logical_w, logical_h }` 消息(新 opcode,不影响旧字段),在已有 `CanvasAttached` 之后紧接着发送;旧 Consumer 不识别该 opcode 时可忽略(需在 `ControlMessage::decode` 中把 unknown opcode 降级为 warn 而非 error,这本身也要求在 Preservation 中小心处理)。
   - (方案 β)保留 `CanvasAttached` 字段布局,在 surface handle 字段内编码"World 句柄";新引入另一条独立消息承载 MonitorLocal 句柄。
   tasks 阶段决策并补丁化实现。

**File**: `core-server/src/ipc/cmd_decoder.rs` 与 `core-server/src/ipc/protocol.rs`

6. **新增 `CMD_PUSH_SPACE` / `CMD_POP_SPACE` opcode(命令 ringbuffer 层,`0x0100..` 范围,不占用控制平面 opcode 空间)**:
   - `CMD_PUSH_SPACE` payload = `u32 space_id`,`space_id = 0` 表示 World、`space_id = 1` 表示 MonitorLocal。
   - `CMD_POP_SPACE` payload = 空。
   - 解码器维护一个 per-`SubmitFrame`-scoped 的栈,栈顶空间决定后续几何 opcode 去哪张 target。
   - **未显式 PUSH 的命令默认 World**,满足 Preservation 3.6。
   - 对栈的不合法使用(POP 空栈、PUSH 不认识的 space_id、帧结束时栈非空) — 跳过该条命令并 warn,不 crash,不影响已提交的其他命令。

**File**: `core-server/src/server_task.rs`

**Function**: `handle_client` 中的 `SubmitFrame` 分支

**Specific Changes**:

7. **双 target 渲染循环**:把当前单一 `ctx` + `canvas.resources` 渲染循环改造成:
   - World 命令(栈顶 = World 或空)→ 写 `canvas.resources`(World 多 buffer)。
   - MonitorLocal 命令(栈顶 = MonitorLocal)→ 对**每一个** `canvas.per_consumer_surfaces[consumer_id]` 各自渲染一次(循环展开这段命令子序列,每个 Consumer 的 target 独立)。
   - 帧尾各 target 独立 `Present`。每个 Consumer 的 per-Consumer surface Present 失败不影响其他 Consumer。
   - 每帧总耗时需要监控:若 Consumer 数很多,MonitorLocal 命令被重放 N 次的 GPU/CPU 成本显著;tasks 阶段需加入简单 metric(例如 rolling avg 渲染耗时日志),超阈值时至少有 log。

**File**: `monitors/desktop-window/src/bin/consumer.rs` / `monitors/desktop-window/src/dcomp.rs`

8. **Consumer 挂双 visual**:在 World 的 `IUnknown` surface 挂接之外,挂第二层 visual 指向 MonitorLocal surface。两层 visual 的 z 顺序:MonitorLocal 在上(覆盖 World),尺寸覆盖整个客户区。已有的 `update_viewport` 依然仅作用于 World 层(World 的 "透视画布的哪一块"语义不变);MonitorLocal 层不对 viewport 做偏移,其像素即"贴客户区"。
   - Game Bar widget 的对应改动在 UWP 端 Compositor 侧同构进行(tasks 阶段按 widget 仓内结构落地)。

## Testing Strategy

### Validation Approach

测试策略分两阶段 **_先_**(在 unfixed 代码上)跑探索测试以 surface 缺陷条件真的触发 — 这一步**还用来证伪 root cause**:如果多 buffer hypothesis 是错的,探索测试给出的 counterexample 形态会告诉我们该换方向;**_再_**(在 fixed 代码上)跑 fix-checking + preservation-checking。

### Exploratory Bug Condition Checking

**Goal**:先在 unfixed 代码上看到 counterexample,并用 counterexample 的**形态**验证或推翻 §Hypothesized Root Cause 的假设(例如:若 A 的 root cause 真是单 buffer,那么手动在 Core 内把 `AddBufferFromResource` 改调 2 次并简单 round-robin SetBuffer — 作为调试补丁,不是正式修复 — 应该看到停滞 symptom 明显减轻;若没减轻,要重新 hypothesize)。

**Test Plan**:启动 `core-server` + `demo-producer` + `desktop-window` Consumer,用 end-to-end 观测手段(pixel-readback 或视频帧差)记录 Consumer 客户区像素序列。对缺陷 B 额外起多 Consumer 配置并在不同屏幕位置摆放。

**Test Cases**:

1. **Animation stall(缺陷 A 核心)**:Producer 稳定发来回移动的方块,1 秒内窗口**绝对不碰**。在 unfixed 代码上应观察到 `frame_id` 日志持续推进,同时 pixel-readback 显示方块位置不变(counterexample 应该是一串 `frame_id=N..N+k` 但 `pixel_hash` 恒定的观测对)。
2. **Animation unstall after window event**:同 1,1 秒后微微拖动窗口 5px 再松手,应观察到 1 帧突变。这个对比样本验证"静止是因为缺触发 recomposition 而不是 Producer 没发内容"。
3. **Single consumer 贴窗口内容(缺陷 B)**:Producer 发 MonitorLocal 意图的 (10, 10) 绿条(本测试作为"规划好的新协议的预期"测,unfixed 代码上必然失败,因为没有 MonitorLocal 协议,会**退化**到 World 空间 → counterexample 形态 = 绿条落全局 (10, 10))。
4. **Multi-consumer 贴窗口内容(缺陷 B 关键)**:两 Consumer 同 attach,分开屏幕位置,都应看到自己客户区 (10, 10) 有绿条。unfixed 代码上必然至多一个 Consumer 看到。
5. **Out-of-viewport World 元素**:Producer 在 World (5000, 5000) 绘制一个元素,Consumer viewport 远离 (5000, 5000) → 该元素不可见(这是正确行为,也不是 bug;列入只是校准观测不把正确行为误判为 bug)。

**Expected Counterexamples**:

- 对 1-2:Consumer 像素 hash 在连续 K 帧上不变、但 `frame_id` 持续递增。
- 对 3-4:对应 Consumer 客户区的 (10, 10) 区域像素 = 背景色(非绿色)。
- 可能原因:
  - A 方向 — 单 buffer 被 DWM 持有、Present 未能交付新内容、错误静默;
  - B 方向 — 协议缺 MonitorLocal 原语、Core per-Canvas 单 surface 架构、Consumer 侧无 MonitorLocal visual 挂接。

### Fix Checking

**Goal**:验证在所有 `isBugCondition` 成立的输入上,fixed 实现满足 Property 1 与 Property 2。

**Pseudocode:**

```
FOR ALL input WHERE isBugCondition_A(input) DO
  run_core_fixed_with(input)
  pixel_series := sample_consumer_pixels(
                     observationWindow = 2 * display_refresh_period)
  ASSERT advance_count(pixel_series) >= floor(
           observationWindow * display_refresh_rate / 2)
  ASSERT NOT requires_any_window_event(pixel_series)
END FOR

FOR ALL input WHERE isBugCondition_B(input) DO
  result := render_fixed(input)
  FOR EACH consumer IN input.attached_consumers DO
    ASSERT pixel_at(consumer.client_area, (10, 10))
             == expected_monitor_local_color(input)
    ASSERT pixel_at(consumer.client_area, NOT near (10, 10))
             NOT contains monitor_local_artifact
  END FOR
END FOR
```

### Preservation Checking

**Goal**:验证对所有 `NOT isBugCondition(input)`,fixed 实现与 unfixed 实现**逐行为**等价。

**Pseudocode:**

```
FOR ALL input WHERE NOT isBugCondition(input) DO
  observable_original := run_core_original(input)
  observable_fixed    := run_core_fixed(input)
  ASSERT observable_original ≈ observable_fixed
    -- where ≈ means:
    --   for control-plane bytes: bit-identical
    --   for World-space pixels:   visually indistinguishable
    --   for consumer lifecycle:   same API call sequences
END FOR
```

**Testing Approach**:Property-based testing 对 preservation checking 至关重要,因为 "NOT isBugCondition" 的输入域极大,手写单元测例遗漏是大头。推荐方法:

- 对控制平面:用 PBT 生成随机合法 `ControlMessage` 序列,分别喂给 original / fixed `ControlMessage::decode` → `encode`,断言输出字节 bit-identical。
- 对 World-space 像素:PBT 随机生成命令 ringbuffer(只使用现有 8 个几何 opcode,不含新 PUSH/POP),在同一 seed D3D11 device 上让 original / fixed Core 渲染,比较最终 texture 的 pixel-by-pixel hash(或允许 ≤ 1 LSB 浮点误差)。
- 对 Consumer 接入:重播录制过的真实 `desktop-window` 启动序列,断言 API 调用 trace 与 handle 种类不变。

**Test Plan**:先在 unfixed 代码上跑上述 PBT 生成的各组 input,采样 / 录制"应当保留"的行为;然后把这些采样作为 oracle,跑在 fixed 代码上比对。

**Test Cases**:

1. **控制平面编解码等价**(PBT):随机合法 `ControlMessage` → `encode` → `decode`,并和 fixed/original 两版实现交叉,断言任何方向的组合都 bit-identical。
2. **World-only 渲染等价**(PBT):随机生成只含 `CLEAR/FILL_RECT/STROKE_RECT/...` 的命令序列(无 PUSH_SPACE),在相同 Canvas 尺寸下 original / fixed 渲染同一帧,比较 Canvas shared surface 的 pixel hash。
3. **desktop-window 挂屏 API trace 等价**:录制 Consumer 启动至稳态的 `DCompositionCreateSurfaceFromHandle` / `SetContent` / `CreateTargetForHwnd` / `SetRoot` 调用与参数 shape(handle 值不同可),断言 fixed 下该 trace 结构不变。
4. **多 Consumer 独立性保留**:一个 Consumer 崩溃(kill 进程)时,另一个 Consumer 持续收到 World 帧不卡顿 — 在 fixed 代码上必须仍然成立。
5. **Producer 崩溃资源回收**:Producer 进程 kill 后 Canvas 相关 D3D/DComp 资源被释放,Consumer 最终收到 detach 事件(至少 World surface 不再 advance),不出现 crash。
6. **高速率提交不冻结 / 不无界增长**:Producer 以 1000Hz 暴力提交 10 秒,断言 Core 进程 RSS 增长 ≤ 阈值,且 Consumer 客户区像素 advance 次数 ≥ 5 秒对应刷新帧数的一半。

### Unit Tests

- `cmd_decoder` 解码器在面对包含 / 不包含 PUSH_SPACE/POP_SPACE 的各类命令流时的行为,包括:POP 空栈、PUSH unknown `space_id`、帧尾栈非空。
- `CanvasResources` 多 buffer 构造 / 销毁的资源对齐(N 个 texture / rtv / buffer 一一对应)。
- `protocol.rs` 现有 6 个消息的 encode/decode 回环(preservation 证据之一)。
- `PerConsumerResources` 按 Consumer viewport 尺寸创建与按 viewport 变化重建。

### Property-Based Tests

- (Preservation)随机控制平面消息 encode/decode 回环 bit-identical(对应 Test Case 1)。
- (Preservation)随机 World-only 命令序列渲染等价(对应 Test Case 2);该 PBT 对"未来若有人不小心在 MonitorLocal 判定里漏写 `default = World`"最敏感。
- (Fix checking)随机 Producer 提交节奏 + 随机观测窗口,断言 Consumer 像素 advance 频率下界。
- (Fix checking)随机 Consumer 屏幕位置 × 随机 MonitorLocal 绘制坐标,断言像素出现在每个 Consumer 的对应客户区坐标。
- (Preservation)随机 Consumer 上下线序列,断言其他 Consumer 的帧不卡。

### Integration Tests

- `core-server` + `demo-producer` + `desktop-window` 三进程端到端:无窗口事件的 10 秒窗口内动画连续推进。
- `core-server` + `demo-producer` + 两个 `desktop-window` 在屏幕不同位置:MonitorLocal FPS 条同时出现在两个 Consumer 各自客户区的 (10, 10)。
- Game Bar widget Consumer:重复上述多 Consumer 测试但其中一个 Consumer 是 UWP 进程,证明 Preservation 3.4 的沙盒兼容性保持。
- 压力测试:100 条 MonitorLocal 命令 × 4 个 Consumer × 60 帧/秒,Core 进程无无界内存增长、World 帧依然 advance。
- 崩溃 / 重连:Producer kill、Consumer kill、Core 重启后 Consumer 显示占位并在 Core 恢复后能重新看到画面(跨越本次修复范围的,Consumer 侧"core lost"状态机不是此 spec 必须改的,但集成测试需观察未回归)。
