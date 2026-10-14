//! Scan commands (spec §4; doc 03 M3.7): `start_scan(target)` and
//! `get_status`, plus the `scan-progress` (150 ms) and `scan-done`
//! events and the §15 dev hooks.
//!
//! Starting a new scan cancels the old one cooperatively (the old
//! thread exits without swapping its tree). The finished tree is
//! `Arc<Tree>` behind the state `RwLock`; the OLD tree drops on a
//! background thread so the UI never stalls freeing a million nodes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use diskgenie_core::scan::node::Tree;
use diskgenie_core::scan::scanner::{Progress, ScanOutcome, ScanTarget};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::platform::HostPlatform;
use crate::state::{tree_cache_key, AppState, ScanHandle};

/// How often the ticker emits `scan-progress` (spec §4: 150 ms).
const TICK_MS: Duration = Duration::from_millis(150);

/// The status response for the Explore idle/scanning/done states.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    /// Current scan generation.
    pub generation: u64,
    /// Whether a scan is running.
    pub scanning: bool,
    /// Whether a finished tree exists.
    pub has_tree: bool,
    /// Live progress (valid while scanning; last values after).
    pub progress: Progress,
    /// Sticky last-completed-scan record (reconcile path — see
    /// `state::DoneRecord`).
    pub last_done: Option<LastDone>,
}

/// The `get_status` replay of the last completed scan (same shape as
/// the `scan-done` payload).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastDone {
    /// The generation that finished.
    pub generation: u64,
    /// Root stats when the scan succeeded: `(logical, on_disk, files,
    /// folders)`.
    pub stats: Option<(u64, u64, u64, u64)>,
    /// User-readable reason when the scan did not produce a tree.
    pub error: Option<String>,
}

/// The `scan-done` payload.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanDone {
    /// The generation that finished.
    pub generation: u64,
    /// Root stats when the scan succeeded: `(logical, on_disk, files,
    /// folders)`.
    pub stats: Option<(u64, u64, u64, u64)>,
    /// User-readable reason when the scan did not produce a tree.
    pub error: Option<String>,
}

/// The `start_scan` result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStart {
    /// The generation this scan owns (request tagging).
    pub generation: u64,
    /// True when NO scan ran: a cached tree for the exact target was
    /// restored instantly (the session-14 drive-flip fix). The UI
    /// adopts the done state directly — no scanning flash.
    pub restored: bool,
    /// Root stats, present only when `restored` (fresh scans report
    /// through the `scan-done` event once they finish).
    pub stats: Option<(u64, u64, u64, u64)>,
}

/// Parse a scan target string: `"ThisPC"`, a drive root, or a folder.
#[must_use]
pub fn parse_target(s: &str) -> ScanTarget {
    if s.eq_ignore_ascii_case("thispc") || s.eq_ignore_ascii_case("this pc") {
        return ScanTarget::ThisPc;
    }
    // The separator follows the target's own family ("/" targets stay
    // POSIX; "C:\…" targets stay Windows).
    let trimmed = s.trim_end_matches(['\\', '/']);
    if cfg!(target_os = "macos") {
        ScanTarget::Folder(format!("{trimmed}/"))
    } else {
        ScanTarget::Folder(format!("{trimmed}\\"))
    }
}

/// Start a scan of `target` (`"ThisPC"` or a path). Cancels any running
/// scan. Emits `scan-progress` every 150 ms and `scan-done` at the end.
///
/// Instant restore (session 14, the drive-flip fix): when the target
/// was scanned before and its tree survives in the flip cache, the
/// cached snapshot swaps back in with a fresh generation — no walk,
/// no ticker, no scanning state. `force` (the UI's explicit Rescan
/// affordance) skips the restore AND purges the target's stale entry
/// so the fresh tree replaces it.
///
/// # Errors
/// String error when the license gate refuses, or when a contended
/// tree restore's background copy fails.
#[tauri::command]
/// The scan lifecycle's ordering invariants live ONCE, in the helpers:
/// `cancel_previous` (the prelude), `complete_scan` (swap → caches →
/// sticky record → event), `record_scan_failure` (the failure pair).
/// This command stays a thin orchestration: gate → cancel → restore
/// check → spawn the engine + ticker → register the handle.
#[allow(clippy::too_many_lines)]
pub async fn start_scan(
    target: String,
    force: Option<bool>,
    app: AppHandle,
    state: State<'_, AppState>,
    platform: State<'_, Arc<HostPlatform>>,
    license: State<'_, crate::commands::license::LicenseManager>,
) -> Result<ScanStart, String> {
    // The hard license gate (docs §2 L6): the operation itself refuses —
    // the UI lock and this gate protect each other.
    crate::commands::license::require_licensed(
        &license,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0)),
    )?;
    // Cancel any running scan (cooperative; its thread exits without
    // swapping its tree — spec §4 "starting a new scan cancels the old")
    // AND any running duplicates pipeline. One helper, shared by every
    // start path (the hand-copies had drifted).
    cancel_previous(&state);

    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let scan_target = parse_target(&target);
    let cache_key = tree_cache_key(&target);

    // ── The instant restore (session 14) ──────────────────────────
    // The owner's C:→D:→C: report: every flip re-scanned because the
    // swap DISCARDED the displaced tree. When the exact target's tree
    // survives in the flip cache, swap it back instead of walking the
    // disk. `force` (explicit Rescan) purges the stale entry instead —
    // the user asked for fresh data.
    if force.unwrap_or(false) {
        // Forced rescan: the target's cached snapshot is stale BY
        // CHOICE — purge it so the fresh tree replaces it (swap_tree
        // also drops the displaced same-key tree instead of filing it).
        state.tree_cache.take(&cache_key);
    } else if let Some(cached) = state.tree_cache.take(&cache_key) {
        // The restored snapshot must carry THIS generation: every
        // IPC guard compares against `Tree.generation`. Fast path:
        // try_unwrap when the cache held the only reference (the
        // common case — the displaced tree's readers are gone).
        // Contended (a long-lived reader, e.g. a running duplicates
        // pipeline, still latches the snapshot): deep-copy on the
        // blocking pool — the copy runs off the async runtime
        // thread (a million-node memcpy would stall every command;
        // the surgery commit pays the same cost by the same rule).
        let tree = match Arc::try_unwrap(cached) {
            Ok(mut owned) => {
                owned.generation = generation;
                Arc::new(owned)
            }
            Err(shared) => {
                let mut owned =
                    tauri::async_runtime::spawn_blocking(move || Tree::deep_from(&shared))
                        .await
                        .map_err(|e| format!("tree restore failed: {e}"))?;
                owned.generation = generation;
                Arc::new(owned)
            }
        };
        let root_stats = complete_scan(&state, &app, tree, generation, &cache_key);
        // Structured line for the benchmark harness: an instant restore
        // proves the flip cache served the tree (no walk happened).
        eprintln!(
            "[bench] scan restored gen={generation} files={} dirs={} bytes={}",
            root_stats.2, root_stats.3, root_stats.1
        );
        return Ok(ScanStart {
            generation,
            restored: true,
            stats: Some(root_stats),
        });
    }

    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let progress = Arc::clone(&state.progress);
    // Reset the progress snapshot for the new generation.
    *progress.lock() = Progress::default();

    state.mark_scanning(generation);

    let app_handle = app.clone();
    let platform = Arc::clone(&*platform);

    // The scan + swap thread. State is borrowed from the handle INSIDE
    // the closure so everything the thread owns is 'static.
    let cancel_for_thread = Arc::clone(&cancel);
    let key_for_thread = cache_key.clone();
    let engine_label = "standard";
    let join = std::thread::spawn(move || {
        let state_inner = app_handle.state::<AppState>();
        let started = std::time::Instant::now();
        let outcome = diskgenie_core::scan::scanner::scan(
            platform,
            &scan_target,
            generation,
            &cancel_for_thread,
            &progress,
        );
        // Clear ONLY our own generation: when a newer scan superseded
        // us, the flag names ITS generation and must stay set (the old
        // unconditional `store(false)` here killed the successor's
        // ticker and made `get_status` report idle mid-scan).
        state_inner.end_scanning(generation);
        // Engine telemetry (doc 07 §4) — performance watchdog, counts only.
        let an = app_handle.try_state::<crate::analytics::Analytics>();
        let denied = state_inner.progress.lock().denied;
        if let Some(an) = &an {
            if let ScanOutcome::Done(ref tree) = outcome {
                an.capture(
                    "scan_perf",
                    &[
                        ("engine", serde_json::json!(engine_label)),
                        ("files", serde_json::json!(tree.root_stats().2)),
                        ("dirs", serde_json::json!(tree.root_stats().3)),
                        ("bytes", serde_json::json!(tree.root_stats().1)),
                        (
                            "duration_ms",
                            serde_json::json!(started.elapsed().as_millis() as u64),
                        ),
                        (
                            "workers",
                            serde_json::json!(std::thread::available_parallelism()
                                .map_or(0u32, |n| u32::try_from(n.get()).unwrap_or(0))),
                        ),
                        ("denied", serde_json::json!(denied)),
                    ],
                );
            }
        }

        match outcome {
            ScanOutcome::Done(tree) => {
                let root_stats = complete_scan(
                    &state_inner,
                    &app_handle,
                    Arc::new(tree),
                    generation,
                    &key_for_thread,
                );
                // Structured timing line for the capture/benchmark
                // harnesses (plain stderr; same posture as the dupes
                // pipeline's eprintln diagnostics).
                eprintln!(
                    "[bench] scan done engine={engine_label} gen={generation} ms={} files={} dirs={} bytes={}",
                    started.elapsed().as_millis(),
                    root_stats.2, root_stats.3, root_stats.1
                );
            }
            ScanOutcome::Cancelled => {
                // Superseded or user-cancelled: no tree swap, no done
                // event (the NEW scan's events own the UI now).
            }
            ScanOutcome::RootFailed(reason) => {
                record_scan_failure(&state_inner, &app_handle, generation, reason);
            }
        }
    });

    // Register the handle (replacing the cancelled one).
    {
        let mut scan = state.scan.lock();
        *scan = Some(ScanHandle {
            generation,
            cancel: Arc::clone(&cancel),
            join: Some(join),
        });
    }

    // The 150 ms progress ticker (spec §4). Generation-owned: it exits
    // when a NEWER scan's flag replaces ours (its own ticker owns the
    // emissions from there) — the old flagless loop double-emitted or
    // died early depending on which worker's exit path cleared the
    // shared bool first.
    let ticker_app = app.clone();
    std::thread::Builder::new()
        .name("db-ticker".into())
        .spawn(move || {
            let ticker_state = ticker_app.state::<AppState>();
            loop {
                if ticker_state.scanning.load(Ordering::SeqCst) != generation {
                    return;
                }
                let snapshot = ticker_state.progress.lock().clone();
                let gen = ticker_state.current_generation();
                let _ = ticker_app.emit(
                    "scan-progress",
                    ProgressEvent {
                        generation: gen,
                        progress: snapshot,
                    },
                );
                std::thread::sleep(TICK_MS);
            }
        })
        .ok();

    Ok(ScanStart {
        generation,
        restored: false,
        stats: None,
    })
}

/// `scan-progress` payload (generation-tagged so stale ticks drop).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    /// The generation this tick belongs to.
    pub generation: u64,
    /// Progress snapshot.
    pub progress: Progress,
}

/// Live status for the Explore states (idle / scanning / done) plus
/// the sticky last-done record (the lost-event reconcile: a scan can
/// complete before the `start_scan` round-trip lands, so the UI polls
/// this right after resolving).
#[tauri::command]
#[must_use]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn get_status(state: State<'_, AppState>) -> StatusResponse {
    let last_done = state.last_done.lock().clone();
    StatusResponse {
        generation: state.current_generation(),
        scanning: state.is_scanning(),
        has_tree: state.tree.read().is_some(),
        progress: state.progress.lock().clone(),
        last_done: Some(LastDone {
            generation: last_done.generation,
            stats: last_done.stats,
            error: last_done.error,
        }),
    }
}

/// Cancel the running scan (user action — spec §4 "starting a new scan
/// cancels the old"; this exposes the SAME cooperative mechanism to the
/// Stop button). The worker thread exits at its next checkpoint
/// without swapping its tree, so the previous tree (if any) stays
/// visible. Returns whether a scan was actually running.
///
/// The `scanning` flag flips immediately so the 150 ms progress ticker
/// stops at once; the frontend reverts to its saved tree-generation
/// optimistically and reconciles through `get_status` (a scan that
/// completed in the cancel window still emits its `scan-done`).
#[tauri::command]
#[must_use]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn cancel_scan(state: State<'_, AppState>) -> bool {
    let running = {
        let scan = state.scan.lock();
        match scan.as_ref() {
            Some(handle) => {
                handle.cancel.store(true, Ordering::SeqCst);
                // Clear the running flag ONLY when it still names this
                // handle's generation (a newer scan may already own it).
                state.end_scanning(handle.generation);
                true
            }
            None => false,
        }
    };
    running
}

/// Swap the finished tree in, FILING the displaced tree into the flip
/// cache under its own key (session 14: the C:→D:→C: flip re-scanned
/// every time because the swap discarded the displaced tree). The
/// displaced tree — and any cache eviction this filing causes — drops
/// on a background thread (spec §4 — never stall the UI on a
/// million-node dealloc; an Arc with surviving readers is a cheap
/// decrement on that thread either way).
///
/// Filing rules (the staleness contract): the displaced tree is worth
/// caching only when it belongs to a DIFFERENT target than the new
/// tree — a same-target swap is an explicit re-scan, so the displaced
/// snapshot is stale BY CHOICE and drops; an empty old key (unknown
/// lineage) drops un-cached.
/// Swap the finished tree in, FILING the displaced tree into the flip
/// cache (see `swap_tree`).
fn swap_tree(state: &AppState, new: Arc<Tree>, generation: u64, new_key: &str) {
    // Everything under ONE write-lock hold: the generation guard, the
    // swap, the displaced tree's cache filing, and the key handover —
    // a concurrent restore can never observe a half-filed state. (The
    // lock order is tree → target_key → tree_cache; no other path
    // takes these locks in any order, so no inversion exists.)
    let mut to_drop: Option<Arc<Tree>> = None;
    {
        let mut guard = state.tree.write();
        // Only swap when we are still the current generation.
        if generation != state.current_generation() {
            return;
        }
        let displaced = guard.replace(new);
        let old_key = std::mem::replace(&mut *state.target_key.lock(), new_key.to_string());
        if let Some(tree) = displaced {
            if old_key.is_empty() || old_key == new_key {
                // Unknown lineage, or an explicit same-target re-scan:
                // the displaced snapshot is not worth restoring.
                to_drop = Some(tree);
            } else if let Some(evicted) = state.tree_cache.insert(old_key, tree) {
                to_drop = Some(evicted);
            }
        }
    }
    if let Some(tree) = to_drop {
        std::thread::spawn(move || drop(tree));
    }
}

/// Cancel any running scan AND duplicates pipeline. The shared prelude
/// of every scan start (spec §4 "starting a new scan cancels the old");
/// the duplicates kill exists because the tree the pipeline is hashing
/// is about to be replaced (a multi-GB hash burning I/O behind a fresh
/// scan describes a dead world). Extracted from the three start paths —
/// the triplicated copies had already started drifting in comments.
fn cancel_previous(state: &AppState) {
    {
        let scan = state.scan.lock();
        if let Some(old) = scan.as_ref() {
            old.cancel.store(true, Ordering::SeqCst);
        }
        state.dupes_cancel.fetch_add(1, Ordering::SeqCst);
        // Drop the sticky dupes result now — the pipeline's own
        // resolution clears `running` a beat later (cooperative cancel).
        let mut st = state.dupes_status.lock();
        st.result = None;
        st.error = None;
        st.progress = None;
    }
}

/// The completion sequence, ONCE: `swap_tree → clear_all_caches →
/// sticky last_done → scan-done emit`. Every start path (standard scan,
/// turbo success, turbo's standard-fallback, the flip-cache restore)
/// walked this exact ordering — four hand-copies of the invariant that
/// a superseded generation must never swap. Returns the swapped tree's
/// root stats. The sticky record BEFORE the emit is load-bearing: a
/// tiny tree can finish before the `start_scan` round-trip resolves,
/// so the event is dropped as stale by the store and `get_status`
/// replays this record for the reconcile.
fn complete_scan(
    state: &AppState,
    app: &AppHandle,
    tree: Arc<Tree>,
    generation: u64,
    cache_key: &str,
) -> (u64, u64, u64, u64) {
    let root_stats = tree.root_stats();
    swap_tree(state, tree, generation, cache_key);
    clear_all_caches(app);
    *state.last_done.lock() = crate::state::DoneRecord {
        generation,
        stats: Some(root_stats),
        error: None,
    };
    let _ = app.emit(
        "scan-done",
        ScanDone {
            generation,
            stats: Some(root_stats),
            error: None,
        },
    );
    root_stats
}

/// The failure sequence, ONCE: sticky `last_done` + the `scan-done`
/// event carrying the reason (no swap, no cache clear — there is no
/// new tree). Standard `RootFailed`, the turbo fallback's `RootFailed`,
/// and the turbo no-tree arm all walked this exact pair.
fn record_scan_failure(state: &AppState, app: &AppHandle, generation: u64, reason: String) {
    *state.last_done.lock() = crate::state::DoneRecord {
        generation,
        stats: None,
        error: Some(reason.clone()),
    };
    let _ = app.emit(
        "scan-done",
        ScanDone {
            generation,
            stats: None,
            error: Some(reason),
        },
    );
}

/// Dev hooks (spec §15): `DISKGENIE_SCAN` / `--scan` auto-start targets,
/// `DISKGENIE_MODE` visualization, `--turbo`, `DISKGENIE_VERIFY`.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DevHooks {
    /// Auto-scan target from `DISKGENIE_SCAN` / `--scan <path>`.
    pub scan: Option<String>,
    /// Visualization from `DISKGENIE_MODE`.
    pub mode: Option<String>,
    /// `--turbo` requested.
    pub turbo: bool,
    /// `DISKGENIE_VERIFY=1` two-engine comparison.
    pub verify: bool,
    /// `DISKGENIE_TOUR=1` auto-cycles tabs/modes/overlays so CI
    /// screenshot passes can capture every state of the real app
    /// without interactive automation.
    pub tour: bool,
    /// `DISKGENIE_TOUR_MODE` — the tour PROGRAM: unset/`"ui"` = the
    /// full UI sweep; `"license"` = the live-key lifecycle (entry →
    /// bad-format → unknown-key → REAL activation → status card →
    /// validate → unlocked app, the macOS license E2E workflow);
    /// `"bench"` = the benchmark program (scan → cache-restore →
    /// duplicates, the macOS benchmark workflow).
    pub tour_mode: Option<String>,
    /// `DISKGENIE_TOUR_LICENSE_KEY` — a real, freshly-minted key the
    /// license tour activates with (the workflow mints it via the
    /// admin API; never hardcoded, never logged).
    pub tour_license_key: Option<String>,
    /// `DISKGENIE_WINDOW` (`WxH`, e.g. `1280x760`) — the initial window
    /// size for capture workflows (the screenshot tour must fit the
    /// runner's display; the logical size also feeds the responsive
    /// window-min/window-full tour steps).
    pub window: Option<String>,
}

/// Read the §15 dev hooks once (flags/env; empty when not set).
#[must_use]
pub fn read_dev_hooks() -> DevHooks {
    let args: Vec<String> = std::env::args().collect();
    let mut hooks = DevHooks::default();
    if let Ok(v) = std::env::var("DISKGENIE_SCAN") {
        if !v.is_empty() {
            hooks.scan = Some(v);
        }
    }
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--scan" {
            if let Some(path) = it.next() {
                hooks.scan = Some(path.clone());
            }
        } else if a == "--turbo" {
            hooks.turbo = true;
        }
    }
    if let Ok(v) = std::env::var("DISKGENIE_MODE") {
        if !v.is_empty() {
            hooks.mode = Some(v);
        }
    }
    if let Ok(v) = std::env::var("DISKGENIE_VERIFY") {
        hooks.verify = v == "1";
    }
    if let Ok(v) = std::env::var("DISKGENIE_TOUR") {
        hooks.tour = v == "1";
    }
    if let Ok(v) = std::env::var("DISKGENIE_TOUR_MODE") {
        if !v.is_empty() {
            hooks.tour_mode = Some(v);
        }
    }
    if let Ok(v) = std::env::var("DISKGENIE_TOUR_LICENSE_KEY") {
        if !v.is_empty() {
            hooks.tour_license_key = Some(v);
        }
    }
    if let Ok(v) = std::env::var("DISKGENIE_WINDOW") {
        if !v.is_empty() {
            hooks.window = Some(v);
        }
    }
    hooks
}

/// `get_dev_hooks` command (spec §15).
#[tauri::command]
#[must_use]
pub fn get_dev_hooks() -> DevHooks {
    read_dev_hooks()
}

/// The turbo scan outcome summary (scan-done + turbo report).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurboReport {
    /// The generation that finished.
    pub generation: u64,
    /// Records parsed.
    pub records: u64,
    /// Warnings (torn/bad/orphans/unreferenced/cycles).
    pub torn: u64,
    /// Malformed records skipped.
    pub bad: u64,
    /// MFT records unreachable from any parent (orphaned).
    pub unreferenced: u64,
    /// Turbo wall time in ms.
    pub ms: u64,
}

/// Start a TURBO scan (spec §5; doc 03 M7) of `target` (a drive root).
/// Requires elevation; the R2 fallback contract: any failure returns a
/// user-readable reason and the caller falls back to the STANDARD
/// engine, user-visible, never silent.
///
/// # Errors
/// - `"ELEVATION_REQUIRED"` when not elevated (JS shows the shield).
/// - A reason string for every other failure (fallback trigger).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
#[allow(clippy::too_many_lines)]
// scan lifecycle: elevation gate + thread spawn
// The async is the tauri command contract (runs on the async runtime
// thread, not the main thread); the body never awaits — the MFT read
// and the fallback live on the spawned scan thread.
#[allow(clippy::unused_async)]
pub async fn start_scan_turbo(
    target: String,
    app: AppHandle,
    state: State<'_, AppState>,
    platform: State<'_, std::sync::Arc<HostPlatform>>,
    license: State<'_, crate::commands::license::LicenseManager>,
) -> Result<u64, String> {
    crate::commands::license::require_licensed(
        &license,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0)),
    )?;
    if !crate::platform::os::is_elevated() {
        return Err("ELEVATION_REQUIRED".into());
    }
    let drive_root = if cfg!(target_os = "macos") {
        format!("{}/", target.trim_end_matches(['\\', '/']))
    } else {
        format!("{}\\", target.trim_end_matches('\\'))
    };
    let label = drive_root.clone();
    // The flip-cache key for this drive (turbo trees file/displace by
    // the same key the standard engine uses, so a turbo C: scan
    // displaces a standard C: snapshot — same target, same key).
    let cache_key = tree_cache_key(&drive_root);
    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;

    // Cancel any standard scan in flight (the shared prelude, same
    // contract as start_scan, including the running-duplicates kill).
    cancel_previous(&state);
    // Turbo scans register a ScanHandle too: without one, cancel_scan
    // flipped a flag on a STALE handle while the MFT read ran
    // uncancelled — the Stop button was a no-op during turbo scans.
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    state.mark_scanning(generation);
    *state.progress.lock() = Progress::default();

    let app_handle = app.clone();
    let platform = std::sync::Arc::clone(&*platform);
    let cancel_for_thread = Arc::clone(&cancel);
    let key_for_thread = cache_key;
    let join = std::thread::Builder::new()
        .name("db-scan-turbo".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let state_inner = app_handle.state::<AppState>();
            let mut reason: Option<String> = None;

            let mut tree_opt: Option<diskgenie_core::scan::node::Tree> = None;
            let mut report: Option<TurboReport> = None;

            if !crate::platform::os::enable_backup_privilege() {
                reason = Some("The backup privilege is unavailable on this process.".into());
            }
            if reason.is_none() {
                // The MFT read itself honours the cancel flag (a user Stop
                // during the raw read should not keep the disk busy).
                if cancel_for_thread.load(Ordering::SeqCst) {
                    reason = Some("cancelled".into());
                }
            }
            if reason.is_none() {
                match crate::platform::os::turbo_geometry(&drive_root) {
                    Ok((mut volume, geo)) => {
                        match crate::platform::os::turbo_read_mft(&mut volume, &geo) {
                            Ok(mft) => {
                                let core_geo = diskgenie_core::turbo::Geometry {
                                    bytes_per_sector: geo.bytes_per_sector,
                                    bytes_per_cluster: geo.bytes_per_cluster,
                                    bytes_per_record: geo.bytes_per_record,
                                    mft_valid_data_length: geo.mft_valid_data_length,
                                };
                                let (entries, warnings) =
                                    diskgenie_core::turbo::parse_all(&mft, &core_geo);
                                let records = entries.len() as u64;
                                let mut build =
                                    diskgenie_core::turbo::tree::build_tree(entries, &label);
                                diskgenie_core::scan::rollup::finalize(&mut build.tree);
                                build.tree.generation = generation;
                                tree_opt = Some(build.tree);
                                report = Some(TurboReport {
                                    generation,
                                    records,
                                    torn: warnings.torn,
                                    bad: warnings.bad,
                                    unreferenced: warnings.unreferenced,
                                    ms: started.elapsed().as_millis() as u64,
                                });
                            }
                            Err(e) => reason = Some(e),
                        }
                    }
                    Err(e) => reason = Some(e),
                }
            }

            match (tree_opt, reason) {
                (Some(tree), None) => {
                    state_inner.end_scanning(generation);
                    complete_scan(
                        &state_inner,
                        &app_handle,
                        std::sync::Arc::new(tree),
                        generation,
                        &key_for_thread,
                    );
                    if let Some(r) = report {
                        let _ = app_handle.emit("turbo-report", r);
                    }
                }
                (_, Some(error)) => {
                    // R2: the ONE allowed fallback — user-visible with reason.
                    let _ = app_handle.emit("turbo-fallback", &error);
                    // Fall back to the standard engine on this thread,
                    // stating the reason in the event above. The scanning
                    // flag STAYS on our generation through the whole
                    // fallback (the old code cleared it first — the entire
                    // fallback ran with `scanning == false`: dead ticker,
                    // `get_status` lying) and the REGISTERED cancel flag
                    // flows in (the old code minted a fresh never-cancelled
                    // flag, so Stop could not cancel a fallback either).
                    let outcome = diskgenie_core::scan::scanner::scan(
                        platform,
                        &parse_target(&drive_root),
                        generation,
                        &cancel_for_thread,
                        &state_inner.progress,
                    );
                    state_inner.end_scanning(generation);
                    match outcome {
                        ScanOutcome::Done(tree) => {
                            complete_scan(
                                &state_inner,
                                &app_handle,
                                std::sync::Arc::new(tree),
                                generation,
                                &key_for_thread,
                            );
                        }
                        ScanOutcome::RootFailed(r) => {
                            record_scan_failure(&state_inner, &app_handle, generation, r);
                        }
                        ScanOutcome::Cancelled => {}
                    }
                }
                (None, None) => {
                    state_inner.end_scanning(generation);
                    record_scan_failure(
                        &state_inner,
                        &app_handle,
                        generation,
                        "The turbo engine produced no tree.".to_string(),
                    );
                }
            }
        })
        .ok();
    // Register the turbo handle so cancel_scan reaches the MFT read and
    // the fallback (see the ScanHandle note above).
    {
        let mut scan = state.scan.lock();
        *scan = Some(ScanHandle {
            generation,
            cancel,
            join,
        });
    }
    Ok(generation)
}

/// Clear every generation-keyed cache (shared by the scan swap paths
/// AND the cleanup commit — the commit path used to hand-duplicate
/// this list, and the two copies were already drifting).
pub fn clear_all_caches(app: &AppHandle) {
    crate::commands::layout::clear_cache(&app.state::<crate::commands::layout::Cache>());
    crate::commands::layout::clear_regroup_cache(
        &app.state::<crate::commands::layout::RegroupCache>(),
    );
    crate::commands::explore::TopCache::clear(&app.state::<crate::commands::explore::TopCache>());
    crate::commands::explore::AgeCache::clear(&app.state::<crate::commands::explore::AgeCache>());
    crate::commands::sidebar::QuickWinsCache::clear_pub(
        &app.state::<crate::commands::sidebar::QuickWinsCache>(),
    );
}
