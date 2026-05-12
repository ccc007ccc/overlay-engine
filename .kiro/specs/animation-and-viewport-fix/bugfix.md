# Bugfix Requirements Document

## Introduction

`v1.0-server` 架构在接入 Consumer 后暴露出两个影响可用性的渲染缺陷,合并在本 spec 中一并修复:

1. **动画停滞** — Producer 持续以 ~120Hz 提交 `SubmitFrame`,但 Consumer 窗口内画面不随之刷新;仅在用户拖动/调整窗口时画面才"蹦一下"。
2. **窗口局部 UI 不跟随 Consumer 窗口** — Producer 在 canvas (10,10) 绘制的 FPS 指示条等"应贴窗口左上"的元素,实际停留在屏幕绝对全局位置,不跟随 Consumer 窗口移动;多 Consumer 同挂一个 Canvas 时大多数 Consumer 根本看不到。

两个缺陷都发生在 Core 的跨进程 DComp surface 共享管线上,影响 `desktop-window` 与 Game Bar widget 两类已接入的 Consumer。本 spec 仅描述用户可观察行为、修复后预期行为与必须保留的既有行为;具体实现路径(含是否引入多 buffer、是否新增 PUSH_SPACE/POP_SPACE 命令、是否改为 per-consumer 渲染)放在 design 阶段决定。

## Bug Analysis

### Current Behavior (Defect)

**缺陷 A — 动画停滞,仅在窗口事件时推进**

1.1 WHEN Producer 以稳定速率(每 ~8ms)持续发送 `SubmitFrame` AND 所有已 attach 的 Consumer 窗口处于静止状态(未被拖动、未 resize、未最小化/还原) THEN Consumer 窗口内画面内容停留在较早的某一帧,不随新提交的帧更新
1.2 WHEN 用户拖动某个 Consumer 窗口、或触发 `WM_WINDOWPOSCHANGED` 导致 DWM 强制重合成 THEN Consumer 窗口内画面短暂推进一到数帧,拖动停止后立即再次停滞
1.3 WHEN 启动 `demo-producer` + `desktop-window` Consumer 且之后无任何窗口事件 THEN 动态元素(来回移动的方块、FPS 条宽度变化)在视觉上完全不动,但 Core 日志与 Producer 日志都显示 `frame_id` 单调递增

**缺陷 B — 窗口局部 UI 不跟随 Consumer 窗口**

1.4 WHEN Producer 在 canvas 坐标 (10,10) 绘制 FPS 指示条(当前 `demo-producer` 行为) AND Consumer 窗口左上角未位于屏幕 (10,10) THEN 在该 Consumer 客户区内任何位置都看不到该 FPS 指示条;它落在 Consumer 窗口之外的全局屏幕位置 (10,10) 附近
1.5 WHEN 多个 Consumer 同时挂同一个 Canvas 且各自位于屏幕上不同位置 THEN 所有"贴窗口"类元素都仅在恰好覆盖全局 (10,10) 的那个 Consumer 上可见(若有),其他 Consumer 的客户区内不显示
1.6 WHEN Producer 希望把某个 UI 元素(FPS 条、状态徽章、窗口边框等)锚定到 Consumer 窗口客户区左上 THEN 当前命令协议与 Core 渲染管线均无可用手段表达该意图 — 命令流缺少类似 `PUSH_SPACE`/`POP_SPACE` 的原语,Core 亦仅渲染一张在所有 Consumer 间共享的全局 surface

### Expected Behavior (Correct)

**缺陷 A 修复后**

2.1 WHEN Producer 以稳定速率持续发送 `SubmitFrame` AND 所有已 attach 的 Consumer 窗口处于静止状态 THEN 系统 SHALL 在下一个显示器 vsync 周期内使该帧内容在所有已 attach 的 Consumer 窗口内可见,不需要任何外部窗口事件或用户交互触发
2.2 WHEN Producer 连续提交多帧,各帧内容呈现时间上的变化(例如来回移动的方块、宽度随 FPS 变化的条) THEN Consumer 窗口内画面 SHALL 以 ≥ 显示器刷新率 / 2 的视觉连贯度平滑刷新,不出现"卡住 → 窗口事件触发 → 跳进"的抽搐节奏
2.3 WHEN Core 收到 `SubmitFrame` 而相应 canvas 的渲染目标缓冲仍被 DWM 持有 THEN 系统 SHALL 正确等待/调度到可用缓冲(或等价机制),完成 draw + present,使该帧真正被 DWM 采纳而不是被静默丢弃导致"光 Present 没 vsync 推进"

**缺陷 B 修复后**

2.4 WHEN Producer 在 MonitorLocal 空间中于 (10,10) 绘制 FPS 指示条 THEN 该指示条 SHALL 显示在每个 attach 该 Canvas 的 Consumer 窗口客户区左上 (10,10) 像素处(以逻辑尺寸计),与该 Consumer 窗口在屏幕上的绝对位置无关
2.5 WHEN 多个 Consumer 同时挂同一 Canvas AND Producer 混合使用 World 与 MonitorLocal 空间 THEN World 空间内容 SHALL 在各 Consumer 按其 viewport 呈现为"透过窗口看同一张世界画布"(现有行为);MonitorLocal 空间内容 SHALL 在每个 Consumer 各自窗口内独立重复出现,相互不覆盖、不串位
2.6 WHEN Producer 未显式声明空间(沿用当前命令序列) THEN 系统 SHALL 将所有命令视为 World 空间(与 painter-abi-v1.0 §2.2 的默认一致),现有不理解空间概念的 Producer 代码 SHALL 在视觉上与修复前的 World 空间部分保持一致

### Unchanged Behavior (Regression Prevention)

3.1 WHEN Producer 使用现有控制平面消息(`RegisterProducer` / `RegisterConsumer` / `CreateCanvas` / `AttachConsumer` / `CanvasAttached` / `SubmitFrame`) THEN 线上消息格式(opcode、字段布局、字节序) SHALL CONTINUE TO 与 `core-server/src/ipc/protocol.rs` 现状一致,不引入破坏既有 Consumer/Producer 的线上不兼容改动

3.2 WHEN Consumer 为 `desktop-window` THEN 其现有接入路径(`RegisterConsumer` → 收 `CanvasAttached` → `CreateSurfaceFromHandle` → `visual.SetContent` → `CreateTargetForHwnd` → `target.SetRoot`) SHALL CONTINUE TO work,Consumer 侧无须感知服务端是否改变了 buffer 数量或空间语义

3.3 WHEN Consumer 为 Game Bar widget(UWP 进程) THEN 其通过跨进程 DComp surface NT handle 挂入 visual tree 的能力 SHALL CONTINUE TO work,不得因修复引入 UWP 沙盒无法使用的 API 或要求 Consumer 拿到额外的新句柄

3.4 WHEN 多个 Consumer 同时 attach 同一 Canvas THEN 每个 Consumer SHALL CONTINUE TO 独立拿到可用的 surface handle、独立挂屏、互不阻塞对方;任一 Consumer 退出或窗口被销毁不得影响其他 Consumer 的渲染

3.5 WHEN Canvas 的 owner Producer 断开 IPC 连接 THEN Core SHALL CONTINUE TO 回收该 Canvas 持有的 D3D/DComp 资源并通知所有 attach 的 Consumer,Consumer 侧不得因悬挂引用而 crash(遵循 `v1.0-server-bootstrap.md` §4.4 错误处理约定)

3.6 WHEN Producer 仅使用现有绘制 opcode(`CLEAR` / `FILL_RECT` / `STROKE_RECT` / `FILL_ROUNDED_RECT` / `STROKE_ROUNDED_RECT` / `FILL_ELLIPSE` / `STROKE_ELLIPSE` / `DRAW_LINE`)且未声明空间 THEN 这些命令 SHALL CONTINUE TO 按 World 空间解码与渲染,几何坐标相对 canvas 逻辑尺寸的含义不变

3.7 WHEN Consumer 窗口因用户操作移动、resize,触发 `WM_WINDOWPOSCHANGED` THEN `monitors/desktop-window/src/bin/consumer.rs::update_viewport` 路径 SHALL CONTINUE TO 正确反映新的客户区屏幕坐标与 logical/render 比例,World 空间内容相对画布的锚点关系保持不变

3.8 WHEN Producer 以远高于显示器刷新率的速率提交帧(例如每 ~8ms 一次) THEN Core SHALL CONTINUE TO 不因 present 阻塞而拖垮 IPC 读循环,不得无限累积内部队列导致内存持续增长或延迟无界放大;当 Producer 提交速率超过消化速率时允许丢弃中间帧,但不得出现"全部帧都被吞"这种视觉上的完全冻结

3.9 WHEN Core 进程在稳定负载下运行 THEN Producer-Owned Canvas 语义 SHALL CONTINUE TO 成立:Consumer 不能创建画布、不能列举/查询画布,只能被动接收 `CanvasAttached` 与(未来的)`CanvasDetached` 事件;修复不得为绕过缺陷 B 而给 Consumer 引入主动绘制/主动申请空间的能力
