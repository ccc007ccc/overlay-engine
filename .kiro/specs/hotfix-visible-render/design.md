# Hotfix Visible Render — Bugfix Design

## Overview

`animation-and-viewport-fix` 已经把"单 buffer 停滞(缺陷 A)"和"MonitorLocal 架构层面缺失(缺陷 B)"两件事从架构上解决了,自动化测试(`preservation.rs` 22 条 + `bug_condition_exploration.rs` 2 条 + core-server lib 26 条 + renderer 87 条)全部通过,用户也端到端确认了橙色块在无窗口事件下持续滑动(该 spec 的 A 类缺陷**视觉上**已修复)。

但同一次端到端观察暴露出 **MonitorLocal 层(cyan 徽章 + FPS 条)在两个 `desktop-window-monitor` consumer 的 DWM 合成里"根本看不见"**:
- 结构化自动化测试全部通过(声称功能已交付);
- 真实 consumer 客户区里 **没有任何** cyan 徽章或 FPS 条像素。

这是一个**测试通过但交付失败**的真实回归,需要追踪驱动的诊断——最可能的方向是 Core 侧 `dispatch_submit_frame` 的 per-Consumer surface 路由/present 没真正跑、或者 consumer 侧 `AddVisual` z-order/`Commit()` 时序/`IPresentationSurface` 尺寸裁剪有问题;确定哪一个之前不能下决定性修复。

同一次端到端还暴露出三个小的命名/文档缺陷,阻塞后续开发者上手:
- `END-TO-END-TESTING.md` 指向不存在的 `--bin consumer`(实际二进制是 `desktop-window-consumer`)。
- `desktop-window-monitor` consumer 窗口标题永远卡在 `"Desktop Monitor - connecting..."`,即使 `CanvasAttached` + `MonitorLocalSurfaceAttached` 都收到了。
- `core-server/src/bin/demo-producer.rs` 与其 `[[bin]]` 项仍然使用旧的 "producer" 术语,与用户确认的 "monitor / core / app" 三层命名不一致。

本 hotfix 把这四个缺陷打包:
- 2.1 / 2.2 / 2.3 是**范围清晰的 A 类修复**,只触文件名、Cargo 绑定、doc 字符串、窗口标题更新这几个边界。
- 2.4 是**真正需要 trace 驱动诊断**的回归:先按 §Exploratory Bug Condition Checking 列出 H1–H5 五条 hypothesis,在 **unfixed 代码**上跑探索测试确认或推翻,命名根因,再落修复;修复不得只让自动化 harness 绿,而必须让真实 DWM 合成里 cyan 徽章 + FPS 条可见。

修复范围被显式收紧:
- 不动任何协议线上字节(`preservation.rs` 的 `control_plane_bytes.bin` / `control_plane_monitor_local_surface_bytes.bin` 必须仍然 bit-identical)。
- 不做架构决策(那是 `canvas-monitor-lifecycle` spec 的事)。
- 不碰输入事件、不碰 Game Bar widget。
- 所有重命名限定在二进制名 / crate 名 / doc-string 边界——core IPC 的 `Producer` / `register_producer` / `ControlMessage::RegisterProducer` 符号**不动**。

## Glossary

- **Bug_Condition (C)**:四个子缺陷对应的输入条件之并集:`C_doc ∪ C_title ∪ C_rename ∪ C_visible`。
- **Property (P)**:对应每个 C 的期望正确行为:`P_doc`(文档命令能跑起来)、`P_title`(窗口标题在 attach / reconnect 时更新)、`P_rename`(bin 名与 "app" 术语一致)、`P_visible`(cyan 徽章 + FPS 条在每个 consumer 客户区 `(10, 10)` 可见)。
- **Preservation**:不触发上述任何 C 的输入——已通过的 22 + 2 + 26 + 87 条自动化测试、World 层动画(`animation-and-viewport-fix` 的缺陷 A 修复)、控制平面字节 oracle、多 consumer 独立性、Game Bar widget 源码——必须保持不变。
- **Canvas**:Producer 创建的逻辑画布,对应 `core-server/src/renderer/dcomp.rs::CanvasResources`(World surface,单 `handle` + N buffer 轮转)+ `Canvas::per_consumer_surfaces: HashMap<u32, PerConsumerResources>`(MonitorLocal per-Consumer surface)。
- **PerConsumerResources**:每个 consumer 对应一张独立的 MonitorLocal DComp surface(自己的 NT `handle` / `IPresentationManager` / `IPresentationSurface` / buffer 环),定义在 `core-server/src/renderer/dcomp.rs`。
- **CanvasAttached / MonitorLocalSurfaceAttached**:两条控制平面消息,按方案 α(`animation-and-viewport-fix` 所选)先后由 Core 发给 consumer,用来挂 World 与 MonitorLocal 两个 visual。
- **Dual visual tree**:consumer 侧在同一个 `CreateTargetForHwnd` 上挂的根 visual,内含 World 子 visual + MonitorLocal 子 visual(z-order:MonitorLocal 在上)。
- **handleKeyPress(隐喻)**:本次没有键盘输入;这里借用 bug-condition 模板的"输入入口"概念指向 `attach_consumer` / `dispatch_submit_frame` / consumer-side `main` attach 流程这几条被本次 hotfix 触及的函数入口。
- **H1..H5**:缺陷 2.4 的五条 root-cause hypothesis(见 §Hypothesized Root Cause),探索阶段用来判定哪个真正成立。

## Bug Details

### Bug Condition

本 hotfix 修复四个相互独立的子缺陷,`isBugCondition` 是四个子条件的析取。每个子条件都有自己的输入域和可观测判据。

**Formal Specification:**

```
FUNCTION isBugCondition(input)
  INPUT: input is a tuple (kind, payload)
    - kind ∈ { DocCommand, WindowTitleObservation, BinaryNameProbe,
               EndToEndVisibleRender }
  OUTPUT: boolean

  RETURN isBugCondition_doc(input)
         OR isBugCondition_title(input)
         OR isBugCondition_rename(input)
         OR isBugCondition_visible(input)
END FUNCTION

FUNCTION isBugCondition_doc(input)
  // 缺陷 1.1:文档命令指向不存在的二进制
  RETURN input.kind == DocCommand
         AND input.cmd == "cargo run -p desktop-window-monitor --bin consumer"
         AND NOT bin_exists("desktop-window-monitor", "consumer")
         AND bin_exists("desktop-window-monitor", "desktop-window-consumer")
END FUNCTION

FUNCTION isBugCondition_title(input)
  // 缺陷 1.2:标题永远停在 "connecting..."
  RETURN input.kind == WindowTitleObservation
         AND input.timeline observes CanvasAttached AT t_attach
         AND observe_window_title(input.hwnd, t_attach + delta)
              contains "connecting..."
         AND NOT call_site_exists(SetWindowTextW, after CanvasAttached)
         AND NOT call_site_exists(SetWindowTextW,
                                  after MonitorLocalSurfaceAttached)
         AND NOT call_site_exists(SetWindowTextW, on pipe_disconnect)
END FUNCTION

FUNCTION isBugCondition_rename(input)
  // 缺陷 1.3:三层命名不一致
  RETURN input.kind == BinaryNameProbe
         AND (file_path_exists("core-server/src/bin/demo-producer.rs")
              OR cargo_bin_entry("core-server", "demo-producer") exists
              OR doc_strings_in_changed_files use_term("producer")
                 in_context("monitor/core/app layer"))
         AND NOT file_path_exists("core-server/src/bin/demo-app.rs")
END FUNCTION

FUNCTION isBugCondition_visible(input)
  // 缺陷 1.4:MonitorLocal 层在真实合成里看不见
  RETURN input.kind == EndToEndVisibleRender
         AND input.setup == two_desktop_window_monitor_attached_to_one_producer
         AND input.producer emits
               PUSH_SPACE(MonitorLocal) / FILL_RECT(10,10, cyan/fps)
               / POP_SPACE each frame
         AND all_automated_tests_pass(input)
              // 22 preservation + 2 exploration + 26 lib + 87 renderer
         AND NOT pixel_in_client_area(consumer_i, (10,10))
                  matches cyan_or_fps_bar_color
              FOR AT LEAST ONE consumer_i in input.consumers
END FUNCTION
```

说明:
- `isBugCondition_doc` / `_title` / `_rename` 是确定性、静态或本地可观测的;`_visible` 是端到端联合观测的(自动化测试全绿 **且** 真实合成不可见)—— 自动化通过 **不等于** 功能交付,这是本 spec 的核心教训。
- 四个子条件互相 disjoint,可以独立触发独立修复。但 `_visible` 的**诊断**阶段一定要在 `_doc` / `_title` / `_rename` 修复之后进行,以便开发者能按更新过的文档/命名跑端到端 repro。

### Examples

缺陷 1.1 可观测例子:
- 跟随 `END-TO-END-TESTING.md`,粘贴 `cargo run -p desktop-window-monitor --bin consumer` 到终端 → `error: no bin target named 'consumer'` → 开发者被挡在 step 2。
- `cargo build -p desktop-window-monitor` 成功;唯一的 consumer bin 名字是 `desktop-window-consumer`,与包名对不上,也与 spec 用户侧术语 "desktop-window-monitor" 对不上。

缺陷 1.2 可观测例子:
- 按 `END-TO-END-TESTING.md` 起两个 consumer → 两个窗口标题都是 `"Desktop Monitor - connecting..."`;
- 起 producer,日志打印 `[desktop-monitor] CanvasAttached: id=1 handle=0x... log=1920x1080 ren=1920x1080` 和 `[desktop-monitor] MonitorLocalSurfaceAttached: handle=0x... log=1920x1080` → 画面其他现象照常推进,**标题仍然是 `"connecting..."`**;
- kill Core 进程,consumer 管道断连 → 标题**仍然**是 `"connecting..."`,没有切到 `"reconnecting..."` 之类。

缺陷 1.3 可观测例子:
- `cargo run -p core-server --bin demo-producer` 能跑,但 spec 用户侧术语已经统一到 "monitor / core / **app**" —— 这里命名跟 `animation-and-viewport-fix` 留下的其他标识符不一致,新开发者看到 "producer" 会误以为还在旧的 consumer/producer 架构里。
- 阅读 `core-server/Cargo.toml`: `[[bin]] name = "demo-producer"` 是这次 hotfix 的**唯一 Cargo 绑定改动点**。core IPC `Producer` / `register_producer` / `ControlMessage::RegisterProducer` **不动**——这些是协议符号,属于 `canvas-monitor-lifecycle` spec。

缺陷 1.4 可观测例子(关键):
- 按 §Example 起 Core + 2 consumer(拖到屏幕不同位置) + demo-producer;
- `cargo test -p core-server` 全绿;
- 两个 consumer 窗口都看到:
  - 右上黄块、左下粉块、右下白块(World 空间,工作中);
  - 中心十字(World,工作中);
  - 橙色动态块持续 `sin(t)` 滑动(缺陷 A 已经在上一个 spec 修好,工作中);
- **但** 两个 consumer 的左上 `(10, 10)` 附近**都没有 cyan 徽章,也没有 FPS 条**——不是被遮、不是出界、就是"不存在"。
- Core 终端日志显示 `mounted dual visual tree (World + MonitorLocal)` 被打印 —— 即 consumer 认为自己挂了两层 visual,但 DWM 合成结果里只有 World 像素。

边界例子(修复必须不破坏):
- consumer 在无 producer 的情况下启动 → 标题应当停在 `"connecting..."` 直到 attach 完成(标题修复只在 attach 成功 / 断连时更新,不是 startup 立刻改)。
- 只有 World 空间命令的历史 producer(不发 PUSH_SPACE)→ consumer 应当仍然只挂单 visual(`ml_info` 为 `None` 分支),World 画面按 `animation-and-viewport-fix` 的行为推进。

## Expected Behavior

### Preservation Requirements

**Unchanged Behaviors(必须与 hotfix 前逐行为一致):**

- **PE-1**(对应 bugfix 3.1):`core-server/tests/preservation.rs` 的 22 条 preservation 测试全部通过,**任何 oracle 文件不得重写** (`control_plane_bytes.bin` / `control_plane_monitor_local_surface_bytes.bin` / `desktop_window_attach_trace.txt` / `high_rate_bounds.txt` / `multi_consumer_independence.txt` / `world_only_hashes.txt`)。
- **PE-2**(对应 bugfix 3.2):`core-server/tests/bug_condition_exploration.rs` 的 2 条 exploration 测试(`prop_1a_submit_frame_rotates_through_distinct_buffers` / `prop_1b_monitor_local_fill_rect_is_visible_at_each_consumer_10_10`)在本 hotfix 后仍然通过。
- **PE-3**(对应 bugfix 3.3):`core-server` 库 26 条单元测试(`server_task::tests::scan_targets_*` 等)全部通过。
- **PE-4**(对应 bugfix 3.4):renderer 的 87 条测试(`painter` / `resources` / `wic` / `mediafoundation` / `dcomp` 单元测试)全部通过。
- **PE-5**(对应 bugfix 3.5):橙色块在无窗口事件时连续 `sin(t)` 滑动(缺陷 A 的 end-to-end 表现)不退化——也就是说,可见性修复**不得**引入每帧 Present 被 flush / 被阻塞的退化,使得 `isBugCondition_A` 再次触发。
- **PE-6**(对应 bugfix 3.6):控制平面字节 oracle `control_plane_bytes.bin` / `control_plane_monitor_local_surface_bytes.bin` 字节逐位不变——本 hotfix **不改**任何 `ControlMessage::encode` / `decode` 的字节布局;新增标题更新**不**引入任何新的 opcode。
- **PE-7**(对应 bugfix 3.7):`Producer` / `register_producer` / `ControlMessage::RegisterProducer` 三个 IPC 符号**不动**——本 hotfix 对这三个符号的引用路径、类型签名、payload 字节一律不触。
- **PE-8**(对应 bugfix 3.8):单 consumer + 仅 World 命令的端到端路径上,橙色块与 rainbow/块阵像素与 hotfix 前视觉 indistinguishable —— 可见性修复只加工 MonitorLocal 分支的 present/visual 挂接,不得影响 World 分支像素。
- **PE-9**(对应 bugfix 3.9):`multi_consumer_independence.txt` oracle 所检查的 per-Consumer surface 独立性(一个 consumer 的 MonitorLocal surface 状态**不**影响另一个)在 hotfix 后仍然成立。
- **PE-10**(对应 bugfix 3.10):`monitors/game-bar-widget/` 目录内源码与构建产物**完全不动**(没有读,就没有写)。

**Scope — 完全不受本次修复影响的输入域:**

- 所有 World 空间几何命令流(`isBugCondition_visible` 的分母之外的情形)。
- 所有控制平面消息(`RegisterProducer` / `RegisterConsumer` / `CreateCanvas` / `AttachConsumer` / `CanvasAttached` / `SubmitFrame` / `MonitorLocalSurfaceAttached`)的 encode/decode 字节。
- `desktop-window-monitor` 已有 `update_viewport` / `WM_WINDOWPOSCHANGED` 处理路径。
- `core-server` IPC 协议层(`protocol.rs` / `cmd_decoder.rs`)。
- core IPC `Producer` / `register_producer` / `ControlMessage::RegisterProducer` 符号、`Canvas` 结构体布局、`CanvasResources` / `PerConsumerResources` 的字段集合与方法签名——只允许**在已有方法里加 present / acquire / visual 挂接顺序的修正**,不允许加新字段或改字段类型。
- Game Bar widget 整个子项目。

**Note:** 修复后的期望正确行为写在 §Correctness Properties 一节。本节只界定**必须不变**的部分。

## Hypothesized Root Cause

### 缺陷 1.1(文档/二进制命名)

`END-TO-END-TESTING.md` 里用 `--bin consumer`,但 `monitors/desktop-window/Cargo.toml` 的 `[[bin]]` name 是 `desktop-window-consumer`,包名是 `desktop-window-monitor`——三者两两不对齐,是一次只改了其中一个入口的半途重命名遗留。这里无需探索,已经是 100% 定性根因。

### 缺陷 1.2(窗口标题不更新)

阅读 `monitors/desktop-window/src/bin/consumer.rs` 的 attach 流程:
- `CreateWindowExW(...w!("Desktop Monitor - connecting..."), ...)` 在 window 创建时写入初始标题;
- 收到 `CanvasAttached` 后 `println!("[desktop-monitor] CanvasAttached: ...")`,**没有** `SetWindowTextW` 调用;
- 收到 `MonitorLocalSurfaceAttached` / fallback 到单 visual 也都只 `println!`;
- pipe 读失败时 eprint 后走 `return Err(e.into())` / 主循环 `WM_QUIT`,**没有**标题回退逻辑。
- `grep_search SetWindowTextW` 返回 0 —— 整个 repo 从没有人写过改标题的代码。

无需探索,根因已经是定性:缺少 `SetWindowTextW` 调用位点。

### 缺陷 1.3(三层命名)

`core-server/Cargo.toml` 存在 `[[bin]] name = "demo-producer", path = "src/bin/demo-producer.rs"`,文件名和绑定名都用旧术语 "producer"。需要改 `path` 目标文件名 + `[[bin]].name` + 文件内 doc 字符串(只限于本次变更触到的字符)。

注意:`desktop-demo-producer`(在 `monitors/desktop-window/Cargo.toml`)按 bugfix 2.3 **不动**,用注释说明推迟到未来的 rename spec。核心 IPC 协议符号 `Producer` / `register_producer` / `ControlMessage::RegisterProducer` **不动**(bugfix 2.3 + 3.7)。

### 缺陷 1.4(MonitorLocal 不可见)—— 需要 trace 驱动诊断

这是**唯一**需要探索阶段的根因。已知事实:
- `core-server` 的 `attach_consumer` 构造了 `PerConsumerResources`,发出了 `MonitorLocalSurfaceAttached`。
- consumer 日志打印 `mounted dual visual tree (World + MonitorLocal)`。
- 自动化测试(`preservation.rs` / `bug_condition_exploration.rs` / `server_task::tests::scan_targets_*`)全部通过。
- 真实 DWM 合成里 **看不见** cyan 徽章 / FPS 条。

按"哪层最可能出错"排序的 root-cause hypothesis(接下来 §Exploratory Bug Condition Checking 会逐条证伪/确认):

1. **H1 — Core 侧 per-Consumer flush / present 路由错配**。
   - 读 `core-server/src/server_task.rs::dispatch_submit_frame`:单一 `ctx.Flush()` 覆盖所有 target 的 GPU 工作,接着对 World 和每个 per-Consumer 独立 `SetBuffer` + `present` + `SleepEx(0, true)` + drain `GetNextPresentStatistics`。
   - 如果 per-Consumer 分支实际从未进入(例如 `scan_targets` 误判 `local_used = false`、或 `canvas.per_consumer_surfaces` 为空),automated test 里 `scan_targets_*` 可以 cover 逻辑本身,但端到端是否走到**真 GPU 路径**从来没人验过。
   - H1 的可观测 signature:在 `dispatch_submit_frame` 的 per-Consumer Present 分支加 log,如果 producer 发了 PUSH_SPACE 但该 log 从未打出 → 路由错配;如果 log 打出但 PresentOutcome 恒为 RetryNextTick / DeviceLost → 见 H4。

2. **H2 — consumer 侧 `AddVisual` 的 z-order 参数方向错误**。
   - 读 `monitors/desktop-window/src/bin/consumer.rs` 挂 dual visual:
     ```
     root.AddVisual(&visual, false, None);           // World at bottom
     root.AddVisual(&ml_visual, true,  &visual);     // MonitorLocal above World
     ```
   - 参数契约是 `AddVisual(visual, insertAbove: bool, referenceVisual)`。如果方向或 reference 被反了(例如 `referenceVisual` 传了 MonitorLocal 自己而不是 World),DComp 会要么忽略 insertion 要么把 MonitorLocal 放到 World **下面**,结果 World 是不透明像素,把 MonitorLocal 整个遮了。
   - H2 signature:在 consumer 侧 dump visual tree 的 z-order(或者临时交换两次 AddVisual 顺序再看),如果 "MonitorLocal 在下" 能直接解释 cyan 不可见 → H2 是真的。

3. **H3 — dual visual 挂载后没有调 `dcomp_dev.Commit()`**。
   - 读 `monitors/desktop-window/src/bin/consumer.rs`:在 `target.SetRoot(&root)?;` 之后**直接**进入 `update_viewport` + 消息循环。`update_viewport` 自己有 `dcomp_dev.Commit()`,但那是**在第一次 WM_WINDOWPOSCHANGED 之后**才跑——如果第一帧还没有窗口事件,或者 WM_WINDOWPOSCHANGED 到来前 DWM 已经合成过一次,那次合成里 root visual 可能还没生效。
   - H3 signature:在 `target.SetRoot` 之后**显式**加一次 `dcomp_dev.Commit()`;如果这样立刻能看到 cyan → H3 是真的。

4. **H4 — per-Consumer `Present()` 卡在 `RetryNextTick`**。
   - `PerConsumerResources::present` 对 transient 错误会降级到 `RetryNextTick`,消息 `"Present transient error ... — dropping this frame"`。consumer 的 MonitorLocal surface 是一次性初始 `present_color([0,0,0,0])` 之后由 `dispatch_submit_frame` 不断 Present 的;如果从 attach 开始就 retry,DWM 就永远只看到初始透明帧,合成里什么都没有。
   - H4 signature:从 `stderr` 里 grep `"PerConsumerResources] Present transient"`,如果刷屏 → H4 是真的。
   - 可能的触发:`SetBuffer` 在 per-Consumer `buffers` 上 **没** rotate(与 World 分支共用 `acquire_available_buffer`,代码看起来没错,但 buffers 数量 `BUFFER_COUNT = 2` 在某些 DWM 路径下仍会导致 retry)。

5. **H5 — per-Consumer surface 尺寸 / SourceRect 裁剪问题**。
   - `PerConsumerResources::new` 用 `canvas.logical_w / logical_h` clamp 到 `[1, 4096]`。consumer 侧的 `ml_visual` 不做 `SetTransform2` —— 假定 surface 已经 sized 到 consumer client area。但这个假设是错的:surface 的 `SetSourceRect` 是 `(0, 0, render_w, render_h)` = canvas 逻辑尺寸,不是 consumer 客户区尺寸。当 consumer client 是 `720x420` 而 canvas 逻辑是 `1920x1080` 时,`(10, 10)` 附近的 20x4 绿 FillRect 仍然在 surface 的 (10, 10),但 DComp visual 不做缩放的话会把**整张 1920x1080 的 surface** 原样贴到窗口左上,大部分被裁出客户区,cyan 那个 80x80 块贴在 (40, 40) 也可能被窗口的非客户区遮挡或超出显示范围。
   - H5 signature:对比 consumer client size 和 `ml_w / ml_h`,如果显著不匹配 → H5 是真的;一个 quick test 是把 consumer client 临时 resize 到接近 canvas logical,如果 cyan 出现 → H5 是真的。

**优先级**:探索阶段先验 H1(日志最便宜)和 H3(加一行 `Commit()` 最便宜),再验 H4(看 stderr),再验 H2 / H5(需要改代码或改 test harness)。任何一个被证实后都要把其他 hypothesis 降级为"后续观察",避免一次改太多导致修复耦合。

## Correctness Properties

Property 1: Bug Condition - 文档命令可被执行

_For any_ 输入 `input` 满足 `isBugCondition_doc(input)`(即开发者照 `END-TO-END-TESTING.md` 粘贴启动 consumer 的命令),修复后的系统 SHALL 让该命令在 `cargo build` 成功的 workspace 里**直接运行**:
- `monitors/desktop-window/Cargo.toml` 的 `[[bin]].name` SHALL 与其 package 名 `desktop-window-monitor` 一致,即 `desktop-window-monitor`;
- `END-TO-END-TESTING.md` 中所有 consumer 启动命令 SHALL 更新为 `cargo run -p desktop-window-monitor --bin desktop-window-monitor`,与代码一致。

**Validates: Requirements 2.1**

Property 2: Bug Condition - 窗口标题反映 attach / reconnect 状态

_For any_ 输入 `input` 满足 `isBugCondition_title(input)`(即 consumer 观察到 `CanvasAttached` 事件或 pipe 断连事件),修复后的 `desktop-window-monitor` consumer SHALL:
- 在收到 `CanvasAttached`(以及 `MonitorLocalSurfaceAttached`,如果有)之后,通过 `SetWindowTextW` 把窗口标题更新到 attach 后的文本——至少要移除 `"connecting..."` 后缀,推荐格式 `"Desktop Monitor - canvas N (world + monitor_local)"` 或 `"Desktop Monitor - canvas N (world only)"`;
- 在后续 pipe 读取错误 / 断连时 SHALL 把标题更新到 reconnecting 文本(例如 `"Desktop Monitor - reconnecting..."`)。

**Validates: Requirements 2.2**

Property 3: Bug Condition - 三层命名一致

_For any_ 输入 `input` 满足 `isBugCondition_rename(input)`(即对 `core-server/src/bin/demo-producer.rs` 文件路径、`core-server` Cargo `[[bin]]` 项、或相关 doc 字符串的静态查询),修复后的仓库 SHALL:
- 把 `core-server/src/bin/demo-producer.rs` 重命名为 `core-server/src/bin/demo-app.rs`;
- `core-server/Cargo.toml` 的对应 `[[bin]]` 项 `name` 更新为 `demo-app`,`path` 更新为 `src/bin/demo-app.rs`;
- 本次变更触达的 doc 字符串 / README / 注释 使用 "app" 术语;
- `desktop-demo-producer` 在 `monitors/desktop-window/Cargo.toml` 不动,并在其上方加一行 comment 说明推迟到未来的 rename spec;
- core IPC `Producer` / `register_producer` / `ControlMessage::RegisterProducer` 符号**保持不变**(这条在 Property 5 的 preservation 里也兜底)。

**Validates: Requirements 2.3**

Property 4: Bug Condition - MonitorLocal 层在真实 DWM 合成里可见

_For any_ 输入 `input` 满足 `isBugCondition_visible(input)`(即两个 `desktop-window-monitor` consumer attach 到同一个 producer,producer 稳定发 `PUSH_SPACE(MonitorLocal) / FILL_RECT(10,10, cyan/fps) / POP_SPACE`),修复后的系统 SHALL 对**每一个** attached consumer 在其客户区 `(10, 10)` 逻辑像素处显示 cyan 徽章 + FPS 条,与该 consumer 在屏幕上的绝对位置**无关**;并且:
- 修复 SHALL 命名其实际落地的根因(H1 / H2 / H3 / H4 / H5 之一),而不是只改自动化 harness 让它"继续过";
- 修复 SHALL 保留 §Preservation Requirements 列出的全部 unchanged behaviors(PE-1..PE-10),尤其是 PE-5 橙色块动画不退化、PE-8 World 层像素不变。

**Validates: Requirements 2.4**

Property 5: Preservation - 非 bug 输入行为与 hotfix 前一致

_For any_ 输入 `input` 满足 `NOT isBugCondition(input)`,修复后的系统 SHALL 产生与 hotfix 前等价的可观察结果,包括:
- 22 条 preservation 测试 + 2 条 exploration 测试 + 26 条 `core-server` lib 单元测试 + 87 条 renderer 测试全部通过,且所有 oracle 文件字节不变(PE-1..PE-4 / PE-6 / PE-9);
- 橙色块在无窗口事件时连续滑动的端到端行为不退化(PE-5);
- 单 consumer World-only 渲染路径像素视觉 indistinguishable(PE-8);
- core IPC `Producer` / `register_producer` / `ControlMessage::RegisterProducer` 符号不变(PE-7);
- Game Bar widget 目录(`monitors/game-bar-widget/`)不动(PE-10)。

**Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7, 3.8, 3.9, 3.10**

## Fix Implementation

### Changes Required

**假设 §Hypothesized Root Cause 的分析对于 1.1 / 1.2 / 1.3 已经定性正确;1.4 的具体落地要在 §Exploratory Bug Condition Checking 给出具体 hypothesis 被证实后再收敛**。本节列出**确定可做**的修复表面,1.4 的具体修复落点在 `Change-D-*` 下按 hypothesis 分支写出候选,tasks 阶段按探索结果选一或多个。

**Change-A:`END-TO-END-TESTING.md` + `monitors/desktop-window/Cargo.toml` —— 修 1.1**

- **File**: `monitors/desktop-window/Cargo.toml`
  - 把 `[[bin]] name = "desktop-window-consumer", path = "src/bin/consumer.rs"` 改成 `[[bin]] name = "desktop-window-monitor", path = "src/bin/consumer.rs"`;
  - `desktop-demo-producer` 的 `[[bin]]` 项**保持不变**,在其上方加一行 `# TODO(canvas-monitor-lifecycle rename spec): rename desktop-demo-producer -> desktop-demo-app`。
- **File**: `END-TO-END-TESTING.md`
  - 全局替换所有 `cargo run -p desktop-window-monitor --bin consumer` → `cargo run -p desktop-window-monitor --bin desktop-window-monitor`;
  - 检查是否还有别处引用 `desktop-window-consumer`,如有一并更新。
- **Preservation 影响**:这条改动不触代码路径,只改构建配置与文档——不影响 preservation / exploration / lib / renderer 任何自动化测试。

**Change-B:`monitors/desktop-window/src/bin/consumer.rs` 加 `SetWindowTextW` —— 修 1.2**

- **File**: `monitors/desktop-window/src/bin/consumer.rs`
- **Function**: `main`
- **Specific Changes**:
  1. 收到 `CanvasAttached` 成功解析后,立即调一次 `SetWindowTextW(hwnd, w!("Desktop Monitor - canvas {canvas_id} (world only)"))`(在 `ml_info` 还未知前先上 world-only 版本)。
  2. `ml_info.is_some()` 分支(成功挂 dual visual)内,再调一次 `SetWindowTextW(hwnd, w!("Desktop Monitor - canvas {canvas_id} (world + monitor_local)"))`。
  3. 主消息循环中 pipe 读取失败 / tokio::select 超时以外的 I/O 错误 branch,调 `SetWindowTextW(hwnd, w!("Desktop Monitor - reconnecting..."))`,保留 "Desktop Monitor -" 前缀便于运维辨识。
  4. 使用 `windows::core::w!` 宏或 `PCWSTR::from_raw`;标题字符串含 `{canvas_id}` 时需要用 `Vec<u16>::from_iter` + `PCWSTR::from_raw` 或 `HSTRING::from(...)` 的路径——具体在 tasks 阶段决定(选择一种与 workspace 其他 `w!` 用法一致的)。
- **Preservation 影响**:仅加 3 个 `SetWindowTextW` 调用位点,不改 attach 流程顺序、不改 visual 挂接、不改 `update_viewport`。`desktop_window_attach_trace.txt` oracle 目前记录的是 attach API 调用,如果 oracle 里没有 `SetWindowTextW`(它应该没有,因为它不是 DComp/D3D 调用),这条改动不会碰 oracle。

**Change-C:`core-server/src/bin/demo-producer.rs` + `core-server/Cargo.toml` —— 修 1.3**

- **File**: `core-server/src/bin/demo-producer.rs` → **重命名为** `core-server/src/bin/demo-app.rs`(文件内容搬迁;文件内 doc 字符串/注释如果提到 "producer (demo)" / "test-producer" / "demo producer" 的自我描述处,改成 "app"/"demo app";但**不动** `ControlMessage::RegisterProducer` / `register_producer` 调用,这些是 IPC 协议符号)。
- **File**: `core-server/Cargo.toml`
  - `[[bin]] name = "demo-producer", path = "src/bin/demo-producer.rs"` → `[[bin]] name = "demo-app", path = "src/bin/demo-app.rs"`。
- **File**: `END-TO-END-TESTING.md`
  - 把所有 `cargo run -p core-server --bin demo-producer` 替换为 `cargo run -p core-server --bin demo-app`;
  - 正文解释里 "demo-producer" → "demo-app"(术语替换,docs 上下文);但**保留** "Producer" 在协议/IPC 上下文里的原写法(例如 "Producer 稳定以 120Hz 调 SubmitFrame")以免引入概念错误。
- **Preservation 影响**:bin 重命名不触协议字节,不触 lib 测试,不触 preservation oracle。

**Change-D:`isBugCondition_visible` 根因修复 —— 修 1.4(候选,按探索结果选)**

以下候选对应 H1–H5,探索阶段必须至少证实其中一条(或证伪全部后**重新 hypothesize**)。tasks 阶段在本节基础上落实具体 diff。

- **Change-D1(若 H1 成立)—— `core-server/src/server_task.rs::dispatch_submit_frame`**
  - 补充 per-Consumer Present 分支的诊断日志(至少在 debug 构建下),或修正 `scan_targets` / `canvas.per_consumer_surfaces` 迭代顺序,确保 PUSH_SPACE(MonitorLocal) 真的触发了 `pc.present()` 调用。
  - 典型 diff:在现有 `for (cid, idx) in &local_idxs` 循环加 `eprintln!("[server_task] canvas={canvas_id} frame={frame_id} consumer={cid} MonitorLocal Present OK")` 在 `PresentOutcome::Success` 分支;或者把遗漏的 SetBuffer / Present 误单放在 early-return 分支里的 bug 搬出来。

- **Change-D2(若 H2 成立)—— `monitors/desktop-window/src/bin/consumer.rs` 挂 dual visual 处**
  - 现有代码:
    ```rust
    root.AddVisual(&visual,     false, None::<&IDCompositionVisual>)?;
    root.AddVisual(&ml_visual,  true,  &visual)?;
    ```
  - 根据 H2 的发现修正 `insertAbove` / `referenceVisual` 参数方向。典型 diff:如果 DComp 的实际语义是 "insertAbove = true 意味着 visual 插在 reference 之上(更靠近用户)",现有代码看起来对——那么 H2 证伪;如果实际语义反了(官方 docs 措辞歧义历史上出过 bug),diff 就是交换 `true/false`。

- **Change-D3(若 H3 成立)—— 在 `target.SetRoot(&root)?;` 之后补 `dcomp_dev.Commit()?;`**
  - 单行修复,最便宜的修复路径。已有的 `update_viewport` 在第一次 `WM_WINDOWPOSCHANGED` 之前不 fire,所以 root commit 可能永远延迟。

- **Change-D4(若 H4 成立)—— `core-server/src/renderer/dcomp.rs::PerConsumerResources::present` / 对应调用点**
  - 检查 `acquire_available_buffer` 的 timeout / `BUFFER_COUNT` 与 World `CanvasResources` 是否一致,或者是否 per-Consumer manager 因为 `AddBufferFromResource` 调用顺序差异导致 buffer event 一直不 signal。
  - 修复可能是把 `BUFFER_COUNT` 调到 3(已有注释说这是 tunable),或者修正 `present_color` 初始 clear 后的 `SleepEx` / `drain` 顺序。

- **Change-D5(若 H5 成立)—— `core-server/src/renderer/dcomp.rs::PerConsumerResources` 构造 / consumer 侧 `ml_visual` 的 Transform**
  - 要么让 per-Consumer surface 尺寸跟随 **consumer client area**(需要 consumer 侧在 attach 时上报 client 尺寸,但这会改协议——超本 spec scope)——所以更可能的修复是 **consumer 侧** 在 `ml_visual.SetTransform2` 加一个 `render → client` 的缩放或 `SetSourceRect`/`SetClip` 把可见区域收到 client area 内。
  - 这条修复比其他几条复杂,tasks 阶段要重新评估是否超出 hotfix scope——如果是,回退到打一个"MonitorLocal 只能在 canvas_logical ≥ client_area 时可见"的 known-issue,把完整修复推迟到 `canvas-monitor-lifecycle` spec。

**收口原则**:Change-D 的任何一条 diff **必须**在修复前先在 **unfixed** 代码上跑探索测试、采到 counterexample,才允许实施。"加一行 Commit 然后跑自动化测试看看绿不绿"是不被接受的——因为本 spec 的起因就是"自动化绿 ≠ 交付"。

## Testing Strategy

### Validation Approach

本 hotfix 的验证策略明显分成两档:
- 缺陷 1.1 / 1.2 / 1.3 是 A 类小修复,可观测判据确定、修复路径确定,直接按 §Fix Checking 写测试;
- 缺陷 1.4 是必须 trace 驱动诊断的端到端回归,先按 §Exploratory Bug Condition Checking 证实/证伪 H1–H5 各 hypothesis,命名根因,再按 §Fix Checking 落测试。

两档都严格走:**_先_** 在 unfixed 代码上 surface counterexample,**_后_** 验证 fixed 代码不破坏任何 Preservation。

### Exploratory Bug Condition Checking

**Goal**:在 unfixed 代码上 surface 1.4 的 counterexample,逐条证实/证伪 H1–H5,命名根因。1.1 / 1.2 / 1.3 不需要探索(静态可判)。

**Test Plan**:

Part 1 —— **端到端 trace capture**(手工 + 脚本):
1. 按更新过的 `END-TO-END-TESTING.md`(已完成 Change-A / Change-C 命名修复后)起 `core-server` + 2 个 `desktop-window-monitor` + `demo-app`。拖两个 consumer 到屏幕不同位置(避开 `(10, 10)` 屏幕坐标)。
2. 在 Core 终端捕获 stdout/stderr 到文件;特别 grep:
   - `PerConsumerResources] Present transient` —— 命中 H4。
   - `dispatch_submit_frame` 相关 per-Consumer Present 日志 —— 如果缺失或全是 dropped,命中 H1。
3. 在 consumer 终端捕获 stdout/stderr;确认 `mounted dual visual tree` 确实打出过。
4. 截图 consumer 客户区 `(10, 10)` 附近 40x40 区域像素,确认 cyan 值不存在。

Part 2 —— **针对 H1-H5 的逐条试验**(只在 unfixed 代码上临时加诊断,不 commit):
- **验 H1**:在 `dispatch_submit_frame` 的 per-Consumer Present 分支加 `eprintln!("ml present ok cid={cid}")`;跑 E2E,若从不打出 → H1 成立。若打出但对应 consumer 依然不可见 → H1 证伪,继续。
- **验 H3**:在 consumer 侧 `target.SetRoot(&root)?;` 后临时加 `dcomp_dev.Commit()?;`;跑 E2E,若 cyan 立刻可见 → H3 成立。
- **验 H4**:在 Core stderr grep `Present transient`;若持续出现 `canvas=1 frame=... consumer=1 MonitorLocal Present transient`,H4 成立。
- **验 H2**:临时交换 consumer 侧 `AddVisual` 的 `insertAbove` 参数方向或交换 root / reference,跑 E2E;若交换后 cyan 可见 → H2 成立;若交换后两层都不可见 → 之前方向是对的 → H2 证伪。
- **验 H5**:把 consumer 启动 window 的初始 size(`CreateWindowExW` 的 `720, 420`)改大到接近 canvas logical(例如 `1920, 1080`);跑 E2E,若 cyan 可见(或 FPS 条可见)→ H5 成立。

Part 3 —— **自动化补充**(可选):为被证实的 hypothesis 写一条 `core-server/tests/` 下的 exploration 风格测试,模拟其失败 signature(以模仿 `bug_condition_exploration.rs` 现有模式);任务阶段评估是否 worth 写,若该 hypothesis 本身是单行 commit 修复,直接进入 Fix Checking 即可。

**Test Cases**:

1. **H1 探索 — per-Consumer Present 路由验证**:unfixed 代码,producer 发 PUSH_SPACE(MonitorLocal),grep Core stderr 查找 per-Consumer Present 日志(will fail on unfixed code if H1 holds)。
2. **H3 探索 — dual visual root commit 验证**:unfixed 代码基础上临时加 `dcomp_dev.Commit()` 在 `SetRoot` 之后,肉眼观察 cyan 是否出现(will fail on unfixed code if H3 holds 且没加 Commit)。
3. **H4 探索 — per-Consumer Present transient 日志**:unfixed 代码,运行 10 秒,统计 `Present transient` 行数。如果 > 0 且密度随帧率增长,H4 成立。
4. **H2 探索 — AddVisual z-order 反转试验**:unfixed 代码,临时交换 `insertAbove` 参数,对比 cyan 可见性前后差异。
5. **H5 探索 — surface 尺寸 vs client area 对比**:unfixed 代码,分别用 720x420 窗口与 1920x1080 窗口跑,对比 cyan 可见性(可能失败于 unfixed code 且 H5 成立)。

**Expected Counterexamples**:

- 对 1.4(某一条 hypothesis 成立):上述某一条 Test Case 给出"明确的可重现 signature"——要么某条 log 总是缺失 / 总是出现,要么某个参数交换立刻让视觉 output 翻转。
- 对 H1:`eprintln!` 日志从不打出。
- 对 H3:只加一行 `Commit()` 就让 cyan 可见,这是最强的确认。
- 对 H4:`Present transient` 行数 ≥ 每秒几百行,持续整个 run。
- 对 H2:交换参数后 cyan 和 World 的相对可见性调换(说明方向确实反了)。
- 对 H5:小窗口不可见,大窗口可见。
- 对 1.4(所有 hypothesis 均证伪):回到 §Hypothesized Root Cause 重新 hypothesize(Testing Strategy 明确允许这种分支)。

### Fix Checking

**Goal**:验证对所有 `isBugCondition` 成立的输入,fixed 实现满足 Property 1 / 2 / 3 / 4。

**Pseudocode:**

```
FOR ALL input WHERE isBugCondition_doc(input) DO
  cargo_check := run("cargo run -p desktop-window-monitor --bin desktop-window-monitor --no-run")
  ASSERT cargo_check.status == success
  ASSERT END_TO_END_TESTING_md contains new consumer command string
END FOR

FOR ALL input WHERE isBugCondition_title(input) DO
  run_consumer_with(input) until CanvasAttached received
  ASSERT window_title(input.hwnd) does_not contain "connecting..."
  ASSERT window_title(input.hwnd) contains "canvas" AND contains "world"
  simulate pipe_disconnect
  ASSERT window_title(input.hwnd) contains "reconnecting"
END FOR

FOR ALL input WHERE isBugCondition_rename(input) DO
  ASSERT file_exists("core-server/src/bin/demo-app.rs")
  ASSERT NOT file_exists("core-server/src/bin/demo-producer.rs")
  ASSERT cargo_bin_entry("core-server", "demo-app") exists
  ASSERT NOT cargo_bin_entry("core-server", "demo-producer") exists
  ASSERT symbol_exists("Producer"), symbol_exists("register_producer"),
         symbol_exists("ControlMessage::RegisterProducer")  -- 保留
END FOR

FOR ALL input WHERE isBugCondition_visible(input) DO
  result := render_fixed(input)
  FOR EACH consumer IN input.consumers DO
    ASSERT pixel_at(consumer.client_area, (10, 10))
             matches cyan_or_fps_bar_color
    ASSERT pixel_at(consumer.client_area, NOT near (10, 10))
             does_not contain monitor_local_artifact
  END FOR
  ASSERT chosen_root_cause IN { H1, H2, H3, H4, H5 }
  ASSERT fix_diff addresses chosen_root_cause (not only test harness)
END FOR
```

### Preservation Checking

**Goal**:验证对所有 `NOT isBugCondition(input)`,fixed 实现与 hotfix 前逐行为等价,特别是 PE-1..PE-10 不退化。

**Pseudocode:**

```
FOR ALL input WHERE NOT isBugCondition(input) DO
  observable_hotfix    := run_fixed(input)
  ASSERT observable_hotfix matches preservation_oracle(input)
    -- where matches means:
    --   control_plane_bytes.bin               : bit-identical
    --   control_plane_monitor_local_..._bytes.bin : bit-identical
    --   desktop_window_attach_trace.txt       : text-identical
    --   high_rate_bounds.txt                  : text-identical
    --   multi_consumer_independence.txt       : text-identical
    --   world_only_hashes.txt                 : text-identical
END FOR

ASSERT preservation.rs              :: 22 tests PASS
ASSERT bug_condition_exploration.rs :: 2  tests PASS
ASSERT core-server lib              :: 26 tests PASS
ASSERT renderer                     :: 87 tests PASS
```

**Testing Approach**:Property-based testing 对 preservation 仍然是第一推力,因为 "NOT isBugCondition" 的输入域极大,手写例子覆盖不全。但由于本 hotfix 对代码路径的改动**最小**(都是 additive:新 `SetWindowTextW` 调用、bin 重命名、doc 更新、MonitorLocal present/visual 分支的诊断级修正),preservation 主要靠**不破坏现有 oracle**来背书:
- 已有的 PBT A / A' / B / C / D 已经用 `proptest` 覆盖了控制平面、World 像素、多 consumer 独立性、高速率有界等面。跑它们就够。
- 对橙色块动画(PE-5)与 World-only 像素(PE-8):跑 `preservation.rs` PBT B 就能断言 World-only hash 不变。若需要额外端到端肉眼验证,按 §Example 中 `animation-and-viewport-fix` 的 end-to-end 指南跑一次。

**Test Plan**:
- Commit 前在本地先跑 `cargo test -p core-server --workspace`,要求 22 + 2 + 26 + 87 条全绿。
- 在 unfixed 基线(即打 hotfix 之前)按 `preservation_oracles/` 中已存在的 oracle 做一次 reference run,记录 baseline。
- 打上本 hotfix 所有 Change-A..Change-D diff 后,再跑一次完整测试套件;任何 oracle 比对失败都是 preservation 回归,必须反查 Change-* 哪条破坏了。

**Test Cases**(所有都应继续通过,无需新写):

1. **PBT A(控制平面 encode/decode bit-identical)**:`preservation.rs` 已有,assert `control_plane_bytes.bin` 不变。
2. **PBT A'(`MonitorLocalSurfaceAttached` 回环)**:`preservation.rs` 已有,assert `control_plane_monitor_local_surface_bytes.bin` 不变。
3. **PBT B(World-only 像素 hash 等价)**:`preservation.rs` 已有,assert `world_only_hashes.txt` 不变——直接验证 PE-8。
4. **desktop-window 接入 trace**:`preservation.rs` 已有,assert `desktop_window_attach_trace.txt` 不变——直接验证 PE-1 的 attach 调用序列。Change-B 的 `SetWindowTextW` 不出现在 DComp/D3D trace 里,所以该 oracle 不动。
5. **多 consumer 独立性**:`preservation.rs` 已有,assert `multi_consumer_independence.txt` 不变——直接验证 PE-9。
6. **高速率有界**:`preservation.rs` 已有,assert `high_rate_bounds.txt` 不变——直接验证 PE-5 的反面(不得把高速路径变成阻塞)。
7. **exploration 重跑**:`bug_condition_exploration.rs` 2 条仍过——验证 PE-2。
8. **core-server lib 单元测试**:`cargo test -p core-server --lib` 全绿——验证 PE-3。
9. **renderer 单元测试**:`cargo test` 在 renderer 相关 83 + 4 模块内全绿——验证 PE-4。
10. **Game Bar widget 不动**:`git diff --stat monitors/game-bar-widget/` 无任何变更——直接验证 PE-10。

### Unit Tests

- **Change-A** 不新增单元测试(Cargo 绑定变化由 `cargo build` 自身覆盖)。
- **Change-B** 可选加一条 consumer 内部辅助函数的单元测试(如 `format_window_title(attach_state) -> String`),验证三种 state(connecting/attached/reconnecting)返回对应字符串;但 `SetWindowTextW` 本身依赖 HWND,不在单元测试覆盖范围。
- **Change-C** 不新增单元测试(重命名由 `cargo test -p core-server --bin demo-app --no-run` 的 compile check 覆盖)。
- **Change-D** 按被证实的 hypothesis 决定:H3 加 single-line Commit 无单元测试价值;H1 / H4 可以考虑给 `dispatch_submit_frame` 加一条 fixture(不启 GPU 的 mock 路径),但可能需要暴露内部 hook,tasks 阶段评估成本/收益。

### Property-Based Tests

本 hotfix **不新增 PBT**:现有的 PBT A / A' / B / C / D 已经完整覆盖 Preservation 面。如果 §Exploratory 发现某条 hypothesis 需要一条新的 exploration-style PBT(例如模拟 per-Consumer Present 的 event signal 时序),在 tasks 阶段按需添加,放在 `core-server/tests/bug_condition_exploration.rs` 或同级新文件。

### Integration Tests

- **IT-1(缺陷 1.1)**:`cargo run -p desktop-window-monitor --bin desktop-window-monitor --no-run` 返回 0。可直接做成 CI smoke step。
- **IT-2(缺陷 1.2)**:启动 Core + 单 consumer + demo-app,observation window 内抓取窗口标题(使用 `GetWindowTextW` 从外部查 HWND,或在 consumer 自己的日志里打印"标题 X 在 t 时刻被更新为 Y"),assert 标题在 attach 后不含 `"connecting..."`,在 Core kill 后含 `"reconnecting"`。手工或配合 Win32 自动化工具。
- **IT-3(缺陷 1.3)**:`cargo build --workspace` 通过(等于验证所有 bin 名字、文件路径、Cargo 绑定自洽),`rg "demo-producer" core-server` 命中为 0(文件内及 Cargo),`rg "Producer|register_producer|ControlMessage::RegisterProducer" core-server/src/` 命中数量**与 hotfix 前保持一致**(不新增,不减少——验证 PE-7)。
- **IT-4(缺陷 1.4,核心)**:按更新后的 `END-TO-END-TESTING.md` 起 2 个 consumer + demo-app,肉眼观察两个 consumer 客户区左上 `(10, 10)` 附近是否同时出现 cyan 徽章 + FPS 条。可以用截图工具 + 人眼判断,也可以用 `PrintWindow` / `BitBlt` 抓两个 consumer 客户区 bitmap 并搜索 cyan(rgb ≈ (0, 229, 229) 的 8x8+ 连通块)。
- **IT-5(Preservation 端到端)**:同 IT-4 的设置,但**先**在不打 hotfix 的 commit 上录一遍 orange 块滑动的视频(PE-5 基线);打上 hotfix 后再录一遍,两段视频的滑动轨迹肉眼一致——验证 PE-5 / PE-8。
- **IT-6(Game Bar widget 不动)**:CI 加一条 `git diff --stat origin/main HEAD -- monitors/game-bar-widget/` 为空的 guard。若需要严格,把 PE-10 做成 pre-commit hook。
