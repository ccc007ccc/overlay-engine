# Game Bar 多 App 合成性能优化计划

## 目标

把当前 Core compositor MVP 从“每个 App 的 `SubmitFrame` 都立即整场景合成”改成“Monitor-paced dirty scene compositor”。目标是在两个或多个 App 同时向同一个 Game Bar Widget / MonitorScene 绘制时，每个 monitor 每个 tick 最多合成一次，减少重复合成、降低 `render_ctx` 串行热路径压力，并保持现有 no-legacy 协议和 ownership 约束不变。

## 修改前三问

1. 这是真问题还是臆想？
   - 是真问题。真实 smoke 已确认多 App 同屏、透明叠加、动态图形都正常，但用户观察到帧数偏低。
   - 代码热路径也吻合：`SubmitFrame` 当前会同步采样并合成相关 `MonitorScene`，两个 App 同时动时，同一个 scene 会被重复合成。
2. 现有实现能否复用或扩展？
   - 能复用现有 `CanvasResources`、`CompositeOutputResources`、`MonitorScene`、`MonitorLayer`、`last_presented_idx`、`render_composite_scenes()` 的实际合成逻辑。
   - 不重写渲染器，不改 wire protocol，先把调度从 per-submit 移到独立 compositor tick。
3. 会影响哪些调用关系、配置或用户流程？
   - 影响 Core 内部 `SubmitFrame` 调度和 composite output 更新时机。
   - 不影响 App 侧 API：App 仍通过 `CreateCanvas` + `SubmitFrame` + `StartMonitor(DesktopWindow/GameBar)`。
   - 不影响 Monitor 侧挂载协议：Desktop / Game Bar 仍消费 `MonitorCompositeAttached` surface。

## 已验证现状

- `core-server/src/server_task.rs` 中 `SubmitFrame` 会在读锁下调用 `snapshot_composite_scene_targets(...)`，并在同一次 `dispatch_submit_frame(...)` 中调用 `render_composite_scenes(...)`。
- `render_composite_scenes(...)` 每次都会：
  1. acquire composite output buffer；
  2. clear output；
  3. 遍历 scene layers；
  4. 每层读取 canvas 的 `last_presented_idx`；
  5. 每层创建 source bitmap；
  6. DrawBitmap；
  7. SetBuffer + Present。
- `CoreDevices.render_ctx` 是全局 `Mutex<RenderContextGuard>`。这是 D3D immediate context / D2D context 的正确安全边界，但现在 App submit 和 scene composite 都在这条串行路径里执行，重复合成会直接放大延迟。

## 长期路线选择

采用“短期低风险、长期可扩展”的两阶段优化：

```text
App SubmitFrame
  -> 只渲染并 present 自己的 Canvas World / MonitorLocal surface
  -> Canvas present 成功后标记相关 MonitorScene dirty
  -> 立即返回继续处理 IPC

Core compositor tick（目标 120Hz，可调）
  -> Core 进程请求 1ms Windows timer resolution，避免 8.333ms tick 被默认约 15.6ms timer resolution 限到约 64Hz
  -> App Canvas 在 Core-composited path 下只写 offscreen texture，不再单独 present Canvas surface
  -> 合并 dirty canvas / dirty monitor 事件
  -> 每个 monitor scene 每 tick 最多 acquire + composite + present 一次
  -> 采样每个 layer 最新的 Canvas texture slot
  -> 输出同一张 CompositeOutputSurface 给 Desktop/Game Bar thin monitor
```

这样保留 Core compositor policy，不把策略下放到 Game Bar Widget，也不引入 Desktop 与 Game Bar 两套合成逻辑。

## 不做范围

- 不恢复旧 `RegisterMonitorV2`、`auto_attach`、ownerless global attach。
- 不让 Game Bar Widget 自己管理 z-order / focus / clip / 权限。
- 不支持任意外部 HWND attach；DesktopWindow 仍是 Core-managed monitor。
- 不在本轮实现完整 VirtualWindow chrome / 输入路由。
- 不把 Desktop monitor 改成多 child visual 的 LayerSurfaces；本轮继续走 Core composite output。
- 不为了帧数牺牲 ownership 校验或错误处理。

## 实施步骤

### Phase 1：引入 compositor scheduler

1. 新增 Core 内部 compositor 调度模块，建议文件：
   - `core-server/src/compositor.rs`
2. 定义事件：
   - `CanvasPresented { canvas_id, frame_id }`
   - `SceneStructureChanged { monitor_id }`
3. 在 `run_server()` 启动一个 compositor task，并把发送端传给每个 `handle_client(...)`。
4. compositor task 内部维护：
   - `dirty_canvas_ids: HashSet<u32>`
   - `dirty_monitor_ids: HashSet<u32>`
   - `tokio::time::interval(Duration::from_micros(8_333))`
   - Core 进程启动时调用 `timeBeginPeriod(1)`，并显式关闭 `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION`，让 120Hz tick 不被默认 timer resolution 限制。
   - `MissedTickBehavior::Skip`，避免落后时补 tick 造成连环合成。

### Phase 2：从 SubmitFrame 热路径拆出 scene composite

1. `SubmitFrame` 保留现有 decode、ownership resolve、world/local target snapshot 和 world/local 渲染。
2. 删除 `SubmitFrame` 同步调用 `snapshot_composite_scene_targets(...)` 的路径。
3. 如果 Canvas 已绑定到有 `monitor_scene_outputs` 的 monitor，World target 走 TextureOnly offscreen path：只写 D3D texture slot 并更新 `last_presented_idx`，不再对 Canvas 自己的 PresentationManager 执行 `SetBuffer + Present`。
4. 调整 `dispatch_submit_frame(...)` 返回轻量结果：
   - `world_presented: bool`
   - 可选：`world_used: bool` / `world_dropped: bool`
4. 仅当 Canvas World 成功 present 并更新 `last_presented_idx` 后，发送 `CanvasPresented` 到 compositor scheduler。
5. `MonitorLocal` 仍按现有逻辑独立 present，不进入 scene composite dirty 判定。

### Phase 3：tick 内合并并渲染 dirty scenes

1. tick 到达时，先 drain channel 中的待处理事件。
2. 在 `SERVER_STATE.read()` 下把 dirty canvas 映射到包含它的 monitor scenes。
3. 每个 dirty monitor 只创建一个 `CompositeSceneSnapshot`。
4. 释放 state lock 后，进入 `devices.render_ctx.lock()`，调用现有合成逻辑。
5. 合成失败保持现有错误处理策略：
   - acquire timeout：跳过该 tick，不阻塞 App submit；
   - SetBuffer / Present retry：记录并等待下一 tick；
   - device-lost：继续输出明确日志，后续单独做 rebuild。

### Phase 4：scene 结构变化也触发 dirty

1. App 加入 Game Bar / Desktop monitor 后，`ensure_monitor_scene_output_and_notify(...)` 或 layer upsert 成功时标记 `SceneStructureChanged`。
2. App 断开 / Monitor 移除 / layer 删除时标记相关 scene dirty，确保画面及时移除退出 App 的 layer。
3. 如果 scene output 不存在，dirty 事件只记录不崩溃；等 output 创建后下一次结构事件或 canvas frame 再合成。

### Phase 5：观测指标与防回退

1. 添加低噪声日志或计数器：
   - submit world render duration；
   - compositor tick composite duration；
   - dirty event coalesced 数量；
   - per tick composite scene 数量；
   - acquire timeout / present retry / device-lost 次数。
2. 避免每帧打印；沿用 rolling window 或每 N tick 输出一次，避免日志本身影响帧率。
3. 如果 monitor-paced 后仍明显低于目标，再进入 Phase 6。

### Phase 6：按结果决定是否做 source bitmap 缓存

第一轮先不强行引入 D2D source bitmap 缓存，避免同时改调度和资源生命周期导致风险叠加。

如果 Phase 1-5 后仍有瓶颈，再补：

1. 复用 `D2DEngine` 内部 cache 思路，为 composite source texture 建立 `ID2D1Bitmap1` 缓存。
2. cache key 需要稳定绑定到 `CanvasResources` 的 texture slot 生命周期，不能只凭易失指针猜测。
3. Canvas resize / resource rebuild / device-lost 时必须清理缓存。

## 测试计划

### 静态检查

```bash
cargo fmt --manifest-path C:/code/Rust/overlay-engine/Cargo.toml --all -- --check
cargo check --manifest-path C:/code/Rust/overlay-engine/Cargo.toml -p core-server --bin core-server
cargo check --manifest-path C:/code/Rust/overlay-engine/Cargo.toml -p core-server --bin demo-app
```

### 单元 / 集成测试

```bash
cargo test --manifest-path C:/code/Rust/overlay-engine/Cargo.toml -p core-server
```

重点保持通过：

- `core-server/tests/multi_app_state.rs`
- `core-server/tests/lifecycle_integration.rs`
- `core-server/tests/preservation.rs`

新增测试建议：

1. 同一个 canvas 连续 dirty 多次，同一 tick 只映射出一个 monitor scene。
2. 两个 canvas 属于同一个 Game Bar scene，同一 tick 只合成一个 monitor scene。
3. scene 结构变化事件能在无新 app frame 时触发一次合成。

### 真实 smoke

1. 打开 Game Bar Widget。
2. 启动 Core。
3. 启动两个 demo app：

```bash
cargo run --manifest-path C:/code/Rust/overlay-engine/Cargo.toml -p core-server --bin demo-app -- --game-bar --smoke-index 1
cargo run --manifest-path C:/code/Rust/overlay-engine/Cargo.toml -p core-server --bin demo-app -- --game-bar --smoke-index 2
```

4. 观察：
   - 蓝色底层 + 半透明洋红 overlay 同时可见；
   - App 1 橙色方块与 App 2 青色方块/线条同时平滑运动；
   - 不再出现明显“两个画面交替跳”；
   - 两 App 应接近 120Hz pacing；10 App 共享 Game Bar scene 时 Core compositor `present rate` 不应再被卡在约 64 scene/s，若仍受 Game Bar / DWM / PresentationManager 限制，则输出实际指标并定位下一瓶颈。
5. 检查日志关键字：
   - `truncated`
   - `payload_len`
   - `device-lost`
   - `SetBuffer`
   - `acquire failed`
   - `create source bitmap failed`
   - `Present fatal`

## 验收标准

- 功能不回退：多 App 单 Game Bar Widget 同屏合成仍正常。
- App submit 不再同步执行整场景合成。
- 同一个 monitor scene 在同一 compositor tick 内最多合成一次。
- 两个 App 同时动的 Game Bar smoke 帧率明显高于当前约 32FPS，目标接近 60FPS。
- `cargo fmt --check`、`cargo check`、`cargo test -p core-server` 通过。
- 无新增 legacy 兼容路径、无新增硬编码敏感信息、无绕过 ownership 校验。

## 风险与回滚

- 风险 1：compositor tick 与 SubmitFrame 的时序导致采样到旧 buffer。
  - 缓解：只在 World present 成功并更新 `last_presented_idx` 后发送 dirty。
- 风险 2：channel 堆积。
  - 缓解：tick 前 drain 并用 `HashSet` 合并，只保留最新 dirty 状态。
- 风险 3：scene output 被移除后仍有 dirty 事件。
  - 缓解：snapshot 时检查 `monitor_scene_outputs` 是否存在，不存在则跳过。
- 风险 4：render_ctx 仍串行，调度改善后仍有 D2D bitmap 创建瓶颈。
  - 缓解：先用指标确认，再进入 Phase 6 source bitmap 缓存。

回滚方式：保留现有 per-submit composite 实现的函数边界；如果 scheduler 出现严重问题，可以暂时恢复 `SubmitFrame -> render_composite_scenes` 调用链并关闭 compositor task。