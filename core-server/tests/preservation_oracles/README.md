# Preservation Oracles

This directory contains **baseline behavior oracles** captured from the
**unfixed** `core-server` code as part of Task 2 of the
`animation-and-viewport-fix` bugfix spec
(`.kiro/specs/animation-and-viewport-fix/tasks.md`).

These oracles encode the **Preservation** property
(design.md §Correctness Properties → Property 3):

> _For any_ input satisfying `NOT isBugCondition(input)` …, the fixed Core
> SHALL produce equivalent observable output to the unfixed Core.

The files in this directory are **_the unfixed baseline_**. Task 3.7 runs the
same property tests against the _fixed_ implementation and compares them to
these files — mismatch = regression.

## Files

| File | Format | What it captures |
|------|--------|------------------|
| `control_plane_bytes.bin` | custom binary | Byte-identical `encode(msg)` output for each canonical `ControlMessage` sample (PBT A) |
| `world_only_hashes.txt` | text `seed=<u64> hash=<hex>` per line | Pixel-hash oracle for World-only command streams rendered through the software model (PBT B) |
| `desktop_window_attach_trace.txt` | text, one call per line | API-call trace structure for `desktop-window` consumer startup (Preservation 3.2) |
| `high_rate_bounds.txt` | text `key=value` per line | Structural bounds for producer-at-1000Hz scenario (PBT C) |
| `multi_monitor_independence.txt` | text | Invariants observed on multi-monitor up/down sequences (PBT D) |

## Capture / Verify Flow

The test harness (`core-server/tests/preservation.rs`) uses
`capture_or_verify_oracle()` helpers:

* **First run (files do not exist):** the tests encode/render/render on unfixed
  code, write the result to each file, and pass trivially. Commit the
  generated files so future runs have something to compare against.
* **Subsequent runs (files exist):** the tests recompute the same values and
  `assert_eq!` against the committed file contents. On unfixed code this
  passes because the implementation has not changed.
* **Task 3.7 (fixed code):** the tests recompute against the **same** oracle
  files, now on fixed code. If the Preservation property holds, values match
  and tests pass. If someone accidentally regressed a preserved behavior
  (e.g. changed `ControlMessage::encode` byte layout), the mismatch is
  surfaced as a concrete diff.

## Scope

These oracles only describe `NOT isBugCondition` inputs (Preservation
Requirements 3.1–3.9 in `bugfix.md`). Bug-condition inputs (animation stall
under steady-rate submits; MonitorLocal coordinates on multi-consumer
canvases) are handled by `core-server/tests/bug_condition_exploration.rs`
(Task 1) and its own regressions file.
