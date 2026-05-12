# Bugfix Requirements Document

## Introduction

The `animation-and-viewport-fix` bugfix spec completed with all automated tests passing, and the user confirmed end-to-end that the orange block animation stall (that spec's defect A) is visibly fixed — the block now slides continuously with no window events. However, the same end-to-end run revealed a gap: the MonitorLocal layer (cyan badge + FPS bar, that spec's defect B) passes its structural automated tests but is **not visible in either consumer window's DWM composition**. The automated harness declares victory while the real composition shows nothing.

In parallel, the end-to-end run exposed three small naming and documentation defects that block a clean developer onboarding path: the end-to-end doc points at a cargo command that does not work, the consumer window title is stuck at `"connecting..."` forever, and the `producer` terminology is inconsistent with the user-facing "monitor / core / app" three-layer architecture.

This hotfix bundles those defects into a single bugfix. Four are small, known-correct A-class items that can be fixed directly. The fifth — MonitorLocal visibility — is a real end-to-end regression that requires trace-driven diagnosis (core-side per-consumer surface / present routing, and consumer-side visual tree commit ordering) before the root cause can be named and fixed.

Scope is intentionally narrow:

- No protocol wire-format changes.
- No architecture decisions — those land in the upcoming `canvas-monitor-lifecycle` spec.
- No input events — separate spec.
- No Game Bar widget changes — explicitly deferred.
- All renames are limited to binary / crate / doc-string boundaries. The core IPC `Producer` / `register_producer` / `ControlMessage::RegisterProducer` symbols are out of scope and are deferred to `canvas-monitor-lifecycle`.

## Bug Analysis

### Current Behavior (Defect)

What currently happens, per defect class:

1.1 WHEN a developer follows `END-TO-END-TESTING.md` at the repo root and runs `cargo run -p desktop-window-monitor --bin consumer` THEN the command fails because no binary named `consumer` exists in the `desktop-window-monitor` package; the actual binary is named `desktop-window-consumer`, so the documentation does not match the code.

1.2 WHEN the `desktop-window-monitor` consumer window is created at `CreateWindowExW` time THEN its title is set to `"Desktop Monitor - connecting..."`, and THEN after the `CanvasAttached` and/or `MonitorLocalSurfaceAttached` events arrive, the system does not update the title, so the window continues to display `"connecting..."` indefinitely even after attachment succeeds; WHEN the pipe connection later breaks THEN the title is likewise not updated to reflect reconnection.

1.3 WHEN a reader inspects the three-layer architecture's naming THEN the `core-server/src/bin/demo-producer.rs` binary file and its corresponding `[[bin]]` entry in `core-server/Cargo.toml`, along with touched doc strings, README content, and comments, use the old "producer" terminology, which conflicts with the user-facing "monitor / core / app" layering the user has now confirmed.

1.4 WHEN both `desktop-window-monitor` consumer windows are launched end-to-end against an attached producer THEN neither window shows the MonitorLocal layer (cyan badge + FPS bar) in the DWM composition, even though (a) all 22 preservation tests pass, (b) both bug-condition tests pass, (c) all 26 `core-server` library unit tests pass, and (d) all 87 renderer tests pass; the feature is structurally correct per the automated harness but not visible in the real end-to-end render, so the claimed capability "MonitorLocal rendering visible at each consumer's client (10,10)" is not actually delivered.

### Expected Behavior (Correct)

What should happen instead, matching the clauses above one-to-one:

2.1 WHEN a developer follows `END-TO-END-TESTING.md` to run the consumer THEN the documented cargo command SHALL invoke a binary that actually exists in the `desktop-window-monitor` package; the binary SHALL be renamed in `monitors/desktop-window/Cargo.toml` from `desktop-window-consumer` to `desktop-window-monitor` so the binary name aligns with the package name, and `END-TO-END-TESTING.md` and any other references SHALL be updated to `cargo run -p desktop-window-monitor --bin desktop-window-monitor` so documentation and code agree.

2.2 WHEN `CanvasAttached` and/or `MonitorLocalSurfaceAttached` events arrive at the consumer THEN the system SHALL call `SetWindowTextW` to update the window title to reflect the attached state (e.g. `"Desktop Monitor - canvas N (world + monitor_local)"`, or at minimum a title that drops the `"connecting..."` suffix); AND WHEN the pipe connection subsequently breaks THEN the system SHALL update the title to a reconnecting state (e.g. `"Desktop Monitor - reconnecting..."`).

2.3 WHEN the three-layer architecture naming is applied THEN `core-server/src/bin/demo-producer.rs` SHALL be renamed to `demo-app.rs`, its `[[bin]]` entry in `core-server/Cargo.toml` SHALL be updated accordingly, and doc strings, README content, and comments touched while making this change SHALL use the "app" terminology consistent with "monitor / core / app"; the `desktop-demo-producer` binary name in `monitors/desktop-window/Cargo.toml` SHALL be left as-is for this spec with a comment noting that it is deferred to a future rename spec; the core IPC symbols `Producer`, `register_producer`, and `ControlMessage::RegisterProducer` SHALL NOT be renamed in this spec because they are protocol-level and belong in the upcoming `canvas-monitor-lifecycle` spec.

2.4 WHEN both `desktop-window-monitor` consumer windows are launched end-to-end against an attached producer THEN each window SHALL display its MonitorLocal layer — the cyan badge and the FPS bar — in the real DWM composition, anchored at client `(10, 10)` in the consumer window's logical coordinates, co-located above the World layer; the identified root cause SHALL be named (e.g. one of H1 core per-consumer flush/present routing, H2 `AddVisual` z-order, H3 missing `dcomp_dev.Commit()` after dual-visual mount, H4 per-consumer `Present()` stuck in `RetryNextTick`, H5 per-consumer surface sizing / clipping) and the fix SHALL address that root cause rather than only the automated harness.

### Unchanged Behavior (Regression Prevention)

Existing behavior that must be preserved as the fix lands:

3.1 WHEN the 22 preservation tests in `core-server/tests/preservation.rs` are executed THEN the system SHALL CONTINUE TO pass all of them with no modification to their oracles.

3.2 WHEN the 2 bug-condition exploration tests in `core-server/tests/bug_condition_exploration.rs` are executed THEN the system SHALL CONTINUE TO pass both.

3.3 WHEN the 26 `core-server` library unit tests are executed THEN the system SHALL CONTINUE TO pass all of them.

3.4 WHEN the 87 renderer tests are executed THEN the system SHALL CONTINUE TO pass all of them.

3.5 WHEN the orange block animation (the end-to-end fix delivered by `animation-and-viewport-fix`, defect A of that spec) is observed end-to-end THEN the orange block SHALL CONTINUE TO slide continuously without requiring any window events.

3.6 WHEN the control-plane byte oracles `control_plane_bytes.bin` and `control_plane_monitor_local_surface_bytes.bin` under `core-server/tests/preservation_oracles/` are compared against a fresh run THEN the serialized bytes SHALL CONTINUE TO match exactly, because this spec introduces no protocol wire-format changes.

3.7 WHEN the core IPC protocol is inspected for the symbols `Producer`, `register_producer`, and `ControlMessage::RegisterProducer` THEN the system SHALL CONTINUE TO expose them unchanged; the protocol-level rename is deferred to the `canvas-monitor-lifecycle` spec.

3.8 WHEN the single-consumer World-only rendering path is exercised end-to-end THEN the orange block and the rainbow squares SHALL CONTINUE TO render correctly in the consumer window, with no regression from the MonitorLocal fix.

3.9 WHEN the multi-consumer independence oracle (`multi_consumer_independence.txt`) is executed THEN per-consumer surface independence SHALL CONTINUE TO hold — one consumer's MonitorLocal surface state SHALL NOT affect another's.

3.10 WHEN the Game Bar widget (`monitors/game-bar-widget/`) is built and inspected THEN its source and build artifacts SHALL CONTINUE TO remain untouched by this spec.
