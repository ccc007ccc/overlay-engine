# Task 2 — Preservation Baseline on UNFIXED code

**Spec**: `.kiro/specs/hotfix-visible-render/`
**Task**: `2. Write preservation property tests (BEFORE implementing fix)`
**Goal**: Confirm that on the **unfixed** hotfix baseline, the existing
22 preservation + 2 animation-and-viewport-fix exploration + 26
core-server lib + 87 rust-renderer = **137 tests all pass**, and that
`monitors/game-bar-widget/` is untouched (PE-10). No new PBTs are added;
this hotfix re-uses the PBTs from `animation-and-viewport-fix` per
design.md §Testing Strategy → Property-Based Tests ("不新增 PBT").

## Result

✅ **Baseline green**: 22 + 2 + 26 + 87 = **137 tests PASS** on unfixed
code. `monitors/game-bar-widget/` has zero diff lines against HEAD.
Task 2 complete.

The new `core-server/tests/hotfix_visible_render_exploration.rs`
authored by task 1 **does** fail on unfixed code (3 failed + 2 ignored)
— that failure is the **bug-condition witness** encoded by task 1, NOT
a preservation regression, and is explicitly excluded from the
preservation baseline per this task's instructions and the
"observation-first" methodology in the task file.

## Log files

| File | What it contains |
| --- | --- |
| `preservation-build.log` | `cargo test -p core-server --test preservation --no-run` build output |
| `preservation-run.log` | 22 preservation tests (PBT A / A' / B / C / D + trace + unit) — all PASS |
| `bug-condition-exploration-run.log` | 2 animation-and-viewport-fix exploration tests — both PASS |
| `core-server-lib-run.log` | 26 core-server lib unit tests — all PASS |
| `rust-renderer-lib-run.log` | 87 rust-renderer crate tests — all PASS |
| `desktop-window-lib-run.log` | desktop-window monitor lib (0 tests, confirms target builds clean) |
| `hotfix-exploration-expected-fail.log` | Task 1's own exploration test — fails on unfixed code as designed (3 fail, 2 ignored) |
| `game-bar-widget-guard.log` | `git diff --stat monitors/game-bar-widget/` — empty (PE-10) |
| `core-server-all-build.log` | Build enumeration of all core-server test targets |
| `workspace-build.log` | Build enumeration of all workspace test targets |

## Per-oracle / suite breakdown

### PBT A (control-plane bit-identical) — `control_plane_bytes.bin`

Validates PE-1, PE-6 (oracle bytes unchanged, no new wire format).

```
test pbt_a_oracle_capture_control_plane_bytes ... ok
test pbt_a_random_control_message_roundtrip_bit_identical ... ok
```

### PBT A' (`MonitorLocalSurfaceAttached` round-trip) — `control_plane_monitor_local_surface_bytes.bin`

Validates PE-1, PE-6, PE-7 (IPC symbol stability).

```
test pbt_a_prime_oracle_capture_monitor_local_surface_attached ... ok
test pbt_a_prime_monitor_local_surface_attached_roundtrip     ... ok
test pbt_a_prime_unknown_opcode_is_skipped_not_fatal          ... ok
```

### PBT B (World-only pixel equivalence) — `world_only_hashes.txt`

Validates PE-8.

```
test pbt_b_oracle_capture_world_only_pixel_hashes ... ok
test pbt_b_world_stream_pixel_hash_is_deterministic ... ok
```

### PBT C (high-rate non-freeze, no unbounded growth) — `high_rate_bounds.txt`

Validates PE-5 (no regression to animation stall) and PE-3.

```
test pbt_c_oracle_capture_high_rate_bounds ... ok
test pbt_c_submit_interval_and_duration_have_bounded_state ... ok
```

### PBT D (multi-consumer independence) — `multi_consumer_independence.txt`

Validates PE-9.

```
test pbt_d_oracle_capture_multi_consumer_invariants ... ok
test pbt_d_multi_consumer_up_down_sequences_preserve_independence ... ok
```

### desktop-window attach trace — `desktop_window_attach_trace.txt`

Validates PE-1 (structural trace still matches).

```
test pbt_preservation_oracle_capture_desktop_window_trace ... ok
```

### Preservation unit tests (opcode encode/decode)

Validates PE-6 (per-opcode bytes unchanged). 10 tests:

```
test unit_preservation_all_8_opcodes_in_one_stream          ... ok
test unit_preservation_clear_decodes_unchanged              ... ok
test unit_preservation_draw_line_decodes_unchanged          ... ok
test unit_preservation_fill_ellipse_decodes_unchanged       ... ok
test unit_preservation_fill_rect_decodes_unchanged          ... ok
test unit_preservation_fill_rounded_rect_decodes_unchanged  ... ok
test unit_preservation_opcode_table_is_consistent           ... ok
test unit_preservation_stroke_ellipse_decodes_unchanged     ... ok
test unit_preservation_stroke_rect_decodes_unchanged        ... ok
test unit_preservation_stroke_rounded_rect_decodes_unchanged ... ok
```

`preservation.rs` total: **22 passed; 0 failed; 0 ignored** (1.28 s)

### Bug-condition exploration replay — PE-2

The two `animation-and-viewport-fix` exploration tests must continue to
pass on this hotfix's baseline (PE-2). They do:

```
test prop_1a_submit_frame_rotates_through_distinct_buffers ... ok
test prop_1b_monitor_local_fill_rect_is_visible_at_each_consumer_10_10 ... ok
```

`bug_condition_exploration.rs` total: **2 passed; 0 failed; 0 ignored** (0.62 s)

### core-server lib (26 tests) — PE-3

`cargo test -p core-server --lib` → **26 passed; 0 failed; 0 ignored** (0.01 s)

Breakdown: 7 `ipc::cmd_decoder` tests, 2 `renderer::mediafoundation`, 6
`renderer::resources`, 11 `server_task` (8 `scan_targets_*` + 4
`record_render_duration_*`).

### rust-renderer crate (87 tests) — PE-4

`cargo test -p renderer --lib` → **87 passed; 0 failed; 0 ignored** (4.92 s)

These are the "87 renderer tests across painter / resources / wic /
mediafoundation / dcomp modules" referenced in bugfix.md 3.4 and
design.md §Preservation Requirements PE-4. They live in the
`rust-renderer/` crate (not `core-server/src/renderer/`).

### Game Bar widget guard — PE-10

`git diff --stat monitors/game-bar-widget/` → **0 lines output** — the
widget source is untouched, as required.

## Comparison with task-1 H-probe log

Task 1 authored `core-server/tests/hotfix_visible_render_exploration.rs`
which documents the H1–H5 hypothesis findings from code inspection in
the file's tail and in the `probe_1d_*` doc-comment. That file's H
findings are referenced by task 3.4 to pick the Change-D* branch once a
definitive runtime H-confirmation is available. Task 2 does not need
to rerun those probes; it only needs the preservation baseline to be
green, which it is.

## What happens next

Task 2 is complete. Task 3.1 / 3.2 / 3.3 / 3.4 (Change-A / B / C / D)
can now proceed, knowing that:

1. The preservation baseline is clean at 137 green tests.
2. No oracle needs to be regenerated.
3. Game Bar widget has zero diff and must stay that way.
4. The hotfix exploration test in
   `core-server/tests/hotfix_visible_render_exploration.rs` is expected
   to **flip** from fail → pass once Changes A+B+C land (sub-properties
   1a / 1b-static / 1c) and the runtime H-probe for sub-property 1d
   confirms cyan at client (10, 10) for each consumer.
