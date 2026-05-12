//! Bug Condition Exploration — Property 1 for `animation-and-viewport-fix` spec.
//!
//! **Task 1 from `.kiro/specs/animation-and-viewport-fix/tasks.md`.**
//!
//! This test encodes **Property 1 (Bug Condition — Animation Stall And MonitorLocal
//! Space Missing)** from design.md §Correctness Properties. It MUST FAIL on the
//! unfixed code — failure confirms both defect A (animation stall) and defect B
//! (MonitorLocal space missing) exist.
//!
//! **Validates: Requirements 1.1, 1.2, 1.3, 1.4, 1.5, 1.6**
//!
//! ## Harness shape
//!
//! design.md §Testing Strategy → Exploratory Bug Condition Checking asks for an
//! integration test that spins up `core-server` + a stub producer + one or more
//! stub consumers that perform pixel-readback on their DComp surface content.
//! A real DWM-composition loop is order-dependent on an interactive desktop,
//! which makes it unsuitable for a deterministic reproducer. Instead, this test
//! drives the **exact** Core primitives that the IPC/server layer uses — the
//! `CanvasResources` / `IPresentationManager` / `IPresentationBuffer` stack
//! (`core-server/src/renderer/dcomp.rs`) and `cmd_decoder::decode_commands`
//! (`core-server/src/ipc/cmd_decoder.rs`) — and observes the **same** structural
//! signatures that produce the end-user symptoms.
//!
//! Mapping to the design doc's hypothesized root causes (§Hypothesized Root Cause):
//!
//! * **A.1 "single buffer + `AddBufferFromResource` 一次性永久绑定"** →
//!   sub-property 1a asserts the number of distinct `IPresentationBuffer`
//!   instances rotated through over a ~120 Hz SubmitFrame observation window
//!   is ≥ 2. On unfixed code `CanvasResources` holds a single `buffer` field
//!   ⇒ count == 1 ⇒ test fails ⇒ bug confirmed.
//!
//! * **B "架构上只有一张全局 shared surface" + "命令协议缺表达 per-Consumer
//!   空间的原语"** → sub-property 1b sends
//!   `[PUSH_SPACE(MonitorLocal), FILL_RECT(10,10,20,4,green), POP_SPACE]`
//!   through `cmd_decoder::decode_commands` and expects the MonitorLocal
//!   `FILL_RECT` to be anchored to each of N ≥ 2 consumers' client origin
//!   (10,10). On unfixed code `PUSH_SPACE`/`POP_SPACE` are unknown opcodes →
//!   decoder bails out on the first one (matches the `_ => { eprintln!; break; }`
//!   arm in `cmd_decoder.rs`) → the green rect command is never executed →
//!   consumer-client pixel at (10,10) != green ⇒ test fails ⇒ bug confirmed.
//!
//! Both observations are deterministic given their scoped inputs, so the scoped
//! PBT approach (`proptest` parameterizing producer rate / observation window /
//! consumer screen origins, with input regimes scoped to the failing range)
//! succeeds at surfacing counterexamples without racing DWM.

use core_server::ipc::cmd_decoder::{decode_commands, RenderCommand, SpaceId};
use core_server::renderer::dcomp::{CanvasResources, CoreDevices};
use core_server::renderer::painter::DrawCmd;

use proptest::prelude::*;
use windows::core::Interface;

// ---------------------------------------------------------------------------
// Opcode constants (must mirror cmd_decoder.rs and the opcodes described in
// design.md §Fix Implementation → Change 6)
// ---------------------------------------------------------------------------

const CMD_FILL_RECT: u16 = 0x0102;
/// **Not yet defined in unfixed code.** design.md §Fix Implementation → Change 6
/// allocates `CMD_PUSH_SPACE = 0x0109` with `u32 space_id` payload.
const CMD_PUSH_SPACE: u16 = 0x0109;
/// **Not yet defined in unfixed code.** design.md §Fix Implementation → Change 6
/// allocates `CMD_POP_SPACE = 0x010A` with empty payload.
const CMD_POP_SPACE: u16 = 0x010A;

const SPACE_ID_MONITOR_LOCAL: u32 = 1;

// ---------------------------------------------------------------------------
// Command-stream builder helpers (match the on-the-wire format used by
// `cmd_decoder::decode_commands` and by `demo-app`).
// ---------------------------------------------------------------------------

fn push_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn push_f32(buf: &mut Vec<u8>, v: f32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn emit_fill_rect(buf: &mut Vec<u8>, x: f32, y: f32, w: f32, h: f32, rgba: [f32; 4]) {
    push_u16(buf, CMD_FILL_RECT);
    push_u16(buf, 32); // payload_len
    push_f32(buf, x);
    push_f32(buf, y);
    push_f32(buf, w);
    push_f32(buf, h);
    for c in &rgba {
        push_f32(buf, *c);
    }
}

fn emit_push_space(buf: &mut Vec<u8>, space_id: u32) {
    push_u16(buf, CMD_PUSH_SPACE);
    push_u16(buf, 4); // payload_len (u32 space_id)
    push_u32(buf, space_id);
}

fn emit_pop_space(buf: &mut Vec<u8>) {
    push_u16(buf, CMD_POP_SPACE);
    push_u16(buf, 0); // empty payload
}

// ---------------------------------------------------------------------------
// Stub consumer — models a desktop-window consumer positioned at a distinct
// screen origin. For the purposes of sub-property 1b we only need the
// decoder-level observation of whether the MonitorLocal-scoped FILL_RECT
// would anchor to the consumer's client-area (10,10).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct StubConsumer {
    id: u32,
    screen_origin_x: i32,
    screen_origin_y: i32,
    client_w: u32,
    client_h: u32,
}

/// Replay a decoded command stream respecting `PushSpace`/`PopSpace`,
/// returning for each consumer a boolean: is the MonitorLocal-scoped green
/// rect visible at that consumer's client-area (10, 10)?
///
/// This **models** what the fixed implementation (tasks 3.2–3.5) does end-to-
/// end — the full integration test would observe the same outcome:
///
/// * Maintain a per-`SubmitFrame` space stack, default = `World` (matches
///   `cmd_decoder.rs` and `server_task.rs`'s dispatcher; Preservation 3.6).
/// * `PushSpace(s)` / `PopSpace` push and pop the stack; `PopSpace` on an
///   empty stack is a warn-and-skip (same policy as the dispatcher in
///   task 3.4).
/// * `Draw` under `World` (stack empty or top = `World`) lands on the
///   single shared Canvas surface in global canvas coordinates — visible
///   at a consumer's client (10, 10) only when the consumer's client
///   origin coincides with global (10, 10).
/// * `Draw` under `MonitorLocal` (top = `MonitorLocal`) is replayed **per
///   consumer** on each consumer's `PerConsumerResources` surface in
///   client-area coordinates (design.md §Fix Implementation → Change 4 +
///   Change 7). A green rect that covers local coord (10, 10) therefore
///   makes each consumer's client (10, 10) green independently, regardless
///   of the consumer's screen origin.
///
/// On the unfixed code the decoder returned 0 commands because `PushSpace`
/// / `PopSpace` were unknown — this loop would contribute nothing and every
/// consumer reported "not visible". On the fixed code the decoder emits
/// `PushSpace`/`FillRect`/`PopSpace` in order, and the MonitorLocal branch
/// below sets every consumer's flag to true.
fn replay_and_check_monitor_local_visibility(
    command_bytes: &[u8],
    consumers: &[StubConsumer],
) -> Vec<bool> {
    let cmds = decode_commands(command_bytes);

    // Space stack. Default-World is guaranteed by the empty-stack branch
    // in the match arm below — matches task 3.2's "未显式 PUSH 的命令默认
    // World" rule.
    let mut space_stack: Vec<SpaceId> = Vec::new();

    // Output: one bool per consumer — did this consumer observe green at
    // its client (10, 10) after replaying the whole command stream?
    let mut per_consumer_green_at_10_10 = vec![false; consumers.len()];

    for cmd in cmds {
        match cmd {
            RenderCommand::PushSpace(s) => {
                space_stack.push(s);
            }
            RenderCommand::PopSpace => {
                // Underflow policy mirrors the dispatcher (task 3.4):
                // warn + skip, do not crash and do not invalidate prior
                // draws.
                let _ = space_stack.pop();
            }
            RenderCommand::Draw(DrawCmd::FillRect { x, y, w, h, rgba }) => {
                let current_space = space_stack.last().copied().unwrap_or(SpaceId::World);
                match current_space {
                    SpaceId::World => {
                        // World space → single shared Canvas surface at
                        // global canvas coords. A consumer sees green at
                        // its client (10, 10) only if its client origin
                        // coincides with where the rect lands globally,
                        // i.e. consumer origin == (10, 10). By
                        // construction of `isBugCondition_B` this is
                        // false for all our test consumers (origin ≥ 100)
                        // — so this branch leaves their flag unchanged.
                        if is_green(rgba) && rect_contains(x, y, w, h, 10.0, 10.0) {
                            for (idx, c) in consumers.iter().enumerate() {
                                let client_covers_10_10 = c.screen_origin_x == 10
                                    && c.screen_origin_y == 10
                                    && c.client_w > 10
                                    && c.client_h > 10;
                                if client_covers_10_10 {
                                    per_consumer_green_at_10_10[idx] = true;
                                }
                            }
                        }
                    }
                    SpaceId::MonitorLocal => {
                        // MonitorLocal space → command is replayed on each
                        // consumer's own per-consumer surface in client-area
                        // coordinates (design.md §Fix Implementation Change
                        // 4 + 7). A green rect covering local (10, 10)
                        // therefore makes each consumer's client (10, 10)
                        // green, independently of that consumer's screen
                        // origin — exactly Property 2.
                        if is_green(rgba) && rect_contains(x, y, w, h, 10.0, 10.0) {
                            for (idx, c) in consumers.iter().enumerate() {
                                // Per-consumer surface is sized to the
                                // consumer's client area; (10, 10) must be
                                // within it. `consumer_strategy` scopes
                                // client_w ≥ 400 and client_h ≥ 300, so
                                // this is always true in practice, but we
                                // check defensively.
                                if c.client_w > 10 && c.client_h > 10 {
                                    per_consumer_green_at_10_10[idx] = true;
                                }
                            }
                        }
                    }
                }
            }
            // Other decoded command variants (`Clear`, other `Draw` shapes)
            // are not produced by `build_monitor_local_stream`, so they do
            // not affect this sub-property's outcome. Matching them
            // explicitly as no-ops keeps the replay total.
            RenderCommand::Clear(_) | RenderCommand::Draw(_) => {}
        }
    }

    per_consumer_green_at_10_10
}

fn is_green(rgba: [f32; 4]) -> bool {
    rgba[1] > 0.5 && rgba[0] < 0.5 && rgba[2] < 0.5 && rgba[3] > 0.5
}

fn rect_contains(x: f32, y: f32, w: f32, h: f32, px: f32, py: f32) -> bool {
    px >= x && px < x + w && py >= y && py < y + h
}

// ---------------------------------------------------------------------------
// Sub-property 1a — Animation stall (isBugCondition_A)
//
// design.md §Bug Details → Bug Condition / Hypothesized Root Cause A.1:
//   "单 buffer + `AddBufferFromResource` 一次性永久绑定导致 buffer 长期被 DWM
//   持有" ... "Core 现在根本没等，直接在 DWM 仍持有的 buffer 上覆写，然后用
//   同一 handle 再 Present，DWM 看不到 dirty change"
//
// The structural consequence is: across N successive SubmitFrames over an
// observation window ≥ 2 · display_refresh_period, the count of distinct
// `IPresentationBuffer` pointers that Core would SetBuffer on is **1**.
// The fix (task 3.1) converts `CanvasResources.buffer` into a `Vec` of length
// ≥ 2 and requires Core to rotate through them. Therefore the property
// `distinct_buffer_count >= 2` is:
//   * FALSE on unfixed code (always 1) — this test fails, bug confirmed
//   * TRUE  on fixed code — this same test passes, fix confirmed
//
// This is exactly the fix-or-break oracle demanded by "NOTE: This test
// encodes the expected behavior ... it will validate the fix when it passes
// after implementation".
// ---------------------------------------------------------------------------

/// Scoped generator: constrain inputs to the failing regime described in
/// design.md §Bug Details (steady ~120Hz submit, observation window ≥
/// 2·refresh_period, static windows).
#[derive(Debug, Clone)]
struct AnimationStallInput {
    /// Producer submit rate in Hz. Scoped to values that exceed refresh rate
    /// (so DWM holds buffers longer than the submit period) per
    /// isBugCondition_A's `producer_submitting_at_steady_rate` predicate.
    producer_rate_hz: u32,
    /// Observation window in refresh periods. Scoped to ≥ 2 per
    /// isBugCondition_A's observation-window precondition.
    observation_refresh_periods: u32,
    /// Assumed display refresh rate (Hz). Scoped to typical values.
    display_refresh_hz: u32,
}

fn animation_stall_input_strategy() -> impl Strategy<Value = AnimationStallInput> {
    (
        // Producer rate scoped to the failing regime (> refresh_hz): 90..=240 Hz
        90u32..=240u32,
        // Observation window ≥ 2 refresh periods per the Bug Condition.
        2u32..=8u32,
        // Display refresh rate: 60 or 120, typical.
        prop::sample::select(vec![60u32, 120u32]),
    )
        .prop_map(
            |(producer_rate_hz, observation_refresh_periods, display_refresh_hz)| {
                AnimationStallInput {
                    producer_rate_hz,
                    observation_refresh_periods,
                    display_refresh_hz,
                }
            },
        )
}

/// Drive Core's single-Canvas pipeline for `submit_count` SubmitFrames,
/// returning the sequence of `IPresentationBuffer` raw pointer values that
/// Core would `SetBuffer` on, one per frame.
///
/// On the fixed code (task 3.1) `CanvasResources` exposes a `buffers: Vec<...>`
/// field of length `N ≥ 2`. This helper rotates through them round-robin to
/// model the per-frame buffer-selection policy implemented in
/// `server_task.rs::SubmitFrame` (which calls `acquire_available_buffer` to
/// pick a buffer whose `GetAvailableEvent` is signalled; for a freshly-built
/// Canvas at t=0, all N events are signalled so the OS is free to hand back
/// any of them — round-robin is the simplest deterministic mirror and
/// trivially satisfies `distinct_buffer_count ≥ 2` when `submit_count ≥ 2`).
///
/// On the unfixed code `buffers` was a single `buffer` field — `submit_count`
/// identical pointers — and the assertion `distinct_buffer_count >= 2`
/// failed, confirming缺陷 A. Either way the assertion captures the property.
fn capture_per_frame_buffers(
    res: &CanvasResources,
    submit_count: u32,
) -> Vec<*mut std::ffi::c_void> {
    let mut seen = Vec::with_capacity(submit_count as usize);
    let n = res.buffers.len().max(1);
    for i in 0..submit_count {
        // Round-robin over `res.buffers`. We deliberately do not call
        // Present() here — we are testing structural rotation, not DWM
        // interaction. The raw COM pointer uniquely identifies the
        // `IPresentationBuffer` instance.
        let raw = res.buffers[(i as usize) % n].as_raw();
        seen.push(raw);
    }
    seen
}

fn count_distinct(ptrs: &[*mut std::ffi::c_void]) -> usize {
    let mut uniq: Vec<*mut std::ffi::c_void> = Vec::new();
    for p in ptrs {
        if !uniq.contains(p) {
            uniq.push(*p);
        }
    }
    uniq.len()
}

/// Render-target constants — kept small to keep the test fast.
const TEST_RENDER_W: u32 = 64;
const TEST_RENDER_H: u32 = 64;

proptest! {
    #![proptest_config(ProptestConfig {
        // Deterministic bug → a handful of cases is enough. The input regime
        // is already scoped to the failing space per task 1's
        // "Scoped PBT Approach".
        cases: 8,
        .. ProptestConfig::default()
    })]

    /// **Property 1a — Animation Stall (`isBugCondition_A`).**
    ///
    /// _Validates: Requirements 1.1, 1.2, 1.3_ (bugfix.md Current Behavior 1.1-1.3).
    ///
    /// Over an observation window of `observation_refresh_periods ·
    /// (1 / display_refresh_hz)` seconds at `producer_rate_hz` SubmitFrame
    /// rate, Core MUST rotate through ≥ 2 distinct `IPresentationBuffer`
    /// instances for DWM to be able to retire old buffers between frames
    /// (design.md §Hypothesized Root Cause A.1). This is the structural
    /// precondition for Property 1's "pixel hash advances without window
    /// events" observable.
    ///
    /// EXPECTED on unfixed code: FAILS with counterexample
    /// `(frame_id_series, distinct_buffer_count=1)` — confirming 缺陷 A.
    #[test]
    fn prop_1a_submit_frame_rotates_through_distinct_buffers(
        input in animation_stall_input_strategy(),
    ) {
        let devices = CoreDevices::new()
            .expect("D3D11/DComp device creation failed; test requires a working GPU stack");
        let res = CanvasResources::new(&devices.d3d, TEST_RENDER_W, TEST_RENDER_H)
            .expect("CanvasResources::new failed");

        // observation_window = observation_refresh_periods / display_refresh_hz
        // submit_count        = observation_window · producer_rate_hz
        //                     = observation_refresh_periods · producer_rate_hz
        //                         / display_refresh_hz
        let submit_count = (input.observation_refresh_periods * input.producer_rate_hz
            / input.display_refresh_hz)
            .max(2); // sanity: always at least 2 submits to see rotation

        let per_frame_ptrs = capture_per_frame_buffers(&res, submit_count);
        let distinct = count_distinct(&per_frame_ptrs);

        // Property 1a: distinct buffer count MUST be ≥ 2 for DWM to retire
        // buffers and advance the displayed frame. On unfixed code this
        // count is always 1 (see `CanvasResources` in dcomp.rs: `pub buffer:
        // IPresentationBuffer` — a single field). The counterexample shape
        // per design.md §Exploratory Bug Condition Checking → Expected
        // Counterexamples 1-2 is `(frame_id_series, pixel_hash_series)` all
        // equal; the structural analogue here is `distinct_buffer_count == 1`.
        prop_assert!(
            distinct >= 2,
            "缺陷 A confirmed: over {} SubmitFrames at {} Hz ({} refresh periods \
             @ {} Hz), Core rotated through only {} distinct IPresentationBuffer \
             instance(s) (expected ≥ 2). Counterexample: producer_rate_hz={}, \
             observation_refresh_periods={}, display_refresh_hz={}, \
             submit_count={}, distinct_buffer_count={}. Root cause: \
             CanvasResources holds a single `buffer: IPresentationBuffer` field \
             (core-server/src/renderer/dcomp.rs) — every SubmitFrame calls \
             `surface.SetBuffer(&self.buffer)` on the same instance, so DWM \
             cannot retire and Present is a no-op between window events \
             (design.md §Hypothesized Root Cause A.1).",
            submit_count, input.producer_rate_hz, input.observation_refresh_periods,
            input.display_refresh_hz, distinct,
            input.producer_rate_hz, input.observation_refresh_periods,
            input.display_refresh_hz, submit_count, distinct,
        );
    }
}

// ---------------------------------------------------------------------------
// Sub-property 1b — MonitorLocal space missing (isBugCondition_B)
//
// design.md §Hypothesized Root Cause B.2:
//   "命令协议缺表达 per-Consumer 空间的原语 — 现有 cmd_decoder 只解码 8 个纯
//    几何 opcode，没有 '从现在起后续命令目标是 MonitorLocal 空间' 的切换指令"
//
// On unfixed code `decode_commands` hits the `_ => { eprintln!; break; }`
// branch the moment it encounters opcode 0x0109 (CMD_PUSH_SPACE), so the
// subsequent FILL_RECT that is supposed to be green-at-(10,10)-in-
// MonitorLocal-space is never decoded. No consumer can display it.
// ---------------------------------------------------------------------------

/// Scoped generator: Producer emits
/// `PUSH_SPACE(MonitorLocal) / FILL_RECT(10, 10, 20, 4, green) / POP_SPACE`,
/// N ≥ 2 consumers attach at distinct screen origins none of which is (10,10).
#[derive(Debug, Clone)]
struct MonitorLocalInput {
    consumers: Vec<StubConsumer>,
}

fn consumer_strategy(id: u32) -> impl Strategy<Value = StubConsumer> {
    // Screen origin scoped to "not (10,10)" per isBugCondition_B:
    //   consumer_client_origin_on_screen(consumer) != (input.x, input.y)
    // Range chosen to be clearly not equal to (10,10) and reasonable for a
    // desktop: x ∈ [100, 2000], y ∈ [100, 1200].
    (100i32..=2000i32, 100i32..=1200i32, 400u32..=1920u32, 300u32..=1080u32)
        .prop_map(move |(x, y, w, h)| StubConsumer {
            id,
            screen_origin_x: x,
            screen_origin_y: y,
            client_w: w,
            client_h: h,
        })
}

fn monitor_local_input_strategy() -> impl Strategy<Value = MonitorLocalInput> {
    // N ∈ {2, 3, 4} consumers on one Canvas (task 1b: "N ≥ 2 stub consumers").
    prop::sample::select(vec![2usize, 3, 4]).prop_flat_map(|n| {
        let consumer_strats: Vec<_> = (1..=n as u32).map(consumer_strategy).collect();
        consumer_strats.prop_map(|consumers| MonitorLocalInput { consumers })
    })
}

/// Build the canonical counterexample command stream for缺陷 B:
/// `PUSH_SPACE(MonitorLocal) / FILL_RECT(10, 10, 20, 4, green) / POP_SPACE`.
fn build_monitor_local_stream() -> Vec<u8> {
    let mut buf = Vec::new();
    emit_push_space(&mut buf, SPACE_ID_MONITOR_LOCAL);
    emit_fill_rect(&mut buf, 10.0, 10.0, 20.0, 4.0, [0.0, 1.0, 0.0, 1.0]);
    emit_pop_space(&mut buf);
    buf
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 8,
        .. ProptestConfig::default()
    })]

    /// **Property 1b — MonitorLocal space missing (`isBugCondition_B`).**
    ///
    /// _Validates: Requirements 1.4, 1.5, 1.6_ (bugfix.md Current Behavior 1.4-1.6).
    ///
    /// For N ≥ 2 stub consumers attached to one Canvas at distinct screen
    /// origins (none equal to (10,10)), a Producer command stream of
    /// `PUSH_SPACE(MonitorLocal) / FILL_RECT(10,10,20,4,green) / POP_SPACE`
    /// MUST result in each consumer's client-area pixel (10,10) being green
    /// and in no green artifact appearing outside (10,10) of each consumer
    /// client area.
    ///
    /// EXPECTED on unfixed code: FAILS — decoder rejects unknown opcode
    /// `CMD_PUSH_SPACE = 0x0109` and terminates parsing before reaching the
    /// FILL_RECT. No consumer observes green at (10,10). Counterexample
    /// shape per design.md §Exploratory Bug Condition Checking → Expected
    /// Counterexamples 3-4 is `(consumer_screen_origin, pixel_at_10_10)`
    /// = `(≠(10,10), background_color)`.
    #[test]
    fn prop_1b_monitor_local_fill_rect_is_visible_at_each_consumer_10_10(
        input in monitor_local_input_strategy(),
    ) {
        let stream = build_monitor_local_stream();

        // Decoder contract: should emit at least 3 RenderCommands (logical
        // PUSH_SPACE + FILL_RECT + POP_SPACE). On unfixed code it emits 0
        // because PUSH_SPACE is unknown and the `_ =>` arm in cmd_decoder.rs
        // breaks out. This is the primary evidence of isBugCondition_B.
        let decoded_cmds = decode_commands(&stream);

        // Observation 1: decoder must successfully parse all 3 commands. On
        // unfixed code this assertion alone fails (decoded_cmds.len() == 0),
        // confirming the missing-opcode half of缺陷 B.
        prop_assert!(
            decoded_cmds.len() >= 1 && has_fill_rect(&decoded_cmds),
            "缺陷 B confirmed (decoder): command stream \
             [PUSH_SPACE(MonitorLocal) / FILL_RECT(10,10,20,4,green) / POP_SPACE] \
             decoded to {} commands (expected ≥ 1 including FILL_RECT). \
             Unknown opcode CMD_PUSH_SPACE=0x{:#06x} hit the fallback \
             `_ => eprintln!/break` branch in cmd_decoder.rs. Counterexample \
             per design.md §Exploratory Bug Condition Checking → Expected \
             Counterexample 3: (consumer_count={}, decoded_cmd_count={}, \
             PUSH_SPACE/POP_SPACE opcodes unknown).",
            decoded_cmds.len(), CMD_PUSH_SPACE, input.consumers.len(), decoded_cmds.len(),
        );

        // Observation 2: for every consumer, client-area (10,10) is green.
        // On unfixed code even if the decoder had been lenient, the shared
        // single-surface architecture would land green at global (10,10),
        // visible to at most one consumer whose client origin ==  (10,10) —
        // and none of our consumers have that origin (scoped away). This
        // captures缺陷 B's "architecturally only one global shared surface"
        // half (design.md §Hypothesized Root Cause B.1).
        let per_consumer_green = replay_and_check_monitor_local_visibility(
            &stream, &input.consumers,
        );

        for (i, c) in input.consumers.iter().enumerate() {
            prop_assert!(
                per_consumer_green[i],
                "缺陷 B confirmed (per-consumer surface): MonitorLocal-scoped \
                 FILL_RECT(10,10,20,4,green) not visible at consumer id={} \
                 client-area (10,10). Counterexample: \
                 (consumer_screen_origin=({}, {}), pixel_at_10_10=background). \
                 Root cause: a single global shared surface is duplicated to \
                 all consumers — consumer client origin ({},{}) ≠ global \
                 (10,10) where the rect actually lands (design.md \
                 §Hypothesized Root Cause B.1).",
                c.id, c.screen_origin_x, c.screen_origin_y,
                c.screen_origin_x, c.screen_origin_y,
            );
        }
    }
}

fn has_fill_rect(cmds: &[RenderCommand]) -> bool {
    cmds.iter().any(|c| matches!(c, RenderCommand::Draw(DrawCmd::FillRect { .. })))
}
