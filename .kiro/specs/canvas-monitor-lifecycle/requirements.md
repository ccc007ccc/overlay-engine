# Requirements Document

## Introduction

本 spec 收口三件被前两个 spec 刻意推迟的事,来源是用户在 `hotfix-visible-render` 端到端验证完成后给出的三条反馈:

1. **命名不一致** — "consumer 不是全部说好了改名成 monitor, producer 改成 app 吗, 现在好混乱, 有点看不懂"。`hotfix-visible-render` 只改了用户可见的 bin 名(`demo-producer` → `demo-app`、`desktop-window-consumer` → `desktop-window-monitor`),但协议符号(`ControlMessage::RegisterProducer` / `AttachConsumer`)、IPC 内部结构(`Producer` / `Consumer` / `per_consumer_surfaces`)、以及 `core-server/src/server_task.rs` 约 21 处局部变量与日志文案仍然使用旧的 "producer" / "consumer" 术语。`monitors/desktop-window/Cargo.toml` 第 12 行有 `TODO(canvas-monitor-lifecycle rename spec)` 标记,显式指向本 spec。

2. **App 关闭后 Monitor 不关** — "app 关闭了监视器不会关(如果是小组件的话是这个逻辑)"。当前 App 断 pipe 后,Core 清理 `per_consumer_surfaces`,但不主动通知任何已 attach 的 Monitor;`desktop-window-monitor` 进程只是让每个窗口的标题跳到 `"Desktop Monitor - reconnecting..."`(`hotfix-visible-render` Change-B),之后继续运行。用户期望 **App 关闭 → 所有独立 Monitor 进程跟着清退**;Game Bar widget 作为 UWP 宿主另有其生命周期。

3. **没有单实例模型** — "我希望一个监视器模块任何时间只能启动一个后台进程, 由这个进程拉起窗口, 而不是我要手动开两个监视器"。当前每个 `desktop-window-monitor.exe` 都是独立进程、独立 `RegisterConsumer`;用户希望 **任意时刻最多一个 desktop-window-monitor 后台进程**,由它统一管 N 个窗口。

除此之外,同一次端到端观察还留了一个**开放范围**(是否纳入本 spec 由用户决定): FPS 数字缺失。用户看到"长方形条和正方形条, 没有fps数字",原因是 `demo-app.rs` 只发 `CMD_CLEAR` + `CMD_FILL_RECT`,没有文字命令;Core 也没接 DirectWrite。

### 本 spec 的边界

- **包含**: 协议层与内部结构的统一重命名(producer/consumer → app/monitor);App 断开后 Core 通知 Monitor 的生命周期协议;`desktop-window-monitor` 的单实例 + 多窗口模型。
- **不改线上字节**(除非 Req 2 / Req 3 引入新 opcode): 对于 opcode `0x0001..=0x0007`,重命名**只改 Rust 符号**,不动线上字节;`control_plane_bytes.bin` / `control_plane_monitor_local_surface_bytes.bin` 两份 oracle 必须仍然 bit-identical。
- **保留前两个 spec 的 137 条绿色基线**(22 preservation + 2 exploration + 26 lib + 87 renderer)。
- **保留"中途连接"能力**: Monitor 可以在 App 已经 Register + CreateCanvas 之后启动并 attach,这是当前已工作的行为(`attach_consumer` 在 `AttachConsumer` 到达时才懒创建 per-Consumer surface),必须不破。
- **后续 spec 留白**: Game Bar widget 目录(`monitors/game-bar-widget/`)是否在本 spec 内做生命周期接入是开放决策(见 §开放决策 D5)。文字渲染(FPS 数字 / `CMD_DRAW_TEXT` / DirectWrite)是否纳入本 spec 是开放决策(见 §开放决策 D1)。

## 开放决策

以下 5 个决策点在 requirements 层表达为高层需求,**具体选型在设计阶段锁定**。用户 review requirements 时需要就这 5 点给出方向,否则 design 阶段无法继续。文档中每个 Requirement 的 AC 都以"与决策无关的方式"书写;决策敲定后,design 文档会把决策结果 back-propagate 成具体字节/API。

### D1 — FPS 数字是否纳入本 spec?

**问题**: `hotfix-visible-render` 已经把 MonitorLocal 层的 FPS 条(颜色方块)做出来了;要显示实际 FPS **数字**需要新增 `CMD_DRAW_TEXT` opcode + payload 格式 + DirectWrite 集成 + `demo-app.rs` 端文字命令构造。

**选项**:
- A(推迟,推荐): 推到单独的 `text-rendering` spec;本 spec 不触 painter ABI、不动 renderer 文字路径。
- B(纳入): 在本 spec 里做 `CMD_DRAW_TEXT` + DirectWrite + `demo-app.rs` 发 FPS 数字。会显著扩大本 spec 的 renderer preservation 面。
- C(只做最小): 加 `CMD_DRAW_TEXT` opcode 与 decoder 路径,但 painter 侧当前仅当 no-op;painter 接入留给后续。

### D2 — App 断开后 Monitor 如何得知?

**问题**: Core 检测到 App pipe EOF 时,Monitor 还**无从区分**三种情况: (a) App 有序退出 — Monitor 应该清退; (b) Core 自己崩溃 — Monitor 应该清退; (c) Monitor 自己的 pipe 抖动(App 和 Core 还在) — Monitor 应该重连。

**选项**:
- A(新增 `AppDetached` 控制消息,推荐): Core 在清理 `Producer` 之前主动给该 Producer 所有已 attach 的 Consumer 发一条明确的 detach 通知。Monitor 收到 → 清退;纯 pipe EOF 没有前导通知 → 重连。需要新增一个 opcode(具体值留给 design)。
- B(Monitor 探测): Monitor 检测 pipe EOF 后自己 retry,失败 N 次超时 → 清退。不需要新 opcode,但 Monitor 要区分"Core 死了"和"短暂抖动"仍然困难。
- C(双通道): 除了现有数据 pipe,额外开一条"心跳 pipe",心跳断掉即清退。成本高,引入第二个 IPC 通道。

### D3 — Monitor 的"lifecycle 类型"如何表达?

**问题**: 独立的 `desktop-window-monitor.exe` 应该在 App 断开时退出进程;Game Bar widget 作为 UWP 宿主不能退出进程(它是宿主应用的一部分),只能关窗口。Core 需要知道对端是哪种类型吗,还是由 Monitor 进程自己决定"收到 detach 通知后做什么"?

**选项**:
- A(Monitor bin 自决,推荐): 协议不区分类型;Core 统一发 detach 通知,Monitor 进程自己决定是"退进程"还是"关窗口保宿主"。`desktop-window-monitor` bin 硬编码为"退进程";Game Bar widget 硬编码为"关窗口"。协议层零改动。
- B(Register 消息携带类型): `RegisterMonitor` 的 payload 新增一个 `lifecycle_hint: u8` 字段(Standalone / Hosted),Core 据此决定发什么、Monitor 进程也据此行动。需要改 `RegisterConsumer` 的线上字节(当前 payload 只有 4-byte pid),破 PE-6。
- C(混合): Monitor bin 自决行为,但 `RegisterMonitor` 附带一个"信息性"的 lifecycle 类型字段,仅用于 Core 日志。同样破 PE-6。

### D4 — 单实例检测机制?

**问题**: 同时运行两次 `desktop-window-monitor.exe` 时必须恰好一个成为后台进程、另一个把"开窗请求"转发给前者然后退出。这个"原子性 + 命令转发通道"怎么实现?

**选项**:
- A(Named Pipe 单例门卫 + 指令通道,推荐): 后台进程创建 `\\.\pipe\overlay-desktop-window-monitor-singleton`(使用 `first_pipe_instance: true`);第二次启动尝试 **连接** 该 pipe: 成功 → 自己是 Launcher,发一条 `OpenWindow` 请求、等 ack、退出;连接失败且 pipe 不存在 → 自己成为后台进程、创建 pipe。一个机制同时解决门卫 + 命令转发。
- B(Named Mutex + 另建 Named Pipe): Named Mutex 判断独占性更"规范",但要**额外**一条 Named Pipe 作为命令通道,多一个机制,实现更复杂。
- C(文件锁): Windows 上文件锁的崩溃残留不可靠,不推荐。

无论哪种,都必须处理 race(两个 .exe 几乎同时启动)和"陈旧残留"(前一次 Monitor_Process 崩溃留下的 pipe/mutex 句柄)。

### D5 — 多窗口在协议层怎么表达?

**问题**: 当一个 Monitor_Process 管 N 个 HWND 窗口时,Core 看到的是 1 个 client 还是 N 个 client?这会影响 `RegisterMonitor` 的调用次数、`consumer_id`(将被重命名为 `monitor_id`)的分配粒度、`attach_consumer` 与 `per_consumer_surfaces` 的键。

**选项**:
- A(N 个 monitor_id, 一个进程, 推荐): Monitor_Process 对每个 HWND 都走一次 `RegisterMonitor` + `AttachMonitor`,Core 侧像今天这样,一个 consumer_id 对应一个 MonitorLocal surface。和今天的多进程模型等价,只是"多进程"换成"单进程多连接";`per_consumer_surfaces` 的语义和多连接度都不变。
- B(1 个 monitor_id, N 个窗口): 一个 Monitor_Process 只有一个 monitor_id,窗口管理完全在 Monitor 进程内部;一个 MonitorLocal surface 被 N 个窗口共享。协议最简洁,但破坏"每个 HWND 在客户区 (10,10) 独立显示 MonitorLocal 内容"(`hotfix-visible-render` Change-D 修的东西),不可行。
- C(混合/分层): 引入 `Monitor_Group` 概念,一个 group 下挂 N 个 `monitor_id`。过度工程,不推荐。

**下游影响**: 本 spec 是否要求 Game Bar widget 目录(`monitors/game-bar-widget/`)同步接入 detach 通知?
- E1(推荐): 本 spec 只改 `desktop-window-monitor`,Game Bar widget 不动;Game Bar widget 在后续 spec 处理(`game-bar-widget-lifecycle` 或类似)。PE-10 维持。
- E2: 本 spec 同步改 Game Bar widget 的 C# 侧接入新 detach 消息。范围扩大。

---

## Glossary

- **App**: 发送渲染命令的业务进程(过去叫 "Producer")。在本仓库中 `demo-app` 是一个 App。
- **Core**: 代理进程 `core-server`,接收所有 App 的命令、创建 DComp shared surface、把逐帧命令分发到每个 Monitor 的 per-Monitor surface。已有术语,不变。
- **Monitor_Process**: 在主机上同一时刻**唯一**运行的 `desktop-window-monitor` 包后台进程。持有零到 N 个 Monitor_Window。
- **Monitor_Window**: Monitor_Process 内的一个 HWND,用于呈现一个 Canvas 视图。
- **Monitor_ID**: Core 为每个已注册的 Monitor 通道分配的 `u32`(过去叫 `consumer_id`)。命名迁移不改 `u32` 字节布局。
- **Monitor_Launcher**: 当用户在已有 Monitor_Process 运行时再次启动 `desktop-window-monitor.exe` 时产生的**瞬态**进程。职责: 连上 Monitor_Process 的单例通道、转发一个"开新窗口"请求、打一行日志、退出。不向 Core 注册。
- **Singleton_Channel**: Monitor_Process 用来识别"是否已有后台实例"以及接收新开窗请求的本机进程间通道(具体机制见 §开放决策 D4)。
- **Canvas**: App 拥有的逻辑画布。`hotfix-visible-render` 与 `animation-and-viewport-fix` 已定义,本 spec 不改其语义,仅把 `per_consumer_surfaces` 字段重命名为 `per_monitor_surfaces`。
- **Detach Notification**: Core 向所有仍 attach 到某 App 的 Canvas 的 Monitor 发出的"该 App 已断开"通知(具体协议形态见 §开放决策 D2)。本 Glossary 故意不钦定 opcode / 字段布局。
- **Transient_Pipe_Error**: Monitor 与 Core 之间的 IPC 在**没有**前导 Detach Notification 的情况下出错。沿用 `hotfix-visible-render` Change-B 的 `"reconnecting..."` 标题路径。
- **Preservation_Oracle**: `core-server/tests/preservation_oracles/` 目录下的 oracle 文件。`control_plane_bytes.bin` 与 `control_plane_monitor_local_surface_bytes.bin` 是字节级 oracle,本 spec 对 opcode `0x0001..=0x0007` 的重命名必须保证这两个 oracle bit-identical。

---

## Requirements

### Requirement 1: 协议层与内部结构的统一重命名 (producer/consumer → app/monitor)

**User Story:** 作为阅读代码的开发者,我希望源码中的协议符号、IPC 结构体、字段、方法、日志文案与用户侧的 "app / core / monitor" 三层命名一致,这样读 `core-server/src/ipc/` 与 `server_task.rs` 时不需要在新旧术语间做心智翻译。

#### Acceptance Criteria

1. THE `core-server/src/ipc/protocol.rs` SHALL expose the renamed identifiers `OP_REGISTER_APP` / `OP_REGISTER_MONITOR` / `OP_ATTACH_MONITOR` / `ControlMessage::RegisterApp` / `ControlMessage::RegisterMonitor` / `ControlMessage::AttachMonitor`,并保持 `const` 的 `u16` 数值与对应消息变体的字段数、字段类型、字段顺序与现状一致 (只是 `consumer_id: u32` 字段改名为 `monitor_id: u32`)。
2. THE `ControlMessage::MonitorLocalSurfaceAttached` 变体名 SHALL 保持不变(其中 "Monitor" 已经指 MonitorLocal 空间, 重命名与本 spec 的 monitor 术语对齐),其 `consumer_id` 字段 SHALL 重命名为 `monitor_id`。
3. FOR ALL `ControlMessage` values `m` of every existing variant (`RegisterApp` / `RegisterMonitor` / `CreateCanvas` / `AttachMonitor` / `CanvasAttached` / `SubmitFrame` / `MonitorLocalSurfaceAttached`), `ControlMessage::decode(m.opcode(), m.payload_len_of(m), encode(m))` SHALL return `Ok(Some(m'))` such that `m` 与 `m'` 字段值逐项相等(round-trip property)。
4. WHEN `core-server/tests/preservation.rs` 被执行,THE `control_plane_bytes.bin` 与 `control_plane_monitor_local_surface_bytes.bin` 两份 oracle SHALL 与重命名后的 `encode` 输出 byte-for-byte 一致(验证 PE-6 / PE-7 不破)。
5. THE `core-server/src/ipc/server.rs` SHALL rename: struct `Producer` → `App`,struct `Consumer` → `Monitor`,`Canvas::per_consumer_surfaces` → `Canvas::per_monitor_surfaces`,`ServerState::producers` → `ServerState::apps`,`ServerState::consumers` → `ServerState::monitors`,`ServerState::next_producer_id` → `ServerState::next_app_id`,`ServerState::next_consumer_id` → `ServerState::next_monitor_id`,方法 `register_producer` → `register_app`,`register_consumer` → `register_monitor`,`remove_producer` → `remove_app`,`remove_consumer` → `remove_monitor`,`attach_consumer` → `attach_monitor`;方法参数 `consumer_id` → `monitor_id`。
6. THE `core-server/src/renderer/dcomp.rs` SHALL rename type `PerConsumerResources` → `PerMonitorResources`,以及每个 `consumer_id` 字段或参数 → `monitor_id`。
7. THE `core-server/src/server_task.rs` SHALL rename 局部变量 `producer_id` → `app_id`、`is_producer` → `is_app`、`consumer_id` → `monitor_id`,以及日志文案 `"Registered Producer with ID ..."` / `"Cleaning up Producer ..."` / `"Cleaning up Consumer ..."` / `"AttachConsumer received but client is not a registered producer"` / `"CreateCanvas received but client is not a registered producer"` / `"CreateCanvas created ID {} for Producer {}"` 至其 App/Monitor 对应文案,保持每条日志的 `{}` 占位符个数与位置与原来一致。
8. THE `monitors/desktop-window/Cargo.toml` SHALL 移除 `[[bin]] name = "desktop-demo-producer"` 的整个条目(含其 `TODO(canvas-monitor-lifecycle rename spec)` 注释行),AND SHALL 把 `desktop-window-monitor` 的 `[[bin]]` 条目的 `path` 从 `"src/bin/consumer.rs"` 指向 `"src/bin/monitor.rs"`(用户侧 bin 名 `desktop-window-monitor` 保持不变, 使 `END-TO-END-TESTING.md` 里的 `cargo run -p desktop-window-monitor --bin desktop-window-monitor` 命令行 byte-identical)。
9. THE 文件 `monitors/desktop-window/src/bin/consumer.rs` SHALL 被重命名为 `monitors/desktop-window/src/bin/monitor.rs`,AND 该文件内的 window class name `"OverlayDesktopMonitor"` 与标题字符串 `"Desktop Monitor - ..."` SHALL 保持不变(这些已是"monitor"新命名)。
10. THE 以下属于 spike 阶段、已被 `core-server/src/ipc/` 完全取代的文件 SHALL 从 workspace 中移除: `monitors/desktop-window/src/dcomp.rs`、`monitors/desktop-window/src/proto.rs`、`monitors/desktop-window/src/bin/producer.rs`;AND THE `monitors/desktop-window/src/lib.rs` SHALL 缩减为仅暴露 `src/bin/monitor.rs` 和 `src/title.rs` 实际引用的 items。
11. THE `monitors/desktop-window/README.md` SHALL 把所有用户可见的 "producer" / "consumer" 术语替换为 "app" / "monitor",AND SHALL 移除 spike 期的 `desktop-demo-producer` 运行指令。
12. THE `END-TO-END-TESTING.md` SHALL 把所有用户可见散文里的 "producer" / "consumer" 替换为 "app" / "monitor",AND SHALL 保持每条 `cargo run` 命令行字节不变(因为用户侧 bin 名 `demo-app` / `desktop-window-monitor` 在 `hotfix-visible-render` 就已经是新名字)。
13. THE `hotfix-visible-render/` 与 `animation-and-viewport-fix/` 两个 spec 目录下的文档 SHALL 不被本 spec 修改;其历史中的 "Producer" / "Consumer" 引用作为历史记录保留。
14. IF 开放决策 D1 选择 A(推迟)THEN THE `core-server/src/renderer/painter.rs` SHALL 不新增文字渲染命令,AND THE `core-server/src/ipc/cmd_decoder.rs` SHALL 不新增 `CMD_DRAW_TEXT` opcode。

### Requirement 2: App-Monitor 生命周期联动

**User Story:** 作为运行 overlay 栈做开发与测试的工程师,我希望当我关掉 App(不论是有序退出还是进程异常)时,所有独立的 Monitor 进程自动清退,这样我不用每次测试后手动杀掉残留的 `desktop-window-monitor.exe`;同时对于作为宿主一部分的 Monitor(Game Bar widget),其宿主 UWP 进程不会被本机制强杀。

#### Acceptance Criteria

1. WHEN 一个已 `RegisterApp` 的 App 的 Core 侧 pipe 到达 EOF 或返回 fatal I/O 错误,THE Core SHALL,在清理 `per_monitor_surfaces` 与 `apps` 条目之前,向每一个仍持有该 App 所拥有 Canvas 的 attach 的 Monitor 发出**恰好一条** Detach Notification(具体协议形态由开放决策 D2 决定);已经失效 / 先于 App 断开的 Monitor pipe 不算"仍持有"。
2. WHEN 一个 Monitor 收到 Detach Notification AND 该 Notification 标识的 `app_id` 匹配其某些 attached Canvas 的 owner,THE Monitor SHALL 在 500 ms 内销毁所有对应该 `app_id` 的 Monitor_Window;对应其他 App 的 Monitor_Window SHALL 不受影响。
3. WHERE Monitor_Process 被实现为"独立进程"的 lifecycle kind(例如 `desktop-window-monitor.exe`),THE Monitor_Process SHALL 在其最后一个 Monitor_Window 被销毁后 2 秒内以状态码 0 退出,AND SHALL 释放 Singleton_Channel;lifecycle kind 的判定策略由开放决策 D3 决定。
4. WHERE Monitor 被实现为"宿主进程的一部分"的 lifecycle kind(例如 Game Bar widget),THE Monitor SHALL 关闭其 Monitor_Window 并向宿主 UI 呈现一个"waiting for app"状态,AND SHALL 不终止宿主进程。
5. IF Transient_Pipe_Error 发生(即 Monitor 侧 pipe 出错 AND 该次出错**没有**前导 Detach Notification),THEN THE Monitor SHALL 对每个受影响的 Monitor_Window 执行 `hotfix-visible-render` Change-B 的 `AttachState::Reconnecting` 标题更新路径,AND SHALL 不销毁 Monitor_Window。
6. WHILE Monitor 处于 Transient_Pipe_Error 引发的 reconnecting 状态,THE Monitor SHALL 每 500 ms 至 2000 ms 之间(具体退避间隔留给 design)尝试重连到 Core;每次失败记录一条 debug 日志;重连成功后 SHALL 重新发起 `RegisterMonitor` 并恢复全部 attached Monitor_Window。
7. WHEN Core 进程自身被终止(例如 Ctrl+C 或被管理员杀进程),THE Core SHALL 在关闭 App 侧 pipe 之前关闭 Monitor 侧 pipe 的行为**不**被本 spec 规定(因为 Core 已死,无法运行协议);因此 Monitor 侧此场景表现为 Transient_Pipe_Error + 持续重连失败。Monitor SHALL 在连续 N 次重连失败(N 的具体值留给 design,但不小于 3 次、不大于 20 次)后,对每个独立进程 lifecycle kind 的 Monitor_Window 视为生命周期结束并走条款 3 的清退路径。
8. WHEN 中途连接场景发生(Monitor_Process 启动时 Core 已经接受了一个或多个 App 的 `RegisterApp` + `CreateCanvas`),THE Monitor_Process SHALL 通过现有 `register_monitor` 自动 attach 到所有已存在 Canvas 的行为继续工作(见 `core-server/src/ipc/server.rs::register_consumer` 当前实现中对 `self.canvases.keys()` 的遍历 auto-attach), AND SHALL 对每个 attach 回来的 Canvas 收到 `CanvasAttached` + `MonitorLocalSurfaceAttached`,使每个 Monitor_Window 能正常显示 cyan 徽章与 FPS 条(保留 `hotfix-visible-render` Change-D 的端到端可见性)。
9. IF Core 在发出 Detach Notification 时某个 Monitor 的写队列已满或其 pipe 已关闭,THEN THE Core SHALL 记录一条 warn 日志并继续清理自身状态;Core 的清理动作 SHALL 不因 Monitor 侧发送失败而阻塞或崩溃。
10. WHEN 一个 Monitor 在收到 Detach Notification 期间同时有一条正在进行的 `SubmitFrame` 处理,THE Monitor SHALL 优先把当前帧画完(不销毁活跃的 DComp 资源),然后在下一帧边界执行 Monitor_Window 销毁;不得出现"帧渲染到一半,HWND 已销毁"导致的 DComp use-after-release。

### Requirement 3: Monitor 单实例 + 多窗口

**User Story:** 作为用户,我希望运行 `desktop-window-monitor.exe` 永远打开"再一个窗口",而不是再开一个背景进程,这样任意时刻我只有一个 monitor 后台进程在管全部窗口。

#### Acceptance Criteria

1. WHEN `desktop-window-monitor.exe` 被启动 AND 主机上当前没有活跃 Monitor_Process,THE 被启动的进程 SHALL 成为 Monitor_Process: 建立 Singleton_Channel、创建一个初始 Monitor_Window、通过现有协议向 Core 注册(走 `RegisterMonitor` + auto-attach),然后进入消息循环继续运行。
2. WHEN `desktop-window-monitor.exe` 被启动 AND 主机上已有活跃 Monitor_Process AND Singleton_Channel 在 1000 ms 内接受连接,THE 被启动的进程 SHALL 成为 Monitor_Launcher: 连接 Singleton_Channel、发送"开新窗口"请求、等待应答、向 stdout 打印**恰好一行**日志 `"forwarded open-window request to existing monitor-process (pid <N>), exiting"` 其中 `<N>` 为后台进程 PID,然后以状态码 0 在 2 秒内退出;Monitor_Launcher SHALL 不向 Core 注册。
3. IF Singleton_Channel 在主机上存在残留(例如上一个 Monitor_Process 崩溃留下的句柄)但无法在 1000 ms 内接受连接,THEN THE 被启动的进程 SHALL 把该场景视作"无活跃后台进程"并走条款 1 的路径成为 Monitor_Process,AND SHALL 向 stderr 打印一行 `"stale singleton channel detected, taking over"`。
4. WHEN Monitor_Process 收到 Monitor_Launcher 转发的"开新窗口"请求,THE Monitor_Process SHALL 创建一个新的 Monitor_Window,走协议层 `RegisterMonitor` + auto-attach 路径注册为一个新的 Monitor_ID(一个 HWND 对应一个 Monitor_ID — 见开放决策 D5),AND SHALL 在同一连接上向 Monitor_Launcher 回送 ack 响应(含自身 PID),然后继续其他请求。
5. WHERE 开放决策 D5 选择 A(N 个 monitor_id, 一个进程),THE `core-server/src/ipc/server.rs::register_monitor` 的 auto-attach 行为 SHALL 对每个新 monitor_id 为每个已存在 Canvas 创建独立的 `PerMonitorResources` 条目,保留 per-Monitor MonitorLocal surface 独立性(PE-9 / `multi_consumer_independence.txt` oracle 的独立性不变)。
6. THE Singleton_Channel 的协议 SHALL 与 Core 控制平面**完全隔离**: 不共享 opcode 空间、不共享 pipe 名、不出现在 `control_plane_bytes.bin` / `control_plane_monitor_local_surface_bytes.bin` 两份 oracle 的任何字节中。
7. WHEN Monitor_Process 因任何原因退出(Req 2 条款 3 的最后一个 Monitor_Window 销毁、Ctrl+C、进程异常终止),THE Monitor_Process SHALL 释放 Singleton_Channel 句柄,使下一次 `desktop-window-monitor.exe` 启动能走条款 1 的"初始后台进程"路径。
8. IF 两个 `desktop-window-monitor.exe` 几乎同时启动(race),THEN THE 实现 SHALL 保证**恰好一个**进程最终持有 Singleton_Channel,另一个 deterministically 走条款 2 或条款 3 的路径;不得出现"两个都成为 Monitor_Process 各自独立 register"或"两个都认为自己是 Launcher 互相等 ack 死锁"。
9. WHERE 主机上有多个已 `RegisterApp` 的 App 且 Monitor_Launcher 转发一个新开窗请求时,THE Monitor_Process SHALL 对新 Monitor_Window 通过现有协议 `attach_monitor` 路径 attach 到所有已存在 Canvas(等同于 `register_monitor` 的 auto-attach 行为)。
10. WHEN Monitor_Process 的最后一个 Monitor_Window 被用户通过 WM_CLOSE 关闭 AND 该关闭**不**是由 Detach Notification 触发的,THE Monitor_Process 的退出策略 SHALL 与条款 3 一致(最后一个窗口销毁后 2 秒内退出);也就是说,用户手动关完所有窗口等价于"准备好清退后台进程"。

### Requirement 4: 既有绿色基线与端到端行为的保留

**User Story:** 作为开发者,我希望本 spec 的落地不回退 `animation-and-viewport-fix` + `hotfix-visible-render` 累积下来的 137 条绿色测试与端到端可见性,这样重命名与 lifecycle 改动是"安全的增量"。

#### Acceptance Criteria

1. WHEN `cargo test -p core-server --test preservation` 被执行,THE 22 条 preservation 测试 SHALL 全部通过,AND 不修改 `core-server/tests/preservation_oracles/` 下任何字节级 oracle 文件(`control_plane_bytes.bin`、`control_plane_monitor_local_surface_bytes.bin`、`world_only_hashes.txt`、`high_rate_bounds.txt`、`desktop_window_attach_trace.txt`)。(PE-1)
2. WHEN `cargo test -p core-server --test bug_condition_exploration` 被执行,THE 2 条 exploration 测试 SHALL 全部通过。(PE-2)
3. WHEN `cargo test -p core-server --test hotfix_visible_render_exploration` 被执行,THE `hotfix-visible-render` 的 exploration 测试 SHALL 全部通过。
4. WHEN `cargo test -p core-server --lib` 被执行,THE 26 条 core-server 库单元测试 SHALL 全部通过。(PE-3)
5. WHEN 完整 workspace 的 renderer 测试套件被执行,THE 87 条 renderer 测试 SHALL 全部通过。(PE-4)
6. WHEN `END-TO-END-TESTING.md` 的 case 1–3 被端到端跑过,THE 橙色块 SHALL 继续无窗口事件地连续滑动。(PE-5 / `animation-and-viewport-fix` 缺陷 A)
7. WHEN `END-TO-END-TESTING.md` 的 case 4–5(两个 `desktop-window-monitor` 窗口同时 attach)被端到端跑过,THE 每个 Monitor_Window 客户区 (10, 10) 处 SHALL 继续显示 cyan 徽章与 FPS 颜色条。(保留 `hotfix-visible-render` Change-D3 `dcomp_dev.Commit()` 在 `SetRoot` 之后)
8. `control_plane_bytes.bin` oracle SHALL 在本 spec 落地后对 opcode `0x0001..=0x0006` 保持 byte-identical。(PE-6)
9. `control_plane_monitor_local_surface_bytes.bin` oracle SHALL 在本 spec 落地后对 opcode `0x0007` 保持 byte-identical。(PE-7)
10. WHEN 两个 Monitor_Window attach 到同一 App 所拥有的不同 Canvas,THE 一个 Monitor_Window 的 MonitorLocal surface 状态 SHALL 不影响另一个。(PE-9 / `multi_consumer_independence.txt` oracle;允许把 oracle 文件名重命名为 `multi_monitor_independence.txt`,但文件**内容** SHALL 字节不变以便 PBT D 继续通过)
11. THE `monitors/game-bar-widget/` 目录是否被本 spec 触及 SHALL 由开放决策 D5(下游影响 E1 / E2)锁定;WHERE 选择 E1,THE `monitors/game-bar-widget/` 目录 SHALL 被完全不动,保留 PE-10。
12. THE `hotfix-visible-render` Change-B 的 `set_window_title` / `format_window_title` / `AttachState` 路径 SHALL 继续存在于 `monitors/desktop-window/src/bin/monitor.rs`(post-rename)与 `monitors/desktop-window/src/title.rs` 中,`"reconnecting..."` 标题 SHALL 仅在 Req 2 条款 5 描述的 Transient_Pipe_Error 场景触发。
13. THE `hotfix-visible-render` Change-D3 的 `dcomp_dev.Commit()` 调用(紧跟 `target.SetRoot(&root)` 之后)SHALL 不被本 spec 移除。
14. THE 所有 `#[test]` 函数 / `mod tests` / proptest `proptest! { ... }` 块内的**测试逻辑** SHALL 不被本 spec 修改(只允许随着结构体/方法重命名对测试代码做机械替换);任何测试的 oracle 值、输入分布、assertion 比较面 SHALL 保持与今天等价。

### Requirement 5: 范围边界(scope negatives)

**User Story:** 作为在本 spec review 里保持注意力的开发者,我希望 explicit 地列出"本 spec 不做的事情",这样 design 阶段不会因"顺便 ..."而把范围撑爆。

#### Acceptance Criteria

1. IF 开放决策 D1 选择 A(推迟),THEN THE `core-server/src/renderer/painter.rs` SHALL 不新增 `CMD_DRAW_TEXT` 等文字 opcode,AND `demo-app.rs` SHALL 不发送文字命令,AND Monitor_Window 中的 FPS 指示 SHALL 保持为颜色方块(今天的视觉语义)。
2. IF 开放决策 D5 选择 E1,THEN `monitors/game-bar-widget/` 目录下的文件(包括其 AppPackages / App.xaml / App.xaml.cs / 各 `.cs`) SHALL 不被本 spec 修改;Game Bar widget 侧的 lifecycle 接入留给后续 spec。
3. THE 本 spec SHALL 不引入输入事件(鼠标/键盘/触摸)协议。
4. THE 本 spec SHALL 不修改 `painter-abi-v1.0` / `painter-abi-v0.7` 文档以外的渲染协议版本号,AND SHALL 不引入新的 painter ABI 版本。
5. THE 本 spec SHALL 不改变 `core-server::renderer::dcomp::CanvasResources` 与 `PerMonitorResources`(重命名后)的 buffer count / acquire 策略 / present 策略 — 这些是 `animation-and-viewport-fix` 固化下来的部分,本 spec 不触及。
6. WHEN 实现期间出现"顺便改一下"的诱惑(例如把 `demo-consumer.rs` 也重命名、把 `log.rs` 重写、把 `core-server/src/bin/server.rs` 的主函数拆小), THE 实现者 SHALL 把这些拆到独立的后续 spec,而不是塞进本 spec 的 PR 中。
