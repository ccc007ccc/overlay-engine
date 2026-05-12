# Task 3.6 — Preservation Verification on FIXED code

**Spec**: `.kiro/specs/hotfix-visible-render/`
**Task**: `3.6 Verify preservation tests still pass`
**Goal**: Re-run the SAME preservation test suite captured by task 2 against
the FIXED code (Change-A + B + C + D3 all landed) and confirm that the
22 preservation + 2 animation-and-viewport-fix exploration + 26 core-server
lib + 87 rust-renderer = **137 tests** stay green, with zero regressions
against the unfixed baseline. Plus Game Bar widget (PE-10) untouched and
IPC symbols (PE-7) intact.

This task writes NO new tests. It reuses the oracles in
`core-server/tests/preservation_oracles/` and the test files from task 2
verbatim, and compares exit codes / pass counts / timings against
`.kiro/specs/hotfix-visible-render/task-2-baseline-logs/`.

## Result

✅ **All preservation tests PASS on the fixed code.** Zero regressions.

```
pre-fix (task-2)        post-fix (task-3.6)
──────────────────────   ──────────────────────
22 passed (1.28s)   →   22 passed (1.33s)     preservation
 2 passed (0.62s)   →    2 passed (0.65s)     bug_condition_exploration
26 passed (0.01s)   →   26 passed (0.01s)     core-server --lib
87 passed (4.92s)   →   87 passed (4.25s)     renderer --lib
──────────────────────   ──────────────────────
137 passed (total)  →  137 passed (total)
```

Plus:

* **Task 3.2 newly-added** `desktop_window_monitor::title` unit tests —
  8 / 8 pass (not a preservation test; included for full post-fix
  coverage).
* **PE-10 Game Bar widget guard** — `git diff --stat
  monitors/game-bar-widget/` is empty AND `git ls-files --others
  --exclude-standard monitors/game-bar-widget/` is empty.
* **PE-7 IPC symbol guard** — `Producer` / `register_producer` /
  `ControlMessage::RegisterProducer` occurrences in `core-server/src/`
  total **62 lines / 62 occurrences** across 7 files (unchanged from
  the pre-hotfix layout, just relocated from `src/bin/demo-producer.rs`
  to `src/bin/demo-app.rs` with the same 4-occurrence count).

## Log files

| File | What it contains |
| --- | --- |
| `preservation-run.log` | 22 preservation tests (PBT A / A' / B / C / D + trace + 10 unit) — all PASS |
| `bug-condition-exploration-run.log` | 2 animation-and-viewport-fix exploration tests — both PASS |
| `core-server-lib-run.log` | 26 core-server lib unit tests — all PASS |
| `rust-renderer-lib-run.log` | 87 rust-renderer crate tests — all PASS |
| `desktop-window-lib-run.log` | 8 desktop-window `title::tests` (Task 3.2 new) — all PASS |
| `ipc-symbols-rg-count.log` | PE-7: IPC symbol occurrence count in `core-server/src/` |
| `game-bar-widget-guard.log` | PE-10: Game Bar widget untouched guard + preservation oracles guard |
| `demo-producer-residuals.log` | Post-fix `demo-producer` residual scan — all expected, all scoped out of Change-C |

## Per-oracle / suite breakdown (post-fix)

### PBT A (control-plane bit-identical) — `control_plane_bytes.bin`

Validates **PE-1, PE-6**. `decode(encode(msg)) == msg` AND encoded bytes
match the oracle. On fixed code:

```
test pbt_a_oracle_capture_control_plane_bytes ... ok
test pbt_a_random_control_message_roundtrip_bit_identical ... ok
```

### PBT A' (`MonitorLocalSurfaceAttached` round-trip) — `control_plane_monitor_local_surface_bytes.bin`

Validates **PE-1, PE-6, PE-7** (IPC symbol stability).

```
test pbt_a_prime_oracle_capture_monitor_local_surface_attached ... ok
test pbt_a_prime_monitor_local_surface_attached_roundtrip      ... ok
test pbt_a_prime_unknown_opcode_is_skipped_not_fatal           ... ok
```

→ confirms on-the-wire bytes of `MonitorLocalSurfaceAttached` (and all
other `ControlMessage` variants) are bit-identical (required by task
3.6 specific confirmation (a)).

### PBT B (World-only pixel equivalence) — `world_only_hashes.txt`

Validates **PE-8**. World-only rendering pixel hashes unchanged.

```
test pbt_b_oracle_capture_world_only_pixel_hashes ... ok
test pbt_b_world_stream_pixel_hash_is_deterministic ... ok
```

→ World-layer pixels are hash-equal (required by task 3.6 specific
confirmation (b)).

### PBT C (high-rate non-freeze, no unbounded growth) — `high_rate_bounds.txt`

Validates **PE-5** (no animation-stall regression) and **PE-3**.

```
test pbt_c_oracle_capture_high_rate_bounds ... ok
test pbt_c_submit_interval_and_duration_have_bounded_state ... ok
```

→ orange-block animation continues to slide under high submit rates
without unbounded growth (required by task 3.6 specific confirmation
(c); the PBT C bounds are what make this an automated rather than
visual-only guarantee for the hotfix).

### PBT D (multi-consumer independence) — `multi_consumer_independence.txt`

Validates **PE-9**.

```
test pbt_d_oracle_capture_multi_consumer_invariants ... ok
test pbt_d_multi_consumer_up_down_sequences_preserve_independence ... ok
```

### desktop-window attach trace — `desktop_window_attach_trace.txt`

Validates **PE-1**. Confirms Change-B's new `SetWindowTextW` call sites
do NOT appear in the DComp/D3D trace (correct — `SetWindowTextW` is a
User32 call, not a DComp call, so the oracle stays byte-identical).

```
test pbt_preservation_oracle_capture_desktop_window_trace ... ok
```

### Unit preservation (opcode encode/decode)

Validates **PE-6** (per-opcode bytes unchanged). 10 tests covering the
8 geometry opcodes + the full 8-opcode stream + the opcode table:

```
test unit_preservation_all_8_opcodes_in_one_stream           ... ok
test unit_preservation_clear_decodes_unchanged               ... ok
test unit_preservation_draw_line_decodes_unchanged           ... ok
test unit_preservation_fill_ellipse_decodes_unchanged        ... ok
test unit_preservation_fill_rect_decodes_unchanged           ... ok
test unit_preservation_fill_rounded_rect_decodes_unchanged   ... ok
test unit_preservation_opcode_table_is_consistent            ... ok
test unit_preservation_stroke_ellipse_decodes_unchanged      ... ok
test unit_preservation_stroke_rect_decodes_unchanged         ... ok
test unit_preservation_stroke_rounded_rect_decodes_unchanged ... ok
```

`preservation.rs` total on fixed code: **22 passed; 0 failed; 0 ignored**
(1.33 s) — matches the task-2 baseline `22 passed (1.28 s)` exactly in
count and composition.

### Bug-condition exploration replay — **PE-2**

```
test prop_1a_submit_frame_rotates_through_distinct_buffers ... ok
test prop_1b_monitor_local_fill_rect_is_visible_at_each_consumer_10_10 ... ok
```

`bug_condition_exploration.rs` total: **2 passed; 0 failed; 0 ignored**
(0.65 s) — matches task-2 baseline.

### core-server lib (26 tests) — **PE-3**

`cargo test -p core-server --lib` → **26 passed; 0 failed; 0 ignored**
(0.01 s).

Breakdown verified identical to baseline:
- 7 `ipc::cmd_decoder` tests
- 2 `renderer::mediafoundation` tests
- 6 `renderer::resources` tests
- 4 `server_task::record_render_duration_*` tests
- 7 `server_task::scan_targets_*` tests

### rust-renderer crate (87 tests) — **PE-4**

`cargo test -p renderer --lib` → **87 passed; 0 failed; 0 ignored**
(4.25 s). Same 87 tests the task-2 baseline enumerated; no regressions.

### Task 3.2 bonus — `desktop_window_monitor::title` unit tests (8 new)

Not a preservation oracle — added by Change-B as the pure-helper
unit-test layer under `format_window_title` / `AttachState`. Included
in this log set so the post-fix verification is comprehensive.

```
test title::tests::attached_dual_visual_contains_canvas_id_and_both_space_markers ... ok
test title::tests::attached_never_contains_connecting_or_reconnecting_substring ... ok
test title::tests::attached_world_only_contains_canvas_id_and_world_only_marker ... ok
test title::tests::connecting_and_reconnecting_are_distinguishable ... ok
test title::tests::connecting_is_exactly_the_initial_create_window_title ... ok
test title::tests::dual_visual_marker_only_appears_when_ml_is_true ... ok
test title::tests::every_variant_keeps_the_desktop_monitor_prefix ... ok
test title::tests::reconnecting_has_its_own_distinct_suffix ... ok
```

**8 passed; 0 failed; 0 ignored** (0.00 s)

## PE-7 IPC symbol guard (task 3.6 specific confirmation (d))

Pattern: `Producer|register_producer|ControlMessage::RegisterProducer`
Scope: `core-server/src/` (recursive).

Per-file occurrence counts (post-fix):

| File                                          | Occurrences |
| --------------------------------------------- | ----------: |
| `core-server/src/bin/demo-app.rs`             |           4 |
| `core-server/src/ipc/cmd_decoder.rs`          |           1 |
| `core-server/src/ipc/protocol.rs`             |           7 |
| `core-server/src/ipc/server.rs`               |          26 |
| `core-server/src/ipc/shmem.rs`                |           1 |
| `core-server/src/renderer/dcomp.rs`           |           2 |
| `core-server/src/server_task.rs`              |          21 |
| **Total**                                     |     **62**  |

Task 3.3's baseline recorded 4 occurrences in the pre-rename
`core-server/src/bin/demo-producer.rs`. Post-rename `demo-app.rs` has
the same 4 occurrences — **PARITY PRESERVED**. The IPC layer symbols
(`Producer` struct in `server.rs`, `ControlMessage::RegisterProducer`
in `protocol.rs`, `register_producer` in `server.rs`, etc.) were NOT
renamed by this hotfix (they are out of scope per bugfix.md 2.3 / 3.7),
and the rg-count confirms it.

## PE-10 Game Bar widget guard (task 3.6 specific confirmation (e))

```
git diff --stat monitors/game-bar-widget/ :
  (empty output — PE-10 PASS)

git ls-files --others --exclude-standard monitors/game-bar-widget/ :
  (none)
```

The Game Bar widget directory is untouched, as required by PE-10.

## Preservation oracles guard

`core-server/tests/preservation_oracles/` was authored by task 1 / task 2
as untracked files (the whole `core-server/tests/` tree shows `??` in
`git status`). Bit-for-bit oracle stability is therefore **test-attested**,
not git-attested: each PBT's oracle-capture test (`*_oracle_capture_*`)
and verify-against-oracle test (`*_roundtrip_*`, `*_pixel_hash_*`, etc.)
loads the exact bytes from disk and would fail if any byte had drifted.
All 7 oracle-capture / verify tests pass, so no oracle drifted during
Change-A / B / C / D3.

## `demo-producer` residuals

A post-fix scan for the string `demo-producer` finds matches only in:

1. `monitors/desktop-window/Cargo.toml` — 2 matches (the retained
   `desktop-demo-producer` `[[bin]]` entry + the TODO comment, both
   explicitly required by task 3.1 Change-A).
2. `monitors/desktop-window/README.md` + `src/bin/producer.rs` — 4
   matches describing the `desktop-demo-producer` binary whose rename
   is scoped to the `canvas-monitor-lifecycle` spec (out of this
   hotfix's scope).
3. `core-server/tests/hotfix_visible_render_exploration.rs` — 11
   matches in the `isBugCondition_rename` bug-condition predicate (by
   design — the test asserts `core-server/src/bin/demo-producer.rs`
   does NOT exist and the old cargo bin name is gone).

**In the target scope of Change-C (`core-server/**` excluding tests/),
there are ZERO `demo-producer` matches** — the rename-completeness
guarantee is intact. See `demo-producer-residuals.log` for the full
breakdown.

## Comparison vs the task-2 baseline

Identical counts, identical composition, comparable timings. No
preservation regression on any of PE-1..PE-10.

| Property | Baseline (unfixed) | Post-fix | Status |
| :--- | :--- | :--- | :--- |
| PE-1 (wire + trace)       | ✅ 22 preservation + trace PBT pass | ✅ same |      PASS |
| PE-2 (exploration replay) | ✅ 2 pass                           | ✅ same |      PASS |
| PE-3 (core-server --lib)  | ✅ 26 pass                          | ✅ same |      PASS |
| PE-4 (renderer --lib)     | ✅ 87 pass                          | ✅ same |      PASS |
| PE-5 (animation bounds)   | ✅ PBT C pass                       | ✅ same |      PASS |
| PE-6 (bytes unchanged)    | ✅ PBT A / A' + 10 unit pass        | ✅ same |      PASS |
| PE-7 (IPC symbols)        | ✅ 4 matches in demo-producer.rs    | ✅ 4 matches in demo-app.rs | PASS |
| PE-8 (world pixel hash)   | ✅ PBT B pass                       | ✅ same |      PASS |
| PE-9 (multi-consumer)     | ✅ PBT D pass                       | ✅ same |      PASS |
| PE-10 (Game Bar widget)   | ✅ `git diff` empty                 | ✅ `git diff` empty | PASS |

## What happens next

Task 3.6 is complete. Task 4 (checkpoint) can now confirm:

1. Task 1 / Task 3.5 exploration test passes across sub-properties 1a / 1b /
   1c / 1d on fixed code.
2. Task 2 / Task 3.6 preservation tests pass on fixed code — this log.
3. Game Bar widget is untouched.
4. `demo-app` binary compiles and runs in place of the removed
   `demo-producer` binary (task 3.3 verification).
5. `desktop-window-monitor` binary compiles under its new
   (self-consistent) name (task 3.1 verification).
