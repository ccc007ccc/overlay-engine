# Implementation Plan: Canvas-Monitor Lifecycle

## Overview

本实现计划按照 design.md §迁移顺序 的五阶段分步推进(Change-B → Change-C → Change-A → Change-D → Change-E),每阶段独立可 `cargo check --workspace` + `cargo test` 验证,回退风险最低。所有命名迁移走 `semanticRename` 逐符号跑,拒绝 `sed` 全局替换;线上字节 preservation(PE-1/PE-6/PE-7/PE-8/PE-9)通过现有 oracle + PBT 机械守死,只允许 `multi_consumer_independence.txt` 文件名改名(内容字节不变)以及 `control_plane_bytes.bin` 追加一条 `AppDetached` 样本。新增的 lifecycle 逻辑(AppDetached 广播、pending_close 帧边界、reconnect 状态机、Singleton_Channel 决策)抽成 pure fn,便于 proptest 覆盖。

## Tasks

- [ ] 1. Rust 符号统一重命名(Change-B,线上字节不变)

  - [x] 1.1 在 `core-server/src/ipc/server.rs` 用 semanticRename 重命名结构体与字段
    - 结构体:`Producer` → `App`、`Consumer` → `Monitor`
    - 字段:`Producer::canvases: Vec<u32>` → `App::canvas_ids: Vec<u32>`(值/类型不变,仅改 Rust 名);`Canvas::per_consumer_surfaces` → `Canvas::per_monitor_surfaces`
    - `ServerState` 字段:`producers` → `apps`、`consumers` → `monitors`、`next_producer_id` → `next_app_id`、`next_consumer_id` → `next_monitor_id`
    - 每改一个符号立刻 `cargo check --workspace`;禁止 `sed`,走 `semanticRename` 保证 comment 自然语义里的 "producer" 不被误伤
    - _Requirements: 1.5_

  - [ ] 1.2 在 `core-server/src/ipc/server.rs` 用 semanticRename 重命名方法与参数
    - 方法:`register_producer` → `register_app`、`register_consumer` → `register_monitor`、`remove_producer` → `remove_app`、`remove_consumer` → `remove_monitor`、`attach_consumer` → `attach_monitor`
    - 方法参数 `consumer_id` → `monitor_id`
    - 每改一个方法跑 `cargo check --workspace`
    - _Requirements: 1.5_

  - [~] 1.3 在 `core-server/src/ipc/protocol.rs` 重命名 ControlMessage 变体、字段与 opcode 常量
    - 变体:`ControlMessage::RegisterProducer { pid }` → `RegisterApp { pid }`、`RegisterConsumer { pid }` → `RegisterMonitor { pid }`、`AttachConsumer { canvas_id, consumer_id }` → `AttachMonitor { canvas_id, monitor_id }`
    - `ControlMessage::MonitorLocalSurfaceAttached` 变体名保持不变(Req 1 AC 2),仅把其 `consumer_id` 字段重命名为 `monitor_id`
    - Opcode 常量:`OP_REGISTER_PRODUCER` → `OP_REGISTER_APP`、`OP_REGISTER_CONSUMER` → `OP_REGISTER_MONITOR`、`OP_ATTACH_CONSUMER` → `OP_ATTACH_MONITOR`,数值 `0x0001`/`0x0002`/`0x0004` 保持不变
    - `encode` / `decode` 的 match arms 与结构体解构跟随变体名与字段名机械更新;产出字节完全不变
    - 运行 `cargo test -p core-server --test preservation` 验证 `control_plane_bytes.bin` / `control_plane_monitor_local_surface_bytes.bin` byte-identical
    - _Requirements: 1.1, 1.2, 4.8, 4.9_

  - [~] 1.4 在 `core-server/src/renderer/dcomp.rs` 重命名 PerConsumerResources 及其常量/字段
    - 类型:`PerConsumerResources` → `PerMonitorResources`
    - 常量:`PER_CONSUMER_MAX_DIM` → `PER_MONITOR_MAX_DIM`、`PER_CONSUMER_MIN_DIM` → `PER_MONITOR_MIN_DIM`
    - 所有 `consumer_id` 参数 → `monitor_id`
    - 日志前缀 `[PerConsumerResources]` → `[PerMonitorResources]`
    - _Requirements: 1.6_

  - [~] 1.5 在 `core-server/src/server_task.rs` 重命名局部变量与日志文案
    - 局部变量:`producer_id` → `app_id`、`is_producer` → `is_app`、`consumer_id` → `monitor_id`
    - 日志文案按 design.md §B3 表格逐条 str_replace:`"Registered Producer ..."` → `"Registered App ..."`、`"Registered Consumer ..."` → `"Registered Monitor ..."`、`"Cleaning up Producer ..."` → `"Cleaning up App ..."`、`"Cleaning up Consumer ..."` → `"Cleaning up Monitor ..."`、`"AttachConsumer received but client is not a registered producer"` → `"AttachMonitor received but client is not a registered app"`、`"CreateCanvas received but client is not a registered producer"` → `"CreateCanvas received but client is not a registered app"`、`"CreateCanvas created ID {} for Producer {}"` → `"CreateCanvas created ID {} for App {}"`、`"Attached Canvas {} to Consumer {}"` → `"Attached Canvas {} to Monitor {}"`、`"AttachConsumer error: {}"` → `"AttachMonitor error: {}"`
    - 每条日志的 `{}` 占位符个数与位置保持与原版一致(Req 1 AC 7 硬约束)
    - _Requirements: 1.7_

  - [~] 1.6 写 rename-and-structure 静态检查测试
    - 新文件 `core-server/tests/rename_and_structure_checks.rs`
    - 断言 `protocol.rs` 含 `OP_REGISTER_APP` / `OP_REGISTER_MONITOR` / `OP_ATTACH_MONITOR` / `ControlMessage::RegisterApp` / `RegisterMonitor` / `AttachMonitor`,且不含 `OP_REGISTER_PRODUCER` / `OP_REGISTER_CONSUMER` / `OP_ATTACH_CONSUMER` / `RegisterProducer` / `RegisterConsumer` / `AttachConsumer`
    - 断言 `server.rs` 含 `ServerState::apps` / `ServerState::monitors` / `per_monitor_surfaces`,不含 `producers` / `consumers` / `per_consumer_surfaces`
    - 断言 `dcomp.rs` 含 `PerMonitorResources`,不含 `PerConsumerResources`
    - 断言 `server_task.rs` 每条新日志文案存在,旧文案不存在,每条 `{}` 占位符数量等于原版
    - _Requirements: 1.1, 1.2, 1.5, 1.6, 1.7_

- [~] 2. Checkpoint — Change-B 完成,preservation 绿
  - 运行 `cargo test -p core-server --test preservation`(PE-1,22 条)、`cargo test -p core-server --test bug_condition_exploration`(PE-2,2 条)、`cargo test -p core-server --lib`(PE-3,26 条)、`cargo test --workspace`(renderer PE-4,87 条)
  - 断言 `control_plane_bytes.bin` / `control_plane_monitor_local_surface_bytes.bin` / `world_only_hashes.txt` / `high_rate_bounds.txt` / `desktop_window_attach_trace.txt` / `multi_consumer_independence.txt` 字节 identical
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 3. Spike 清理与 Monitor bin 搬家(Change-C)

  - [~] 3.1 用 smartRelocate 把 `monitors/desktop-window/src/bin/consumer.rs` 搬到 `monitors/desktop-window/src/bin/monitor.rs`
    - smartRelocate 自动更新所有 import / path 引用
    - **不改文件内容**:窗口类名 `"OverlayDesktopMonitor"` 与标题字符串 `"Desktop Monitor - ..."` 保持不变(已是 monitor 新命名,Req 1 AC 9)
    - `println!` 的 `[desktop-monitor]` 前缀保持不变
    - _Requirements: 1.9_

  - [~] 3.2 更新 `monitors/desktop-window/Cargo.toml` bin 条目
    - 把 `[[bin]] name = "desktop-window-monitor"` 条目的 `path` 从 `"src/bin/consumer.rs"` 改为 `"src/bin/monitor.rs"`(`name` 保持 `desktop-window-monitor` 不变,保证 `END-TO-END-TESTING.md` 的 `cargo run -p desktop-window-monitor --bin desktop-window-monitor` byte-identical)
    - 删除整段 `desktop-demo-producer` `[[bin]]` 条目,以及上方 `# TODO(canvas-monitor-lifecycle rename spec): rename desktop-demo-producer -> desktop-demo-app` 注释行
    - 运行 `cargo check -p desktop-window-monitor --bin desktop-window-monitor`
    - _Requirements: 1.8_

  - [~] 3.3 删除 spike 阶段遗留的三个文件
    - `monitors/desktop-window/src/dcomp.rs`
    - `monitors/desktop-window/src/proto.rs`
    - `monitors/desktop-window/src/bin/producer.rs`
    - _Requirements: 1.10_

  - [~] 3.4 缩减 `monitors/desktop-window/src/lib.rs`
    - 删除 `pub mod dcomp;` 与 `pub mod proto;`
    - 保留 `pub mod title;`(被 `bin/monitor.rs` 使用 + 有独立单元测试)
    - 基于 `cargo check` 的结果决定是否保留 `PIPE_PATH` / `handle_to_u64` / `u64_to_handle` helpers(被引用则保留,否则删除)
    - _Requirements: 1.10_

  - [~] 3.5 更新 `monitors/desktop-window/README.md` 与 `END-TO-END-TESTING.md` 散文术语
    - README.md:全部用户可见的 "producer" / "consumer" 替换为 "app" / "monitor",删除 spike 期 `desktop-demo-producer` 运行指令
    - END-TO-END-TESTING.md:散文里 "producer" / "consumer" 替换为 "app" / "monitor";所有 `cargo run ...` 命令行字节**不变**(用户侧 bin 名 `demo-app` / `desktop-window-monitor` 在 `hotfix-visible-render` 已稳定)
    - _Requirements: 1.11, 1.12_

  - [~] 3.6 更新 `core-server/tests/hotfix_visible_render_exploration.rs` 静态字面量
    - sub-property 1a 的 `cargo_bin_entry` path 期望从 `"src/bin/consumer.rs"` 改为 `"src/bin/monitor.rs"`
    - sub-property 1c 确认 `cargo_bin_entry("core-server", "demo-app")` 继续存在(已由 `hotfix-visible-render` 落地,本 spec 仅验证)
    - 1b-runtime / 1d-runtime 继续 `#[ignore]`
    - _Requirements: 4.3_

- [~] 4. Checkpoint — Change-C 完成,workspace 编译并 test 绿
  - 运行 `cargo check --workspace` + `cargo test --workspace`
  - `git diff --stat monitors/game-bar-widget/` 为空(PE-10,Req 4 AC 11 + 5.2)
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 5. 新增 AppDetached opcode(Change-A2)

  - [~] 5.1 在 `core-server/src/ipc/protocol.rs` 新增 AppDetached 协议元素
    - 加常量 `pub const OP_APP_DETACHED: u16 = 0x0008;`
    - 加变体 `ControlMessage::AppDetached { app_id: u32, reason: u8 }`(payload 5 字节:`u32 LE app_id` + `u8 reason`)
    - 加 `#[repr(u8)] pub enum AppDetachReason { GracefulExit = 0, IoError = 1, Other = 2 }`
    - 在 `encode` 的 match arm 写入 `OP_APP_DETACHED` header + 5 字节 payload;在 `decode` 的 match 追加 `OP_APP_DETACHED` arm(payload 不足 5 字节返回 `ProtocolError::BufferTooSmall`)
    - unknown-opcode 降级路径**不改**(已在 `hotfix-visible-render` task 3.3 做成 "skip payload + warn")
    - _Requirements: 1.1, 1.3_

  - [~] 5.2 写 Property 1 PBT — Control-plane round-trip 含 AppDetached + oracle capture-or-verify
    - **Property 1: Control-plane round-trip + oracle byte-identity(含 AppDetached 新样本)**
    - **Validates: Requirements 1.1, 1.2, 1.3, 1.4, 4.1, 4.8, 4.9**
    - 扩展 `core-server/tests/preservation.rs` 的 PBT A proptest strategy,使其生成的 `ControlMessage` 包含 `AppDetached { app_id, reason }`
    - 断言 `ControlMessage::decode(opcode, header.payload_len, encode(m))` 返回 `Ok(Some(m'))` 且 `m` 与 `m'` 字段逐项相等(round-trip)
    - 对 `0x0001..=0x0007` 字节子集继续对比现有 oracle;**首次运行**时把 `AppDetached` 字节样本 append 到 `control_plane_bytes.bin`(capture-or-verify 语义),之后运行 byte-identical
    - 新测试 `app_detached_oracle_sample_appended_byte_identical` 固化 design.md §A2 的字节样本(`app_id=0x00000042`, `reason=1` → `4C 52 56 4F 01 00 08 00 05 00 00 00 42 00 00 00 01`)
    - _Requirements: 1.1, 1.3, 4.1, 4.8, 4.9_

- [ ] 6. Core-side 生命周期广播(Change-D1)

  - [~] 6.1 在 `core-server/src/server_task.rs::handle_client` 跟踪 detach reason
    - 在 read_exact 循环之外声明局部 `let mut detach_reason = AppDetachReason::GracefulExit;`
    - 任何 `return Err(e.into())` 或 pipe 出错路径之前先把它设为 `AppDetachReason::IoError`
    - 正常 `break`(`bytes_read == 0`)保持 `GracefulExit`
    - _Requirements: 2.1_

  - [~] 6.2 在 disconnect cleanup 路径插入 AppDetached 广播(remove_app 之前)
    - 在 `if let Some((id, is_app)) = client_id { ... if is_app { ... } }` 的 `state.remove_app(id)` **之前**,取一把 `SERVER_STATE.write()`,按 `apps[id].canvas_ids → canvases[cid].per_monitor_surfaces.keys() → monitors[mid].tx` 遍历反向索引,对每个受影响的 `monitor_id` 通过 `UnboundedSender<ControlMessage>` 发**恰好一条** `ControlMessage::AppDetached { app_id: id, reason: detach_reason as u8 }`
    - `monitor.tx.send` 返回 `Err` 时只 `eprintln!` warn 并继续遍历,不 panic、不阻塞 `remove_app`(Req 2 AC 9)
    - 广播完成后再调用 `state.remove_app(id)` 走现有 drop 链(Canvas → per_monitor_surfaces → PerMonitorResources → 释放 COM + NT handle)
    - _Requirements: 2.1, 2.9_

  - [~] 6.3 写 Property 2 PBT — AppDetached 广播正确性 + 健壮性
    - **Property 2: AppDetached 广播对所有 attached Monitor 恰好一次且健壮**
    - **Validates: Requirements 2.1, 2.9**
    - 新文件 `core-server/tests/lifecycle_integration.rs`(或扩展现有 integration 测试),用 `proptest!` 生成随机 `ServerState` 快照(随机 apps / canvases / monitors 拓扑 + 一部分 monitor 的 `tx` 预先关闭)
    - 断言:对每一个 `m_id ∈ { mid : ∃ cid ∈ apps[app_id].canvas_ids, mid ∈ canvases[cid].per_monitor_surfaces.keys() }`,若 `monitors[m_id].tx` 仍有效则其收件箱恰好收到 1 条 `AppDetached { app_id, reason }`
    - 断言:清理路径对已失效 `tx` 的 send 失败不 panic、不 deadlock、不阻塞 `remove_app`
    - 断言:清理完成后 `apps[app_id]` / 对应 `canvases[cid]` / `per_monitor_surfaces` 条目全部被回收
    - _Requirements: 2.1, 2.9_

- [ ] 7. Monitor-side 生命周期主循环(Change-D2/D3/D4/D5)

  - [~] 7.1 定义 MonitorLifecycleKind 枚举
    - 新增 `monitors/desktop-window/src/lib.rs`(或新 module `lifecycle.rs`)中:
      ```rust
      #[derive(Debug, Clone, Copy, PartialEq, Eq)]
      pub enum MonitorLifecycleKind { Standalone, Hosted }
      ```
    - 在 `monitors/desktop-window/src/bin/monitor.rs` 顶部硬编 `const MONITOR_LIFECYCLE_KIND: MonitorLifecycleKind = MonitorLifecycleKind::Standalone;`
    - _Requirements: 2.3, 2.4_

  - [~] 7.2 抽出 `MonitorWindow` struct 与 Vec 主状态
    - 在 `monitors/desktop-window/src/bin/monitor.rs`(或提到 `src/monitor_window.rs`)定义 design.md §E6 的 `struct MonitorWindow { hwnd, monitor_id, canvas_id, owner_app_id: Option<u32>, pipe_writer, target, world_visual, ml_visual, viewport, pending_close: Arc<AtomicBool>, dcomp_dev }`
    - 在 rustdoc 里记录 `owner_app_id` 保持 `None` 的已知限制(`CanvasAttached` 线上字节不含 `app_id`,见 design.md §E6 caveat)
    - 把主状态从 `main` 的局部变量改为 `Vec<MonitorWindow>`
    - _Requirements: 2.2, 2.10, 3.5_

  - [~] 7.3 重写 `monitors/desktop-window/src/bin/monitor.rs` 主循环为 tokio + Win32 混合
    - 用 `#[tokio::main(flavor = "current_thread")]`
    - 主循环 `tokio::select!` 三路:(a) 读每个 `MonitorWindow` 的 pipe reader(选 `FuturesUnordered<_>` 或 per-window `spawn_local` + `mpsc` fan-in,在实现注释给选型理由);(b) 非阻塞 `PeekMessageW(PM_REMOVE)` 一次 tick 后 yield;(c) Singleton_Channel accept(task 9.3 填充)
    - pipe reader 用 `MessageHeader::decode` + `ControlMessage::decode`;收到 `AppDetached { app_id, .. }` → 见 task 7.4
    - pipe 读出错(无前导 `AppDetached`)→ 切 Transient_Pipe_Error 路径,见 task 7.5
    - 中途连接(Req 2 AC 8)auto-attach 行为不改 —— 沿用 `register_monitor` 对 `canvases.keys()` 的遍历
    - _Requirements: 2.2, 2.5, 2.8, 2.10_

  - [~] 7.4 实现 pending_close + 帧边界清 shutdown
    - 收到 `AppDetached` 主循环迭代 `windows`,对每个受影响窗口执行 `w.pending_close.store(true, Ordering::SeqCst)`(按 design.md §E6 简化策略,`owner_app_id == None` 时全部标)
    - 每 tick 结束后 `cleanup_pending_close_windows`:用 `windows.retain(|w| ...)` 模式过滤;销毁时调用 `DestroyWindow(hwnd)` 并 drop 该 `MonitorWindow` 的 DComp 资源(`ViewportState` 在 `GWLP_USERDATA` 里 drop)
    - `wnd_proc` 的 `WM_DESTROY` 路径**不再** `PostQuitMessage(0)`,改为让主循环决定进程退出(只有最后一个窗口销毁后走 grace period 才退)
    - 最后一个 `MonitorWindow` 销毁且 `MONITOR_LIFECYCLE_KIND == Standalone` → `tokio::time::sleep(Duration::from_secs(2))` grace period,期间若未收到新 OpenWindow 则 `break` 退进程并释放 Singleton_Channel
    - `MONITOR_LIFECYCLE_KIND == Hosted` 分支本 spec 不落地,留纯函数 stub + 单元测试覆盖 Req 2 AC 4
    - Req 4 AC 13 守护:`dcomp_dev.Commit()` 紧跟 `target.SetRoot(&root)?` 的那段代码字节不改
    - _Requirements: 2.2, 2.3, 2.4, 2.10, 3.7, 4.13_

  - [~] 7.5 实现 reconnect 退避(Transient_Pipe_Error 路径)
    - 常量 `const RECONNECT_BACKOFF_MS: &[u64] = &[500, 1000, 2000]; const RECONNECT_MAX_ATTEMPTS: u32 = 10;`
    - 每次 attempt:delay = `RECONNECT_BACKOFF_MS[min(attempt, len-1)]` + 0~200ms 随机 jitter;对每个存活的 `MonitorWindow` 先 `set_window_title(w.hwnd, AttachState::Reconnecting)`(复用 `hotfix-visible-render` Change-B 的 `format_window_title` + `title.rs` 路径,一条字节不改)
    - 尝试 `ClientOptions::new().open(PIPE_NAME)`。成功 → 对每个窗口重发 `RegisterMonitor`、接收 `CanvasAttached` + `MonitorLocalSurfaceAttached`、把标题回切到 `AttachState::Attached { canvas_id, ml }`
    - attempts ≥ `RECONNECT_MAX_ATTEMPTS` → 走 Standalone lifecycle 清退:标所有窗口 `pending_close = true`,由 task 7.4 的 cleanup 路径处理
    - 连续 debug 日志记录每次失败(Req 2 AC 6)
    - _Requirements: 2.5, 2.6, 2.7, 4.12_

  - [~] 7.6 写 Property 3 PBT — 帧边界安全 shutdown
    - **Property 3: 帧边界安全 shutdown(pending_close + no-destroy-mid-frame)**
    - **Validates: Requirements 2.2, 2.10**
    - 抽 pure fn `apply_app_detached_events(W: &mut [MonitorWindow], events: &[AppDetachedEvent])` 与 `fn should_destroy_now(w: &MonitorWindow) -> bool`
    - proptest 随机生成 `MonitorWindow` 集合 + `AppDetached` 事件序列 + `in_frame` 布尔序列
    - 断言:`w.pending_close == true` 当且仅当事件中存在匹配 `w.owner_app_id`(或 `owner_app_id == None` 时被全部标);`w.in_frame == true` 时 `should_destroy_now(w) == false`;`should_destroy_now(w) == true` 只当 `w.pending_close == true` 且 `w.in_frame == false`
    - 放 `monitors/desktop-window/src/bin/monitor.rs` 内 `#[cfg(test)] mod tests`(或 `lifecycle_integration.rs`)
    - _Requirements: 2.2, 2.10_

  - [~] 7.7 写 Property 4 PBT — Reconnect 状态机终态正确性
    - **Property 4: Reconnect 状态机的终态正确性**
    - **Validates: Requirements 2.5, 2.6, 2.7**
    - 抽 pure fn `reconnect_step(state: &mut ReconnectState, outcome: ReconnectOutcome)`,`ReconnectOutcome ∈ { Success, Failed }`
    - proptest 生成随机 outcome 序列(不含 `AppDetached` 前导):
      - 子断言 (1):中间任意时刻 `state.windows` 每个窗口 `pending_close == false`
      - 子断言 (2):`outcomes` 最后一项为 `Success` → 每个窗口被标记为已重发 `RegisterMonitor` 并收到 `CanvasAttached`(+ optionally `MonitorLocalSurfaceAttached`)
      - 子断言 (3):`outcomes` 包含连续 `RECONNECT_MAX_ATTEMPTS == 10` 次 `Failed` 且 `MONITOR_LIFECYCLE_KIND == Standalone` → 序列结束后每个窗口 `pending_close == true`
    - _Requirements: 2.5, 2.6, 2.7_

- [~] 8. Checkpoint — Change-D 完成,runtime 路径不回归
  - 运行 `cargo test --workspace`(全绿)
  - 机械更新现有 PBT A/A'/B/C/D 中引用到的 Rust 符号(如果还有残留),oracle 字节不变
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 9. Singleton_Channel 单实例 + 多窗口(Change-E)

  - [~] 9.1 新建 `monitors/desktop-window/src/singleton.rs`,定义协议 + 纯状态机
    - 常量 `pub const SINGLETON_PIPE_NAME: &str = r"\\.\pipe\overlay-desktop-window-monitor-singleton";`
    - 帧结构:`u16 LE opcode + u32 LE len + payload`;Request opcode `SINGLETON_OP_OPEN_WINDOW = 0x0101`(payload `u32 LE target_canvas_id`);Response opcodes `SINGLETON_OP_ACK = 0x0201`(payload `u32 LE pid + u32 LE new_monitor_id`)、`SINGLETON_OP_NACK = 0x0202`(payload `u16 LE reason + UTF-8 message`)
    - 纯类型 `SingletonRequest` / `SingletonResponse` / `SingletonState { monitor_process_pid: u32, registered_windows: Vec<MonitorWindowSnapshot> }`
    - 纯函数:
      - `fn try_become_singleton(osr: OsPipeState) -> Result<SingletonServer, TryBecomeErr>` —— `OsPipeState ∈ { NoPipe, PipeExistsAcceptsInWindow, PipeExistsStale, Race }`
      - `fn handle_singleton_request(req: SingletonRequest, state: &mut SingletonState) -> SingletonResponse`
      - `fn launcher_log_line(pid: u32) -> String` 产出 `"forwarded open-window request to existing monitor-process (pid {}), exiting"` 的字面字符串(Req 3 AC 2 的**恰好一行**)
    - 在 `monitors/desktop-window/src/lib.rs` 中 `pub mod singleton;`
    - _Requirements: 3.1, 3.2, 3.3, 3.7, 3.8_

  - [~] 9.2 重写 `monitors/desktop-window/src/bin/monitor.rs` main 入口为 try_become 分支
    - `match try_become_singleton(probe_os_state()) { Ok(srv) => run_as_monitor_process(srv).await, Err(AlreadyExists) => run_as_launcher().await, Err(Race) => retry once after 50-100ms sleep then give up with exit(1), Err(StaleHandle) => run_as_monitor_process(take_over_stale().await?).await }`
    - Race 兜底使用 `ServerOptions::new().first_pipe_instance(true).create(..)` 的 `ERROR_ACCESS_DENIED` 作为信号
    - StaleHandle 兜底:connect 1000ms 超时 + `ERROR_PIPE_BUSY`(`first_pipe_instance = true` 失败)→ 带 `first_pipe_instance = false` 再 create;stderr 打印 `"stale singleton channel detected, taking over"`
    - _Requirements: 3.1, 3.3, 3.7, 3.8_

  - [~] 9.3 把 Singleton accept 集成到主循环 tokio::select! 第三个分支
    - `req = singleton_server.accept_next_request() => { ... SingletonRequest::OpenWindow { target_canvas_id: 0 } → 在主线程创建新 HWND(post 一个 user-defined Win32 message 唤醒消息泵)→ 对新 HWND 走 `RegisterMonitor` + auto-attach,得到 reader/writer + `new_monitor_id` → push 到 `windows: Vec<MonitorWindow>` → 回送 `SINGLETON_OP_ACK { pid: std::process::id(), new_monitor_id }` }`
    - Req 3 AC 5:每个新窗口对应独立 `monitor_id`,保留 `multi_consumer_independence.txt` 独立性(本 spec 只动命名,不动 per-monitor surface 的独立性策略)
    - _Requirements: 3.4, 3.5, 3.9_

  - [~] 9.4 实现 Launcher 路径 + 用户手动关窗 grace period
    - `run_as_launcher`:connect `SINGLETON_PIPE_NAME` → 发 `SINGLETON_OP_OPEN_WINDOW { target_canvas_id: 0 }` → 读 ack(5 秒硬超时,超时则 stderr 打印 `"singleton ack timeout; assuming existing monitor-process is stuck, exiting"` + `exit(1)`) → `println!("forwarded open-window request to existing monitor-process (pid {}), exiting", pid)`(Req 3 AC 2 的**恰好一行**)→ `std::process::exit(0)` 在 2 秒内
    - `WM_CLOSE` 路径(Req 3 AC 10):用户手动关最后一个窗口等价于"准备好清退后台进程"— 标 `pending_close = true`,走与 `AppDetached` 路径相同的 cleanup + 2 秒 grace
    - _Requirements: 3.2, 3.10_

  - [~] 9.5 写 Property 5 PBT — try_become_singleton 决策表 + race 单赢家
    - **Property 5: Singleton_Channel become/take-over 决策 + race 单实例不变量**
    - **Validates: Requirements 3.1, 3.3, 3.7, 3.8**
    - 在 `monitors/desktop-window/src/singleton.rs` 的 `mod tests` 里,用 proptest 对 `OsPipeState ∈ { NoPipe, PipeExistsAcceptsInWindow, PipeExistsStale, Race }` 生成决策序列
    - 子断言:`NoPipe → Monitor_Process`;`PipeExistsAcceptsInWindow → Launcher`;`PipeExistsStale → Monitor_Process`(且 stderr 有 takeover 提示);对任意并发两个调用 `(osr_a, osr_b)`,不存在交错使得 `out_a == Monitor_Process ∧ out_b == Monitor_Process`(最多一个 winner)
    - 子断言:前序 Monitor_Process exit(正常/异常/SIGKILL)后,下一次 `try_become_singleton(NoPipe ∨ PipeExistsStale)` 必定返回 `Monitor_Process`(Req 3 AC 7)
    - _Requirements: 3.1, 3.3, 3.7, 3.8_

  - [~] 9.6 写 Property 6 PBT + Launcher 字面日志单元测试
    - **Property 6: Singleton request/response pure state machine 正确性**
    - **Validates: Requirements 3.2, 3.4**
    - proptest 随机 `SingletonRequest::OpenWindow { target_canvas_id }` + 随机 `SingletonState`:断言 `response == SingletonResponse::Ack { pid, new_monitor_id }` 且 `pid == state.monitor_process_pid`,`state.registered_windows.len() == old_len + 1`
    - `launcher_exits_within_2s_after_ack`(EXAMPLE):pure launcher fn 模拟 ack → assert `SystemTime::now() - start < 2s` 并返回 `exit_code == 0`
    - `launcher_prints_exact_forwarded_log_line`(EXAMPLE):断言 `launcher_log_line(42) == "forwarded open-window request to existing monitor-process (pid 42), exiting"`
    - _Requirements: 3.2, 3.4_

- [ ] 10. Integration 测试 + PBT D 重命名保留

  - [~] 10.1 把 `multi_consumer_independence.txt` 重命名为 `multi_monitor_independence.txt`
    - `smartRelocate core-server/tests/preservation_oracles/multi_consumer_independence.txt → core-server/tests/preservation_oracles/multi_monitor_independence.txt`
    - 文件**内容字节不变**(Req 4 AC 10),只是文件名变更
    - 更新 `preservation.rs` 里 PBT D 对 oracle 文件路径的引用
    - _Requirements: 4.10_

  - [~] 10.2 写 Property 7 PBT — 多-monitor 独立性重命名后保留
    - **Property 7: PBT D 多-monitor 独立性的重命名后保留**
    - **Validates: Requirements 2.8, 3.5, 3.9, 4.10**
    - 机械更新 PBT D strategy:随机 Monitor up/down(2..=4)+ 交错 `register_monitor` / `remove_monitor`,一个或多个 Canvas
    - 断言:每个存活 Monitor 保持 registered + attached;Monitor 退出 → 其 `per_monitor_surfaces` 条目从对应 Canvas 清除且不影响其他;App 退出 → 该 App 的 Canvas 完全回收;oracle `multi_monitor_independence.txt` 字节 identical
    - _Requirements: 2.8, 3.5, 3.9, 4.10_

  - [~] 10.3 写 lifecycle_integration.rs 的真实 mpsc 集成测试
    - `app_detached_broadcast_hits_all_attached_monitors`:stub App pipe + stub N 个 Monitor `UnboundedSender`,触发 App 断开,断言每个存活 Monitor 收件箱恰好一条 `AppDetached { app_id, reason }`
    - `app_detached_broadcast_robust_under_closed_monitor_tx`:随机关闭一部分 Monitor 的 receiver,验证 Core 不 panic、不 deadlock、清理继续
    - 放 `core-server/tests/lifecycle_integration.rs`
    - _Requirements: 2.1, 2.9_

- [~] 11. 最终 Checkpoint — 全绿 + Preservation 守界
  - 运行 `cargo test -p core-server --test preservation` / `--test bug_condition_exploration` / `--test hotfix_visible_render_exploration` / `--lib` / `cargo test --workspace`
  - `git diff --stat monitors/game-bar-widget/ .kiro/specs/hotfix-visible-render .kiro/specs/animation-and-viewport-fix` 为空(PE-10 + Req 1 AC 13 + Req 4 AC 11)
  - 断言 `control_plane_bytes.bin`(除 append 一条 `AppDetached` 样本外)/ `control_plane_monitor_local_surface_bytes.bin` / `world_only_hashes.txt` / `high_rate_bounds.txt` / `desktop_window_attach_trace.txt` / `multi_monitor_independence.txt` 内容字节 identical
  - 断言 `painter-abi-v1.0.md` / `painter-abi-v0.7.md` 字节不变(Req 5 AC 4);`core-server/src/ipc/cmd_decoder.rs` 不含 `CMD_DRAW_TEXT`(Req 1 AC 14 + Req 5 AC 1)
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- 标 `*` 的子任务为可选测试任务(property tests / 单元测试 / 集成测试),可按需跳过以走 MVP 路径;核心实现任务(不带 `*`)必须执行。
- 每个任务通过 `_Requirements: X.Y_` 追溯到 requirements.md 的具体子条款,便于追踪覆盖率。
- Change-B 的所有符号重命名都通过 `semanticRename` 逐符号执行,禁止 `sed` 全局替换(避免误伤 comment 里的自然语义 "producer")。
- 线上字节 preservation:`0x0001..=0x0007` 的 oracle 字节守死;`0x0008`(AppDetached)首次 capture 后 append 到 `control_plane_bytes.bin`。
- `multi_consumer_independence.txt` 仅允许文件名改成 `multi_monitor_independence.txt`,**内容字节不变**,保证 PBT D 继续通过。
- 新增 lifecycle / singleton 逻辑抽成 pure fn(`apply_app_detached_events`、`should_destroy_now`、`reconnect_step`、`try_become_singleton`、`handle_singleton_request`),proptest 覆盖;真实 Named Pipe / HWND / tokio 集成仅出现在 `lifecycle_integration.rs`。
- 范围边界(Req 5):不引入文字渲染(`CMD_DRAW_TEXT`)、不动 painter ABI、不改 `CanvasResources` buffer count / present 策略、不动 `monitors/game-bar-widget/`、不改 `hotfix-visible-render` / `animation-and-viewport-fix` 两个历史 spec 目录。

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1"] },
    { "id": 1, "tasks": ["1.2"] },
    { "id": 2, "tasks": ["1.3", "1.4"] },
    { "id": 3, "tasks": ["1.5"] },
    { "id": 4, "tasks": ["1.6", "3.1"] },
    { "id": 5, "tasks": ["3.2", "3.3"] },
    { "id": 6, "tasks": ["3.4", "3.5", "3.6"] },
    { "id": 7, "tasks": ["5.1"] },
    { "id": 8, "tasks": ["5.2", "6.1", "7.1"] },
    { "id": 9, "tasks": ["6.2", "7.2"] },
    { "id": 10, "tasks": ["6.3", "7.3"] },
    { "id": 11, "tasks": ["7.4"] },
    { "id": 12, "tasks": ["7.5", "7.6"] },
    { "id": 13, "tasks": ["7.7", "9.1"] },
    { "id": 14, "tasks": ["9.2"] },
    { "id": 15, "tasks": ["9.3"] },
    { "id": 16, "tasks": ["9.4", "9.5", "9.6"] },
    { "id": 17, "tasks": ["10.1"] },
    { "id": 18, "tasks": ["10.2", "10.3"] }
  ]
}
```

## Workflow Completion

本 spec 的三份文档(requirements.md / design.md / tasks.md)已齐。你可以:

1. 打开 `.kiro/specs/canvas-monitor-lifecycle/tasks.md`
2. 点击任一任务项旁边的 "Start task" 开始执行

按 design.md §迁移顺序 的 Step 1 → Step 5 推进,每完成一个 Step 的 Checkpoint 都要跑一遍 preservation + workspace 测试,若红色出现必须回退该 Step 再改,不允许带着红色进入下一 Step。
