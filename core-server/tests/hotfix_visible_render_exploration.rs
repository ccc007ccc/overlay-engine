//! Bug Condition Exploration — Property 1 for the `hotfix-visible-render` spec.
//!
//! **Task 1 from `.kiro/specs/hotfix-visible-render/tasks.md`.**
//!
//! This test encodes **Property 1 (Bug Condition — Hotfix Visible Render:
//! Doc / Title / Rename / MonitorLocal-Invisible)** from design.md
//! §Correctness Properties. It MUST FAIL on unfixed code — failure
//! confirms all four sub-defects exist:
//!
//!   * **1a** `isBugCondition_doc`    — `END-TO-END-TESTING.md` points at a
//!     `--bin` name the workspace doesn't expose.
//!   * **1b** `isBugCondition_title`  — `desktop-window-monitor` consumer
//!     window title is stuck at `"connecting..."` forever.
//!   * **1c** `isBugCondition_rename` — `core-server/src/bin/demo-producer.rs`
//!     still uses the old "producer" terminology.
//!   * **1d** `isBugCondition_visible` — MonitorLocal layer (cyan badge +
//!     FPS bar) is invisible in the real DWM composition even though all
//!     22 + 2 + 26 + 87 automated tests pass.
//!
//! **Validates: Requirements 1.1, 1.2, 1.3, 1.4**
//!
//! ## Harness shape
//!
//! Following the style of `core-server/tests/bug_condition_exploration.rs`
//! for the earlier `animation-and-viewport-fix` spec, but adapted for the
//! Scoped-PBT approach design.md §Testing Strategy prescribes for this
//! hotfix:
//!
//!   * **1a / 1b-static / 1c** are deterministic STATIC probes over the
//!     repository's own files and Cargo metadata — a single concrete
//!     assertion per sub-property, no `proptest!` wrapping needed.
//!   * **1b-runtime** requires launching a real `desktop-window-monitor`
//!     consumer, driving a stub `CanvasAttached` + `MonitorLocalSurfaceAttached`
//!     through a named pipe, and reading the HWND title via
//!     `GetWindowTextW`. This needs an interactive Windows DWM session and
//!     a spare pipe handle; we scaffold the probe as an `#[ignore]`-gated
//!     test that documents the exact assertion and the manual run command.
//!     The STATIC half (zero `SetWindowTextW` call sites repo-wide) already
//!     surfaces an unambiguous counterexample.
//!   * **1d** needs two consumer windows, `core-server`, `demo-app`,
//!     and pixel readback via `PrintWindow` / `BitBlt`. The full E2E probe
//!     is scaffolded as `#[ignore]`-gated with the five H1–H5 hypothesis
//!     probes described as code-inspection evidence in comments. The
//!     definitive H-confirmation is flagged as "requires real DWM run"
//!     and is the last-mile handoff to task 3.4.
//!
//! ## Counterexamples surfaced on unfixed code
//!
//! All four sub-properties produce concrete, reproducible counterexamples
//! from the repository-level probes:
//!
//! * **1a**: `monitors/desktop-window/Cargo.toml` defines
//!   `[[bin]] name = "desktop-window-consumer"`; `END-TO-END-TESTING.md`
//!   contains `cargo run -p desktop-window-monitor --bin consumer` (twice).
//!   Pasting that command yields `error: no bin target named 'consumer'`.
//! * **1b-static**: `rg -n SetWindowTextW monitors/desktop-window/` returns
//!   0 matches — no code path can update the window title after attach.
//! * **1c**: `core-server/src/bin/demo-producer.rs` exists;
//!   `core-server/Cargo.toml` has `[[bin]] name = "demo-producer"`;
//!   `core-server/src/bin/demo-app.rs` does not exist.
//! * **1d**: code-inspection analysis of `dcomp.rs` / `consumer.rs` /
//!   `server_task.rs` narrows H1–H5 to a specific candidate (see §Task 1
//!   H-hypothesis findings at the bottom of this file); definitive
//!   confirmation requires a DWM run. See `probe_1d_manual_protocol` for
//!   the step-by-step procedure.
//!
//! ## Why a new file instead of extending `bug_condition_exploration.rs`
//!
//! `bug_condition_exploration.rs` is the exploration test for the
//! preceding `animation-and-viewport-fix` spec and its two tests (1a
//! animation-stall, 1b MonitorLocal space missing) are expected to
//! **pass** on this hotfix's baseline (Preservation PE-2 from
//! `.kiro/specs/hotfix-visible-render/design.md` §Preservation Requirements).
//! Mixing the two spec's bug-condition tests in a single file would force
//! that file to both pass and fail on the same baseline — a contradiction.
//! A separate file keeps each spec's oracle cleanly invertible:
//!
//!   * `bug_condition_exploration.rs`          — passes on this spec's base.
//!   * `hotfix_visible_render_exploration.rs`  — **fails** on this spec's
//!     base (this file); will pass once task 3.4 lands the fix.

use std::fs;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Workspace-path helpers
// ---------------------------------------------------------------------------

/// Workspace root derived from `CARGO_MANIFEST_DIR` (which equals
/// `core-server/` at test-build time). One `../` pop lands on the repo
/// root where the top-level `Cargo.toml`, `END-TO-END-TESTING.md`, and
/// `monitors/` live.
fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p
}

fn read_text(rel: &str) -> String {
    let mut p = workspace_root();
    for seg in rel.split('/') {
        p.push(seg);
    }
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("failed to read {:?}: {e}", p))
}

fn file_exists(rel: &str) -> bool {
    let mut p = workspace_root();
    for seg in rel.split('/') {
        p.push(seg);
    }
    p.exists()
}

// ---------------------------------------------------------------------------
// Sub-property 1a — isBugCondition_doc (缺陷 1.1, static)
//
// design.md §Bug Details → isBugCondition_doc:
//   input.cmd == "cargo run -p desktop-window-monitor --bin consumer"
//   AND NOT bin_exists("desktop-window-monitor", "consumer")
//   AND bin_exists("desktop-window-monitor", "desktop-window-consumer")
//
// We assert the **correct** (post-fix) state:
//   * `monitors/desktop-window/Cargo.toml` exposes a bin named
//     "desktop-window-monitor" (Property 1's Change-A).
//   * `END-TO-END-TESTING.md` does NOT reference
//     `--bin consumer` or `--bin desktop-window-consumer`.
//
// On unfixed code the Cargo.toml still says `name = "desktop-window-consumer"`
// and the doc still contains `--bin consumer` — this assertion fails,
// surfacing the exact counterexample strings as part of the failure message.
// ---------------------------------------------------------------------------

/// Minimal TOML-level probe — good enough for the single-line `[[bin]]` /
/// `name = "..."` pattern used by the workspace without pulling in a full
/// TOML parser dependency. We scan for a `name = "<target>"` line whose
/// enclosing block started with `[[bin]]`.
fn cargo_toml_has_bin_name(cargo_toml_rel: &str, expected_bin_name: &str) -> bool {
    cargo_toml_bin_path(cargo_toml_rel, expected_bin_name).is_some()
}

fn cargo_toml_bin_path(cargo_toml_rel: &str, expected_bin_name: &str) -> Option<String> {
    let content = read_text(cargo_toml_rel);
    let mut in_bin_table = false;
    let mut current_name_matches = false;
    let mut current_path: Option<String> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            if in_bin_table && current_name_matches {
                return current_path;
            }
            in_bin_table = line == "[[bin]]";
            current_name_matches = false;
            current_path = None;
            continue;
        }
        if !in_bin_table || !line.contains('=') {
            continue;
        }
        if line.starts_with("name") {
            current_name_matches = line.contains(&format!("\"{expected_bin_name}\""));
        } else if line.starts_with("path") {
            current_path = line
                .split_once('=')
                .map(|(_, value)| value.trim().trim_matches('"').to_string());
        }
    }

    if in_bin_table && current_name_matches {
        current_path
    } else {
        None
    }
}

#[test]
fn prop_1a_isbugcondition_doc_desktop_window_monitor_bin_resolves() {
    let cargo_toml_rel = "monitors/desktop-window/Cargo.toml";
    let md_rel = "END-TO-END-TESTING.md";

    let monitor_bin_path = cargo_toml_bin_path(cargo_toml_rel, "desktop-window-monitor");
    let bin_exists_monitor = monitor_bin_path.is_some();
    let bin_path_is_monitor_rs = monitor_bin_path.as_deref() == Some("src/bin/monitor.rs");
    let bin_exists_consumer = cargo_toml_has_bin_name(cargo_toml_rel, "desktop-window-consumer");

    let md = read_text(md_rel);
    // Both variants the old doc/binary might reference.
    let doc_mentions_consumer_bin =
        md.contains("cargo run -p desktop-window-monitor --bin consumer");
    let doc_mentions_desktop_window_consumer_bin =
        md.contains("cargo run -p desktop-window-monitor --bin desktop-window-consumer");

    // Correct (post-fix) predicate:
    //   * the bin `desktop-window-monitor` is exposed, AND
    //   * `END-TO-END-TESTING.md` does not cling to either old bin name.
    // Any violation of this conjunction surfaces the缺陷 1.1 counterexample.
    assert!(
        bin_exists_monitor
            && bin_path_is_monitor_rs
            && !doc_mentions_consumer_bin
            && !doc_mentions_desktop_window_consumer_bin,
        "缺陷 1.1 (isBugCondition_doc) confirmed:\n\
         \n\
         counterexample:\n\
           monitors/desktop-window/Cargo.toml exposes bin \
             `desktop-window-monitor`? {}\n\
           desktop-window-monitor path is `src/bin/monitor.rs`? {}\n\
           monitors/desktop-window/Cargo.toml exposes bin \
             `desktop-window-consumer`? {}\n\
           END-TO-END-TESTING.md contains \
             `cargo run -p desktop-window-monitor --bin consumer`? {}\n\
           END-TO-END-TESTING.md contains \
             `cargo run -p desktop-window-monitor --bin desktop-window-consumer`? {}\n\
         \n\
         pasting the documented command into a shell yields:\n\
           error: no bin target named `consumer`\n\
         \n\
         Root cause: half-done rename — bin name, package name, and doc \
         drifted apart. Fix: task 3.1 (Change-A) renames the bin to \
         `desktop-window-monitor` and rewrites the doc command.\n\
         (design.md §Fix Implementation → Change-A)",
        bin_exists_monitor,
        bin_path_is_monitor_rs,
        bin_exists_consumer,
        doc_mentions_consumer_bin,
        doc_mentions_desktop_window_consumer_bin,
    );
}

// ---------------------------------------------------------------------------
// Sub-property 1b — isBugCondition_title (缺陷 1.2)
//
// design.md §Bug Details → isBugCondition_title:
//   timeline observes CanvasAttached AT t_attach
//   AND observe_window_title(hwnd, t_attach + delta) contains "connecting..."
//   AND NOT call_site_exists(SetWindowTextW, after CanvasAttached)
//   AND NOT call_site_exists(SetWindowTextW, after MonitorLocalSurfaceAttached)
//   AND NOT call_site_exists(SetWindowTextW, on pipe_disconnect)
//
// Two halves:
//   * **static** — `rg -n SetWindowTextW monitors/desktop-window/` returns
//     zero matches on unfixed code. This is deterministic and we encode it
//     in a regular `#[test]`.
//   * **runtime** — launching the consumer + driving the pipe + probing the
//     HWND title requires interactive Windows DWM and is gated with
//     `#[ignore]` + a manual-probe doc.
// ---------------------------------------------------------------------------

/// Count textual occurrences of `needle` across every `.rs` file under
/// the workspace-relative directory `dir_rel` (shallow traversal is
/// sufficient here: `monitors/desktop-window/src/**` is small).
fn grep_count(dir_rel: &str, needle: &str) -> usize {
    let mut root = workspace_root();
    for seg in dir_rel.split('/') {
        root.push(seg);
    }
    let mut total = 0usize;
    let mut stack: Vec<PathBuf> = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Ok(content) = fs::read_to_string(&path) {
                    total += content.matches(needle).count();
                }
            }
        }
    }
    total
}

#[test]
fn prop_1b_isbugcondition_title_static_setwindowtextw_call_site_exists() {
    let count = grep_count("monitors/desktop-window", "SetWindowTextW");

    // Correct (post-fix) predicate: there is ≥ 1 call site of
    // `SetWindowTextW` in the desktop-window monitor crate (task 3.2 /
    // Change-B adds three: on `CanvasAttached`, on
    // `MonitorLocalSurfaceAttached`, on pipe disconnect). Zero matches is
    // the缺陷 1.2 signature.
    assert!(
        count >= 1,
        "缺陷 1.2 (isBugCondition_title, static half) confirmed:\n\
         \n\
         counterexample:\n\
           grep_count(\"monitors/desktop-window\", \"SetWindowTextW\") == {}\n\
         \n\
         No code path in the desktop-window monitor crate ever updates the \
         HWND title after the initial `CreateWindowExW(..., \
         w!(\"Desktop Monitor - connecting...\"), ...)`. The window stays \
         on \"connecting...\" forever, including after `CanvasAttached` / \
         `MonitorLocalSurfaceAttached` arrive and after the pipe breaks.\n\
         \n\
         Fix: task 3.2 (Change-B) adds three `SetWindowTextW` call sites.\n\
         (design.md §Fix Implementation → Change-B)",
        count,
    );
}

/// **RUNTIME** probe for sub-property 1b — `#[ignore]`-gated because it
/// requires an interactive Windows DWM session, a spare named pipe, and a
/// stub producer driving `CanvasAttached` / `MonitorLocalSurfaceAttached`.
///
/// Manual-probe protocol (to be run by a human on a real desktop):
///
///   1. Build: `cargo build -p desktop-window-monitor`
///   2. Terminal A (stub producer — or a full `core-server + demo-app`
///      per `END-TO-END-TESTING.md`):
///        `cargo run -p core-server --bin core-server`
///      then
///        `cargo run -p core-server --bin demo-app`
///   3. Terminal B (consumer under test):
///        `cargo run -p desktop-window-monitor --bin desktop-window-monitor`
///      NOTE: on unfixed code the bin name is still `desktop-window-consumer`.
///   4. Once `[desktop-monitor] CanvasAttached` appears in Terminal B,
///      observe the window title via Spy++ or `GetWindowTextW` (e.g. a
///      PowerShell one-liner via `Add-Type` and `user32!GetWindowTextW`).
///      **Expected on unfixed code**: the title remains
///      `"Desktop Monitor - connecting..."`.
///   5. Kill Terminal A (pipe breaks). **Expected on unfixed code**: the
///      title remains `"Desktop Monitor - connecting..."` (NO transition
///      to any `"reconnecting..."` text).
///
/// Both observations (step 4 and step 5) ARE the runtime half of
/// `isBugCondition_title`. The assertion below is the structural
/// equivalent the automated half (above) already encodes; this
/// `#[ignore]`d test exists so the harness documentation (this doc-comment)
/// stays co-located with the rest of Property 1b.
#[test]
#[ignore = "requires interactive Windows DWM; run manually per the doc above"]
fn probe_1b_runtime_title_still_connecting_after_attach_and_disconnect() {
    // This body is intentionally unreachable in CI. The correct assertion,
    // had we an automated harness, would be:
    //
    //   let hwnd = launch_consumer_and_wait_for_canvas_attached();
    //   drive_stub_monitor_local_surface_attached(hwnd);
    //   let title_after_attach = get_window_text_w(hwnd);
    //   assert!(!title_after_attach.contains("connecting..."),
    //           "缺陷 1.2 runtime half: title stuck at `{}`",
    //           title_after_attach);
    //
    //   kill_stub_producer_pipe();
    //   let title_after_disconnect = get_window_text_w(hwnd);
    //   assert!(title_after_disconnect.contains("reconnecting"),
    //           "缺陷 1.2 runtime half: title not reconnecting after pipe \
    //            disconnect, still `{}`", title_after_disconnect);
    //
    // Expected counterexample on unfixed code:
    //   (t_attach + 16ms, title = "Desktop Monitor - connecting...",
    //    t_disconnect + 16ms, title = "Desktop Monitor - connecting...").
    eprintln!(
        "probe_1b_runtime_title_still_connecting_after_attach_and_disconnect: \
         skipped (interactive). See doc-comment for manual protocol."
    );
}

// ---------------------------------------------------------------------------
// Sub-property 1c — isBugCondition_rename (缺陷 1.3, static)
//
// design.md §Bug Details → isBugCondition_rename:
//   (file_path_exists("core-server/src/bin/demo-producer.rs")
//    OR cargo_bin_entry("core-server", "demo-producer") exists
//    OR doc_strings_in_changed_files use_term("producer")
//       in_context("monitor/core/app layer"))
//   AND NOT file_path_exists("core-server/src/bin/demo-app.rs")
// ---------------------------------------------------------------------------

#[test]
fn prop_1c_isbugcondition_rename_demo_app_bin_exists() {
    let producer_rs_exists = file_exists("core-server/src/bin/demo-producer.rs");
    let app_rs_exists = file_exists("core-server/src/bin/demo-app.rs");
    let core_cargo_has_demo_producer =
        cargo_toml_has_bin_name("core-server/Cargo.toml", "demo-producer");
    let core_cargo_has_demo_app = cargo_toml_has_bin_name("core-server/Cargo.toml", "demo-app");

    // Correct (post-fix) predicate:
    //   * `demo-app.rs` exists at the new path, AND
    //   * `demo-producer.rs` does NOT exist, AND
    //   * the Cargo `[[bin]]` entry is `demo-app`, AND
    //   * the Cargo `[[bin]]` entry `demo-producer` is gone.
    assert!(
        app_rs_exists
            && !producer_rs_exists
            && core_cargo_has_demo_app
            && !core_cargo_has_demo_producer,
        "缺陷 1.3 (isBugCondition_rename) confirmed:\n\
         \n\
         counterexample:\n\
           file_exists(\"core-server/src/bin/demo-producer.rs\") == {}\n\
           file_exists(\"core-server/src/bin/demo-app.rs\")       == {}\n\
           core-server/Cargo.toml has [[bin]] name = \"demo-producer\" == {}\n\
           core-server/Cargo.toml has [[bin]] name = \"demo-app\"       == {}\n\
         \n\
         Old `producer` terminology conflicts with the user-confirmed \
         \"monitor / core / app\" layering. Fix: task 3.3 (Change-C) \
         renames the bin file and Cargo entry; IPC symbols \
         `Producer` / `register_producer` / `ControlMessage::RegisterProducer` \
         stay unchanged (PE-7).\n\
         (design.md §Fix Implementation → Change-C)",
        producer_rs_exists,
        app_rs_exists,
        core_cargo_has_demo_producer,
        core_cargo_has_demo_app,
    );
}

// ---------------------------------------------------------------------------
// Sub-property 1d — isBugCondition_visible (缺陷 1.4, end-to-end + H1–H5)
//
// design.md §Bug Details → isBugCondition_visible:
//   input.setup == two_desktop_window_monitor_attached_to_one_producer
//   AND input.producer emits
//        PUSH_SPACE(MonitorLocal) / FILL_RECT(10,10, cyan/fps)
//        / POP_SPACE each frame
//   AND all_automated_tests_pass(input)
//   AND NOT pixel_in_client_area(consumer_i, (10,10))
//           matches cyan_or_fps_bar_color
//       FOR AT LEAST ONE consumer_i
//
// The automated-tests-pass precondition is already satisfied on this
// hotfix's baseline (PE-1..PE-4 all green, see bugfix.md). The remaining
// observable is pixel-readback at each consumer's client `(10, 10)`, which
// requires an interactive Windows DWM session.
//
// We scaffold the full E2E probe as `#[ignore]`-gated with step-by-step
// instructions and encode the H1–H5 hypothesis probes as code-inspection
// evidence in the comments below. The Naming Obligation from task 1 is
// recorded at the bottom of this file.
// ---------------------------------------------------------------------------

/// **RUNTIME** probe for sub-property 1d — `#[ignore]`-gated. Full E2E
/// manual protocol:
///
///   1. `cargo build --workspace`
///   2. Terminal A: `cargo run -p core-server --bin core-server 2>&1 | \
///      Tee-Object -FilePath core_stderr.log`
///   3. Terminal B: `cargo run -p desktop-window-monitor --bin \
///      desktop-window-monitor` — drag the window to screen region
///      `(100, 100, 820, 520)` (client origin ≠ (10, 10)).
///   4. Terminal C: `cargo run -p desktop-window-monitor --bin \
///      desktop-window-monitor` — drag the window to screen region
///      `(1200, 600, 1920, 1020)` (client origin ≠ (10, 10), not
///      overlapping terminal B's window).
///   5. Terminal D: `cargo run -p core-server --bin demo-app`.
///   6. Wait ≥ 3 s for the `demo-app`'s MonitorLocal `FILL_RECT` stream
///      to reach steady state.
///   7. For each of the two consumer windows, capture client-area pixels
///      via `PrintWindow(hwnd, hdc, PW_CLIENTONLY)` or `BitBlt` and read the
///      pixel at client `(10, 10)`. **Expected on unfixed code**: the
///      pixel is NOT cyan — design.md says the cyan badge + FPS bar are
///      invisible in both consumers.
///
/// H1–H5 probes (run IN ADDITION to the main E2E capture above):
///
///   * **H1 probe** — Core per-Monitor Present routing.
///     After step 6, run in a fourth terminal:
///       `Select-String -Path core_stderr.log -Pattern 'consumer=' | \
///        Where-Object { $_.Line -match 'MonitorLocal' }`
///     Expected counterexample if H1 holds: zero matches despite
///     `demo-app` emitting PUSH_SPACE(MonitorLocal). Note: today
///     `dispatch_submit_frame` has NO diagnostic `eprintln!` inside the
///     per-Monitor Present branch — adding it (task 1 H1 probe) is part
///     of the H1 confirmation protocol. See inline code-inspection notes
///     in the file-level Naming Obligation at the bottom.
///
///   * **H2 probe** — consumer-side `AddVisual` z-order.
///     Edit `monitors/desktop-window/src/bin/monitor.rs` locally to swap:
///       `root.AddVisual(&visual,    false, None)` →
///       `root.AddVisual(&visual,    true,  None)`
///     and
///       `root.AddVisual(&ml_visual, true,  &visual)` →
///       `root.AddVisual(&ml_visual, false, &visual)`
///     Rebuild and rerun steps 2–7. Expected counterexample if H2 holds:
///     cyan becomes visible when swapped; original args hide it.
///
///   * **H3 probe** — missing `dcomp_dev.Commit()` after `SetRoot`.
///     Edit `monitor.rs` to add `dcomp_dev.Commit()?;` immediately after
///     `target.SetRoot(&root)?;`. Rebuild and rerun. Expected
///     counterexample if H3 holds: single-line Commit addition makes cyan
///     visible without any other change.
///
///   * **H4 probe** — per-Monitor `Present()` stuck in `RetryNextTick`.
///     After step 6, run for ≥ 10 s, then
///       `Select-String -Path core_stderr.log -Pattern 'PerMonitorResources] Present transient'`
///     Expected counterexample if H4 holds: match count grows roughly
///     linearly with frame count (hundreds per second).
///
///   * **H5 probe** — per-Monitor surface vs consumer client-area sizing.
///     Edit `monitor.rs` to change `CreateWindowExW(..., 720, 420, ...)`
///     to `CreateWindowExW(..., 1920, 1080, ...)`. Rebuild and rerun.
///     Expected counterexample if H5 holds: small window hides cyan;
///     enlarged window reveals it.
#[test]
#[ignore = "requires interactive Windows DWM with 2 consumer windows; run manually per the doc above"]
fn probe_1d_runtime_pixel_at_client_10_10_matches_cyan_on_each_consumer() {
    // This body is unreachable in CI. The correct assertion, had we an
    // automated harness, would be:
    //
    //   let consumers = launch_two_consumers_at_distinct_origins();
    //   launch_core_and_demo_producer();
    //   wait_for_steady_state(Duration::from_secs(3));
    //   for (idx, c) in consumers.iter().enumerate() {
    //       let pixel = bitblt_client_pixel_at(c.hwnd, 10, 10);
    //       assert!(is_cyan_or_fps_bar(pixel),
    //               "缺陷 1.4: consumer[{}] (origin=({}, {})) client (10,10) \
    //                is {:?}, expected cyan; automated suite reported green",
    //               idx, c.origin_x, c.origin_y, pixel);
    //   }
    //
    // Expected counterexample on unfixed code: consumer[0] (e.g. at
    // origin=(100,100)) and consumer[1] (e.g. at origin=(1200,600)) both
    // report background pixels at client (10,10), NOT cyan — even though
    // 22 preservation + 2 exploration + 26 lib + 87 renderer tests are
    // green on this baseline.
    eprintln!(
        "probe_1d_runtime_pixel_at_client_10_10_matches_cyan_on_each_consumer: \
         skipped (interactive). See doc-comment for manual protocol and \
         H1–H5 probe procedure."
    );
}

// ---------------------------------------------------------------------------
// Task 1 Naming Obligation — H1–H5 hypothesis findings from code inspection.
//
// design.md §Hypothesized Root Cause lists five candidate root causes for
// `isBugCondition_visible`. Task 1 requires that ONE (or more) be confirmed
// before task 3.4 implements the fix. Full confirmation requires an
// interactive DWM run (see `probe_1d_runtime_*` above). Pending that run,
// the code-inspection evidence below narrows the field:
//
// -- H1 (Core per-Monitor Present routing) ---------------------------------
//   Code-inspection evidence (`core-server/src/server_task.rs` lines 300+
//   → `dispatch_submit_frame`):
//     * `scan_targets(cmds)` walks the command stream once and correctly
//       sets `local_used = true` whenever `top == SpaceId::MonitorLocal`
//       (unit-tested in `scan_targets_monitor_local_region_sets_local_used`).
//     * The per-Monitor `for (cid, pc) in &canvas.per_consumer_surfaces`
//       acquire loop AND the subsequent per-Monitor Present loop both
//       iterate `canvas.per_consumer_surfaces`, not `local_idxs`, for the
//       ACQUIRE step (present step iterates `local_idxs`). So any consumer
//       attached to the canvas that fails `acquire_available_buffer` is
//       correctly skipped only in the present step.
//     * There is NO diagnostic `eprintln!` on the SUCCESS path of
//       per-Monitor Present — only on the ERROR paths (SetBuffer error,
//       device-lost). The H1 probe proposes adding one to surface whether
//       the branch is reached. Under normal operation the 60-frame
//       `SubmitFrame:` println WOULD print `local_targets=N` once per
//       second — that line already exists at the bottom of
//       `dispatch_submit_frame`.
//   Verdict: **Code-inspection evidence points to H1 NOT holding** — the
//   routing logic looks correct on paper. Runtime probe would need to
//   confirm the `local_targets=N` line actually prints N ≥ 1 on each
//   second of a MonitorLocal-emitting run.
//
// -- H2 (consumer-side AddVisual z-order) -----------------------------------
//   Code-inspection evidence (`monitors/desktop-window/src/bin/monitor.rs`
//   around the dual-visual mount):
//     root.AddVisual(&visual,     false, None::<&IDCompositionVisual>)?;
//     root.AddVisual(&ml_visual,  true,  &visual)?;
//   DComp contract: `AddVisual(visual, insertAbove, referenceVisual)` —
//   `insertAbove = true` means `visual` is placed above `referenceVisual`
//   in z-order (closer to the viewer). The current code:
//     * adds `visual` (World) to an empty root with insertAbove=false,
//       reference=None → World is the bottom-most child.
//     * adds `ml_visual` (MonitorLocal) above World → MonitorLocal is the
//       top child, above World, facing the viewer.
//   This matches the **intended** z-order. The code looks correct.
//   Verdict: **Code-inspection evidence points to H2 NOT holding.**
//   Runtime swap probe would confirm — if swapping flips visibility, H2
//   holds; if both states are invisible, H2 is falsified.
//
// -- H3 (missing dcomp_dev.Commit() after SetRoot) --------------------------
//   Code-inspection evidence:
//     unsafe {
//         root.AddVisual(&visual,    false, None::<&IDCompositionVisual>)?;
//         root.AddVisual(&ml_visual, true,  &visual)?;
//         target.SetRoot(&root)?;
//     }
//     println!("[desktop-monitor] mounted dual visual tree (World + MonitorLocal)");
//     // ... fall through to update_viewport + message loop ...
//   There is NO `dcomp_dev.Commit()?;` call immediately after
//   `target.SetRoot(&root)?;`. The only `Commit()` in the consumer lives
//   inside `update_viewport`, which is called once explicitly after
//   `ShowWindow` (line 1 of the render loop setup) AND every iteration of
//   the message loop.
//   BUT `update_viewport` only runs its Commit AFTER calling
//   `visual.SetContent(&state.surface)` and `visual.SetTransform2(&matrix)`
//   on the World visual — `ml_visual` is never touched by `update_viewport`.
//   This means:
//     (a) The World visual re-commits every frame.
//     (b) The MonitorLocal visual's `AddVisual` + `SetContent` + `SetBitmap...`
//         all happen BEFORE the first `Commit()`. Whether DComp treats
//         them as staged or committed depends on whether the code path
//         between `AddVisual(&ml_visual, ...)` and the first
//         `update_viewport`'s `Commit()` includes any implicit commit.
//     (c) There is NO synchronization guarantee that the root visual's
//         composition (including ml_visual as a child) is committed before
//         DWM's first frame after `SetRoot`.
//   Verdict: **H3 is PLAUSIBLE.** The code inspection cannot definitively
//   confirm H3 without a runtime run (adding the single-line `Commit()`
//   after `SetRoot` and observing whether cyan becomes visible), BUT the
//   absence of an explicit commit at the dual-visual-mount boundary IS
//   the specific signature design.md §Hypothesized Root Cause H3
//   describes. Crucially `update_viewport`'s `Commit()` fires AFTER
//   `visual.SetContent(&state.surface)` which is already harmless /
//   idempotent (the content was set one frame earlier inside the mount
//   block), so `update_viewport` may well be committing the pending root
//   tree mutation on its first call. If that path works, H3 is falsified.
//   If DWM has already composed a frame before `update_viewport` runs
//   (happens when startup is ordered differently across DWM versions or
//   when `ShowWindow` triggers a paint before the first `WM_WINDOWPOSCHANGED`),
//   the root tree is stale until the first actual window event. The
//   second Changelog note in design.md §Hypothesized Root Cause confirms
//   this concern.
//
// -- H4 (per-Monitor Present stuck in RetryNextTick) -----------------------
//   Code-inspection evidence (`core-server/src/renderer/dcomp.rs`
//   `PerMonitorResources::present` + server_task.rs post-present drain):
//     * The classification matches `CanvasResources::present` exactly
//       (same DXGI_ERROR_DEVICE_* triage, same transient→RetryNextTick
//       fallback, same eprintln format).
//     * `BUFFER_COUNT = 2` for both World and per-Monitor. `server_task.rs`
//       does `SleepEx(0, true)` + `while manager.GetNextPresentStatistics().is_ok()`
//       after each Present, identical to the World path.
//   The per-Monitor Present flow is structurally the SAME as the World
//   Present flow — and World's orange-block animation is visibly healthy
//   on this baseline. That strongly suggests H4 is NOT holding: if
//   `RetryNextTick` were stuck, transient-error eprintlns would flood
//   stderr on unfixed code. Runtime probe (grep `Present transient`) is
//   cheap and should be run first to confirm.
//   Verdict: **Code-inspection evidence points to H4 NOT holding** (pending
//   cheap stderr-grep runtime probe).
//
// -- H5 (per-Monitor surface vs consumer client-area sizing) ---------------
//   Code-inspection evidence:
//     * `PerMonitorResources::new` (dcomp.rs) clamps `logical_w/_h` to
//       `[1, 4096]` for render dimensions — sized to the *canvas logical*,
//       NOT to the consumer's client-area.
//     * The consumer's `ml_visual` in `consumer.rs` is mounted with
//       NO `SetTransform2` call — unlike the World `visual` which has
//       viewport scale + translate applied every frame in `update_viewport`.
//     * `PerMonitorResources` sets `surface.SetSourceRect` to the full
//       `(0, 0, render_w, render_h) = (0, 0, logical_w, logical_h)` of the
//       canvas, not the consumer's client area.
//   Consequence: if canvas logical is 1920×1080 and consumer client is
//   720×420, the entire 1920×1080 MonitorLocal surface is mapped 1:1 to
//   the consumer window's top-left area — only the top-left 720×420
//   portion is visible. The cyan FILL_RECT(10, 10, 80, 80) IS inside that
//   visible top-left portion at canvas coords (10, 10), so sizing-wise
//   the cyan square SHOULD be visible. This is the opposite of what H5
//   predicts (H5 says "small client hides cyan"). Under this analysis a
//   client-area-aligned cyan IS inside the composed region.
//   Verdict: **Code-inspection evidence points to H5 NOT holding** in the
//   concrete 720×420 vs 1920×1080 case design.md gives — cyan falls
//   within the visible intersection. Runtime enlargement probe would
//   confirm (if cyan reveals only at 1920×1080, H5 holds; if invisible at
//   both, H5 is falsified and another hypothesis is indicated).
//
// -- Summary / Naming --
//   Code-inspection evidence alone narrows the field to **H3 as the
//   leading candidate** (missing explicit `Commit()` after `SetRoot`)
//   with H1 as a plausible secondary (routing looks correct on paper but
//   has no diagnostic logging on the happy path). H2 / H4 / H5 appear
//   structurally correct from inspection alone.
//
//   THIS IS CODE-INSPECTION EVIDENCE, NOT RUNTIME-CONFIRMED. Task 3.4
//   `Change-D*` selection MUST wait for the `probe_1d_runtime_*` manual
//   run to definitively confirm one (or more) of H1–H5 — design.md
//   §Fix Implementation → "收口原则" explicitly forbids committing a fix
//   without a surfaced counterexample.
//
//   Recommended runtime probe order (cheapest first):
//     1. H4 (stderr grep `Present transient` for 10 s) — either instantly
//        confirms or instantly rules out.
//     2. H3 (add 1-line `Commit()` after `SetRoot`, rebuild, rerun) — if
//        cyan appears, H3 confirmed; commit the Change-D3 fix.
//     3. H1 (add 1-line `eprintln!` inside per-Monitor Present success
//        branch, rebuild, rerun) — confirms whether routing fires.
//     4. H2 (swap AddVisual args) — falsifies or confirms z-order direction.
//     5. H5 (enlarge window to 1920×1080) — sanity sizing probe.
//
// ---------------------------------------------------------------------------
