//! Duplicates commands (spec §10): the v3 engine flow — collect from
//! the scan tree, then `diskgenie-core`'s screens + lockstep verify
//! (see `core/src/dupes/engine.rs` for the throughput story). Hardlink
//! exclusion via (volume-serial, file-index); cloud placeholders
//! never open (R7.3); wasted-space ranking per the spec.
//!
//! Liveness contract (the "Scanning… forever" fix): a real disk can
//! hold hundreds of GB in same-size buckets, so the command reports
//! honest progress on `dupes-progress` (phase, files, bytes, elapsed
//! — a 200 ms ticker thread samples atomics the workers bump) and
//! accepts cancellation (`cancel_duplicates` bumps a generation
//! counter; the run latches it at start and every per-file check
//! compares against the latch, so a late cancel can never poison a
//! newer run).
//!
//! State contract (the "page switch killed my scan" fix): the run's
//! live status and sticky result live in `AppState.dupes_status`
//! (`dupes_status` command) so the DuplicatesView can re-attach after
//! any tab switch; a UI-unmount can no longer orphan a running
//! multi-GB verify.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diskgenie_core::dupes::engine::{self, EngineCandidate, ProgressSink};
use diskgenie_core::dupes::{self, DupeGroup, HashedFile};
use diskgenie_core::scan::node::Tree;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;

/// Progress ticker cadence (ms).
const TICK_MS: u64 = 200;
/// Progress `elapsed_ms` cap (10 minutes) — `as_millis` is u128; real
/// scans stay far below this and the UI re-computes from its own clock.
const ELAPSED_CAP_MS: u128 = 600_000_000;

/// Phase ids for the atomic phase slot.
const PHASE_COLLECT: u8 = 0;
const PHASE_SCREEN: u8 = 1;
const PHASE_VERIFY: u8 = 2;
const PHASE_DONE: u8 = 3;
const PHASE_CANCELLED: u8 = 4;

/// Global-progress weights: [start, span] per phase on a 0..1 axis.
/// The screen phase reads a bounded window per file (64 KiB + 2 MiB)
/// while verify streams the remaining bytes of every survivor — the
/// span split reflects that asymmetry on real disks (the verify pass
/// owns the bulk of a duplicate-heavy scan; the screen pass owns a
/// candidate-heavy one). Sums to exactly 1.0 so `done` lands on 100%.
const PHASE_WEIGHTS: [(f32, f32); 5] = [
    (0.0, 0.02),  // collect
    (0.02, 0.33), // screen
    (0.35, 0.65), // verify
    (1.0, 0.0),   // done
    (0.0, 0.0),   // cancelled (bar resets with the view)
];

/// Shared run control: atomics the engine workers bump (cheap — no
/// mutex on the hot path), the cancel latch, and the optional event
/// sink. `app: None` in tests (no Tauri runtime needed).
struct DupesCtl {
    app: Option<AppHandle>,
    files_done: AtomicU64,
    files_total: AtomicU64,
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
    /// Cumulative across ALL phases (never reset — the rate + overall
    /// sources; see [`DupesProgress`]).
    files_all: AtomicU64,
    bytes_all: AtomicU64,
    phase: AtomicU8,
    /// Cancel generation shared with `AppState` — `cancel_duplicates`
    /// bumps it; this run latched the value it saw at start.
    cancel_gen: Arc<AtomicU64>,
    /// The latched generation: cancelled iff the shared counter moved.
    latch: u64,
    started: Instant,
}

impl DupesCtl {
    /// A quiet control (no events) — the pure snapshot tests (no
    /// Tauri runtime needed).
    #[cfg(test)]
    fn quiet(gen: Arc<AtomicU64>) -> Self {
        Self::with_app(None, gen)
    }

    /// A live control emitting `dupes-progress` on `app`.
    fn live(app: AppHandle, gen: Arc<AtomicU64>) -> Self {
        Self::with_app(Some(app), gen)
    }

    fn with_app(app: Option<AppHandle>, gen: Arc<AtomicU64>) -> Self {
        let latch = gen.load(Ordering::SeqCst);
        Self {
            app,
            files_done: AtomicU64::new(0),
            files_total: AtomicU64::new(0),
            bytes_done: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            files_all: AtomicU64::new(0),
            bytes_all: AtomicU64::new(0),
            phase: AtomicU8::new(PHASE_COLLECT),
            cancel_gen: gen,
            latch,
            started: Instant::now(),
        }
    }

    /// True when `cancel_duplicates` fired after this run latched.
    fn cancelled(&self) -> bool {
        self.cancel_gen.load(Ordering::Relaxed) != self.latch
    }

    /// Enter a phase: per-phase counters RESET (they describe the
    /// upcoming phase), cumulative counters never do. Totals are
    /// written BEFORE the phase id (the ticker reads phase first, so
    /// a boundary-straddling tick can only show the NEW phase with
    /// fresh-zero counters for one 200 ms beat — never the old phase
    /// with the new totals).
    fn set_phase(&self, phase: u8, files_total: u64, bytes_total: u64) {
        self.files_total.store(files_total, Ordering::Relaxed);
        self.bytes_total.store(bytes_total, Ordering::Relaxed);
        self.files_done.store(0, Ordering::Relaxed);
        self.bytes_done.store(0, Ordering::Relaxed);
        self.phase.store(phase, Ordering::Relaxed);
    }

    /// One finished engine unit: bump files, and the bytes it cost —
    /// both the per-phase and the cumulative counters.
    fn file_done(&self, bytes: u64) {
        self.files_done.fetch_add(1, Ordering::Relaxed);
        self.bytes_done.fetch_add(bytes, Ordering::Relaxed);
        self.files_all.fetch_add(1, Ordering::Relaxed);
        self.bytes_all.fetch_add(bytes, Ordering::Relaxed);
    }

    fn snapshot(&self) -> DupesProgress {
        let phase = self.phase.load(Ordering::Relaxed);
        let phase_str = match phase {
            PHASE_SCREEN => "screen",
            PHASE_VERIFY => "verify",
            PHASE_DONE => "done",
            PHASE_CANCELLED => "cancelled",
            _ => "collect",
        };
        let files_done = self.files_done.load(Ordering::Relaxed);
        let files_total = self.files_total.load(Ordering::Relaxed);
        let bytes_done = self.bytes_done.load(Ordering::Relaxed);
        let bytes_total = self.bytes_total.load(Ordering::Relaxed);
        let (start, span) = PHASE_WEIGHTS[phase.min(4) as usize];
        let frac = if phase == PHASE_DONE {
            1.0
        } else if phase == PHASE_VERIFY {
            // The verify phase's early exits read less than the
            // estimate — the FILE counter is the honest progress
            // source (every member retires or groups exactly once).
            if files_total > 0 {
                (files_done as f64 / files_total as f64).min(1.0) as f32
            } else {
                0.0
            }
        } else if bytes_total > 0 {
            (bytes_done as f64 / bytes_total as f64).min(1.0) as f32
        } else if files_total > 0 {
            (files_done as f64 / files_total as f64).min(1.0) as f32
        } else {
            0.0
        };
        DupesProgress {
            phase: phase_str.to_string(),
            files_done,
            files_total,
            bytes_done,
            bytes_total,
            elapsed_ms: self.started.elapsed().as_millis().min(ELAPSED_CAP_MS) as u64,
            files_done_all: self.files_all.load(Ordering::Relaxed),
            bytes_done_all: self.bytes_all.load(Ordering::Relaxed),
            overall: (start + span * frac).clamp(0.0, 1.0),
        }
    }

    /// Emit one progress event (best-effort; the UI ignores events
    /// outside a busy window).
    fn tick(&self) {
        if let Some(app) = &self.app {
            let _ = app.emit("dupes-progress", self.snapshot());
        }
    }
}

/// The engine's progress seam, backed by the command's control block.
impl ProgressSink for DupesCtl {
    fn phase(&self, phase: &str, files_total: u64, bytes_total: u64) {
        let id = match phase {
            engine::PHASE_SCREEN => PHASE_SCREEN,
            engine::PHASE_VERIFY => PHASE_VERIFY,
            _ => return,
        };
        self.set_phase(id, files_total, bytes_total);
    }
    fn file_done(&self, bytes: u64) {
        DupesCtl::file_done(self, bytes);
    }
    fn cancelled(&self) -> bool {
        DupesCtl::cancelled(self)
    }
}

/// Hardlink identity via the platform seam (spec §10: hardlinks are
/// NOT duplicates). None = unavailable (treated unique).
#[cfg(any(windows, target_os = "macos"))]
fn hardlink_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    crate::platform::os::hardlink_identity(path)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn hardlink_identity(_path: &std::path::Path) -> Option<(u64, u64)> {
    None
}

/// Resolve the scan scope (session 15 "Duplicates here"): the walk
/// start node + the folder path the status/result report. Unknown
/// nodes and FILES reject (the UI's disabled-file contract has a
/// server-side twin); a scope at the tree ROOT is the whole-tree run
/// (normalized to `None` so the status record, the result and the tab
/// all frame it identically).
///
/// # Errors
/// String error for an unknown node or a file node.
fn resolve_scope(tree: &Tree, node: Option<u32>) -> Result<(u32, Option<String>), String> {
    match node {
        None => Ok((tree.root, None)),
        Some(id) if id == tree.root => Ok((tree.root, None)),
        Some(id) => {
            let n = tree.node(id).ok_or_else(|| format!("unknown node {id}"))?;
            if !n.is_dir() {
                return Err("Duplicates scans a folder. Select one to scope it.".into());
            }
            Ok((id, Some(tree.node_path(id))))
        }
    }
}

/// One member of a duplicate-group row (rich facts for the keep
/// rules + reveal).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupeFileView {
    /// Display path.
    pub path: String,
    /// Tree node id (reveal-in-explore wiring).
    pub node_id: u32,
    /// Last-write time (unix seconds; 0 = unknown) — the UI's
    /// keep-newest/keep-oldest smart rules.
    pub modified: i64,
}

/// One duplicate-group row for the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupeGroupView {
    /// Group id (index).
    pub id: usize,
    /// The group's members.
    pub files: Vec<DupeFileView>,
    /// Per-file size.
    pub size: u64,
    /// Member count.
    pub count: u64,
    /// Wasted space = size × (count − 1).
    pub wasted: u64,
}

/// The duplicates response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupesResult {
    pub generation: u64,
    pub groups: Vec<DupeGroupView>,
    /// Total wasted bytes.
    pub wasted_total: u64,
    /// Files considered.
    pub files: u64,
    /// The folder the scan was scoped to (session 15 "Duplicates
    /// here": `None` = the whole tree; a path = the subtree walked).
    /// Byte-identical groups found INSIDE the scope — copies that live
    /// outside it are not candidates, so "wasted" is reclaimable in
    /// context, not a claim about the rest of the disk.
    pub scope_path: Option<String>,
}

/// Live progress snapshot for the `dupes-progress` event (camelCase
/// DTO — the UI's busy row renders phase, files, bytes, elapsed).
///
/// The `*_all` counters and `overall` are the session-5 blink fix:
/// per-phase counters reset at every phase boundary (the old bar
/// snapped 100%→0% four times per scan and the MB/s counter froze);
/// `files_done_all` / `bytes_done_all` accumulate across the WHOLE
/// run (monotonic — rate + ETA stay honest through transitions), and
/// `overall` is a weighted global fraction that never moves backwards
/// (the bar animates one smooth ramp).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupesProgress {
    /// "collect" | "screen" | "verify" | "done" | "cancelled".
    pub phase: String,
    /// Files finished so far in the current phase.
    pub files_done: u64,
    /// Files the current phase will finish.
    pub files_total: u64,
    /// Bytes read so far in the current phase.
    pub bytes_done: u64,
    /// Bytes the current phase will read (estimate — verify early
    /// exits read less).
    pub bytes_total: u64,
    /// Milliseconds since the scan started (capped, see
    /// [`ELAPSED_CAP_MS`]).
    pub elapsed_ms: u64,
    /// Files finished across ALL phases so far (never resets).
    pub files_done_all: u64,
    /// Bytes read across ALL phases so far (never resets — the rate
    /// source).
    pub bytes_done_all: u64,
    /// Weighted global fraction [0, 1] — the bar source (monotonic).
    pub overall: f32,
}

/// Find duplicates in the current tree with live progress,
/// cooperative cancellation, and an APP-LIFETIME state record (page
/// switches can no longer orphan the run: the UI re-attaches via
/// [`dupes_status`]).
///
/// `node` scopes the scan to a folder's subtree (session 15
/// "Duplicates here" — the inspector's launchpad): the collect walk
/// starts there, so only byte-identical groups fully INSIDE the scope
/// are reported. `None` (the tab's own button) walks the whole tree.
///
/// # Errors
/// String error when no scan exists, the generation is stale, a run
/// is already in flight ("already running" — the UI no-ops), the node
/// is unknown or not a folder, the pipeline was cancelled, or the
/// blocking thread failed.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
#[allow(clippy::print_stderr)] // liveness tracing (doc 07 perf-watchdog pattern)
pub async fn find_duplicates(
    generation: u64,
    node: Option<u32>,
    app: AppHandle,
    state: State<'_, AppState>,
    license: State<'_, crate::commands::license::LicenseManager>,
) -> Result<DupesResult, String> {
    // The hard license gate (docs §2 L6).
    crate::commands::license::require_licensed(
        &license,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0)),
    )?;
    let tree = {
        let guard = state.tree.read();
        let Some(tree) = guard.as_ref() else {
            return Err("no scan yet".into());
        };
        if tree.generation != generation {
            return Err(format!(
                "stale generation {} (current {})",
                generation, tree.generation
            ));
        }
        Arc::clone(tree)
    };
    // Resolve the scope BEFORE marking running: a bad node must
    // reject without touching the run state.
    let (start, scope_path) = resolve_scope(&tree, node)?;
    // The run-state gate: mark running BEFORE spawning so a same-tick
    // second click (or a stale view's invoke) is rejected cleanly
    // instead of stacking a second pipeline. The sticky result dies
    // with the new run — a fresh scan means a fresh page.
    {
        let mut st = state.dupes_status.lock();
        if st.running {
            return Err("already running".into());
        }
        st.running = true;
        st.generation = generation;
        st.progress = None;
        st.result = None;
        st.error = None;
        st.scope_path.clone_from(&scope_path);
    }
    eprintln!(
        "[dupes] start gen={generation} scope={:?} tree_nodes={}",
        scope_path,
        tree.len()
    );
    let t_start = Instant::now();
    let ctl = Arc::new(DupesCtl::live(app, Arc::clone(&state.dupes_cancel)));
    let status_out = Arc::clone(&state.dupes_status);
    // The ticker also mirrors every snapshot into the app-lifetime
    // status record — `dupes_status` queries never observe a stale
    // phase, and a mid-scan page switch re-attaches to LIVE counters.
    let ticker_ctl = Arc::clone(&ctl);
    let ticker_status = Arc::clone(&state.dupes_status);
    let finished = Arc::new(AtomicBool::new(false));
    let finished_t = Arc::clone(&finished);
    std::thread::spawn(move || {
        while !finished_t.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(TICK_MS));
            if finished_t.load(Ordering::Relaxed) {
                break;
            }
            let snap = ticker_ctl.snapshot();
            ticker_ctl.tick();
            let mut st = ticker_status.lock();
            if st.running {
                st.progress = Some(snap);
            }
        }
    });
    let compute_ctl = Arc::clone(&ctl);
    let started = ctl.started; // Instant is Copy

    // The join is a DOUBLE Result: outer = JoinError (thread failure),
    // inner = the pipeline's own Ok/Err. The session-4 code flattened
    // with an early `?` — which also early-returned on a join failure,
    // skipping `finished.store` (the ticker thread leaked) and leaving
    // `dupes_status.running` stuck true. Matching BOTH layers in one
    // place (below) settles the record for every outcome.
    let joined = tauri::async_runtime::spawn_blocking(move || {
        eprintln!("[dupes] spawn_blocking task ENTERED");
        let out = compute_dupes(&tree, compute_ctl.as_ref(), start);
        eprintln!("[dupes] compute finished at {:?}", started.elapsed());
        out
    })
    .await
    .map_err(|e| format!("dupes thread failed: {e}"));
    eprintln!("[dupes] await resolved at {:?}", t_start.elapsed());
    finished.store(true, Ordering::Relaxed);
    // Terminal resolution: settle the app-lifetime record + the event
    // stream in ONE place (cancel is a quiet reset — no error banner;
    // a failure — pipeline OR join — records the message for the
    // re-attached view). The or-pattern binds `msg: &String` on both
    // the pipeline error and the join error.
    let terminal = match &joined {
        Ok(Ok(res)) => {
            ctl.set_phase(PHASE_DONE, 0, 0);
            let mut st = status_out.lock();
            st.running = false;
            st.progress = Some(ctl.snapshot());
            st.result = Some(res.clone());
            st.error = None;
            Ok(res.clone())
        }
        Ok(Err(msg)) | Err(msg) => {
            let cancelled = msg.contains("cancelled");
            if cancelled {
                ctl.phase.store(PHASE_CANCELLED, Ordering::Relaxed);
            }
            let mut st = status_out.lock();
            st.running = false;
            st.progress = Some(ctl.snapshot());
            if !cancelled {
                st.error = Some(msg.clone());
            }
            Err(msg.clone())
        }
    };
    // Terminal event: the busy row settles on "done" (or the invoke's
    // Err lands first — either way the window closes).
    ctl.tick();
    terminal
}

/// Read the app-lifetime duplicates status (the page-switch fix's
/// query half): a freshly-mounted DuplicatesView adopts the running
/// pipeline's live progress or the sticky last result instead of
/// showing "Start scan" over a scan that is still verifying.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn dupes_status(state: State<'_, AppState>) -> DupesStatusView {
    let st = state.dupes_status.lock();
    DupesStatusView {
        running: st.running,
        generation: st.generation,
        progress: st.progress.clone(),
        result: st.result.clone(),
        error: st.error.clone(),
        scope_path: st.scope_path.clone(),
    }
}

/// Serializable mirror of [`crate::state::DupesStatus`] (camelCase).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupesStatusView {
    pub running: bool,
    pub generation: u64,
    pub progress: Option<DupesProgress>,
    pub result: Option<DupesResult>,
    pub error: Option<String>,
    /// The scoped folder (None = whole tree) — a remounted view shows
    /// the scope it re-attached to.
    pub scope_path: Option<String>,
}

/// Cancel the running duplicates scan. Idempotent; safe when nothing
/// is running (the bump only affects runs that latched an older
/// value). Returns the bumped generation.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn cancel_duplicates(state: State<'_, AppState>) -> u64 {
    state.dupes_cancel.fetch_add(1, Ordering::SeqCst) + 1
}

/// The full pipeline: collect the scan tree's candidates → the core
/// v3 engine (screens + lockstep verify) → hardlink exclusion +
/// wasted-space ranking.
///
/// The engine owns all file I/O and both worker pools (screen: the
/// latency pool; verify: the bandwidth pool — see the core module for
/// the sizing laws). This function owns the tree walk, the platform
/// hardlink seam, and the DTO assembly.
///
/// # Errors
/// `Err("cancelled")` when the user cancelled mid-pipeline.
#[allow(clippy::print_stderr)] // liveness tracing
fn compute_dupes(tree: &Tree, ctl: &DupesCtl, start: u32) -> Result<DupesResult, String> {
    // Collect: live files only. Cloud placeholders NEVER open (R7.3)
    // and Windows-managed (protected) files never hash or stage (§4 —
    // pagefile.sys is not a "duplicate" anyone should reclaim).
    // The walk starts at `start` (the tree root for whole-tree scans,
    // the scoped folder for "Duplicates here") — the scope decides
    // which files are candidates; nothing downstream knows the
    // difference.
    ctl.set_phase(PHASE_COLLECT, 0, 0);
    let scope_path: Option<String> = (start != tree.root).then(|| tree.node_path(start));
    let mut candidates: Vec<EngineCandidate> = Vec::new();
    tree.walk(start, |id, n| {
        if !n.is_dir()
            && !n.is_removed()
            && !n.is_cloud_placeholder()
            && !n.is_protected()
            && n.logical > 0
        {
            candidates.push(EngineCandidate {
                path: tree.node_path(id),
                size: n.logical,
                node_id: id,
            });
        }
    });
    // Disk-locality order: the tree walk order is
    // traversal-dependent, not on-disk order — the engine sorts by
    // path internally (same contract as v2).
    let total_files = candidates.len() as u64;
    eprintln!(
        "[dupes] collected {total_files} candidates at {:?}",
        ctl.started.elapsed()
    );

    // The v3 engine: screens + lockstep verification (core-side).
    let verified = engine::run(&candidates, &engine::EngineConfig::default(), ctl)
        .map_err(|e| e.to_string())?;
    eprintln!(
        "[dupes] engine finished: {} verified groups at {:?}",
        verified.len(),
        ctl.started.elapsed()
    );

    // Hardlink identity per verified member + the tree facts the UI's
    // keep rules and reveal wiring need (node id + modified). The
    // platform seam needs an open handle; verified members are the
    // true-duplicate subset, so this opens a small fraction of the
    // tree.
    let mut hashed: Vec<HashedFile> = Vec::new();
    for (class, group) in verified.iter().enumerate() {
        for f in &group.files {
            let (vs, fi) = hardlink_identity(std::path::Path::new(&f.path))
                .unwrap_or((u64::MAX, u64::from(f.node_id)));
            let modified = tree.node(f.node_id).map_or(0, |n| n.modified);
            hashed.push(HashedFile {
                path: f.path.clone(),
                size: f.size,
                volume_serial: vs,
                file_index: fi,
                class: class as u64,
                node_id: f.node_id,
                modified,
            });
        }
    }

    // Core ranking (hardlink exclusion + wasted-space sort).
    let groups: Vec<DupeGroup> = dupes::rank(&hashed);
    let (wasted_total, _) = dupes::totals(&groups);
    // The list cap: the UI filters/sorts client-side, so it gets a
    // generous window of the biggest groups (the header's totals
    // still reflect EVERY group — `wasted_total` is computed before
    // the cap).
    let views: Vec<DupeGroupView> = groups
        .into_iter()
        .take(500)
        .enumerate()
        .map(|(id, g)| {
            let count = g.files.len() as u64;
            DupeGroupView {
                id,
                files: g
                    .files
                    .into_iter()
                    .map(|f| DupeFileView {
                        path: f.path,
                        node_id: f.node_id,
                        modified: f.modified,
                    })
                    .collect(),
                size: g.size,
                count,
                wasted: g.wasted,
            }
        })
        .collect();
    ctl.set_phase(PHASE_DONE, 0, 0);
    Ok(DupesResult {
        generation: tree.generation,
        groups: views,
        wasted_total,
        files: total_files,
        scope_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_dto_carries_scope_path() {
        // The session-15 shape: the tab's scope framing reads
        // scopePath — serde must camelCase it (and the status view's
        // twin). The expected fragment is BUILT by serde itself so the
        // backslash escaping of real Windows paths can't drift.
        let r = DupesResult {
            generation: 7,
            groups: vec![],
            wasted_total: 0,
            files: 0,
            scope_path: Some("C:\\Users\\dev".into()),
        };
        let json = serde_json::to_string(&r).unwrap();
        let scope_json = serde_json::to_string(&r.scope_path).unwrap();
        assert!(json.contains(&format!("\"scopePath\":{scope_json}")));
        assert!(serde_json::to_string(&DupesStatusView {
            running: false,
            generation: 7,
            progress: None,
            result: None,
            error: None,
            scope_path: None,
        })
        .unwrap()
        .contains("\"scopePath\":null"));
    }

    #[test]
    fn phase_dto_is_camelcased() {
        // The UI reads phase/filesDone/bytesDone + the session-5
        // cumulative fields — serde must camelCase all of them.
        let p = DupesProgress {
            phase: "verify".into(),
            files_done: 1,
            files_total: 2,
            bytes_done: 3,
            bytes_total: 4,
            elapsed_ms: 5,
            files_done_all: 6,
            bytes_done_all: 7,
            overall: 0.5,
        };
        let s = serde_json::to_string(&p).expect("serialize");
        assert!(s.contains("\"filesDone\""), "camelCase DTO: {s}");
        assert!(s.contains("\"bytesDone\""), "camelCase DTO: {s}");
        assert!(s.contains("\"elapsedMs\""), "camelCase DTO: {s}");
        assert!(s.contains("\"filesDoneAll\""), "camelCase DTO: {s}");
        assert!(s.contains("\"bytesDoneAll\""), "camelCase DTO: {s}");
        assert!(s.contains("\"overall\""), "camelCase DTO: {s}");
        assert!(s.contains("\"phase\":\"verify\""), "camelCase DTO: {s}");
    }

    #[test]
    fn phase_weights_partition_the_axis() {
        // The bar must land on exactly 100% at `done` and never exceed
        // 1. The first FOUR entries form the sequential ramp; the
        // cancelled entry (index 4) is a reset marker OUTSIDE the ramp
        // — a cancel clears the bar with the view, it does not
        // continue the ramp (contiguity through it is meaningless and
        // the ramp must still sum to exactly 1.0 on its own).
        let mut acc = 0.0f32;
        for (start, span) in PHASE_WEIGHTS.iter().take(4) {
            assert!((0.0..=1.0).contains(start), "weight start in range");
            assert!(*span >= 0.0, "weight span non-negative");
            assert!((start - acc).abs() < 1e-6, "weights are contiguous");
            acc = start + span;
        }
        assert!((acc - 1.0).abs() < 1e-6, "ramp sums to 1.0 (got {acc})");
        assert_eq!(
            PHASE_WEIGHTS[usize::from(PHASE_CANCELLED)],
            (0.0, 0.0),
            "cancelled resets, it does not ramp"
        );
    }

    #[test]
    fn overall_is_monotonic_across_a_full_run() {
        // The blink fix's core promise: no phase boundary can move the
        // global bar backwards (the old per-phase bar snapped to 0%
        // four times per scan and read as blinking/lagging).
        let ctl = DupesCtl::quiet(Arc::new(AtomicU64::new(0)));
        let mut last = 0.0f32;
        // (no `mut`: the closure captures `ctl` by shared reference —
        // `unused_mut` is a hard error under CI's `-D warnings`.)
        let check = |last: &mut f32| {
            let s = ctl.snapshot();
            assert!(
                s.overall >= *last - 1e-6,
                "overall regressed: {} -> {} ({})",
                *last,
                s.overall,
                s.phase
            );
            *last = s.overall;
        };
        check(&mut last); // collect (empty)
        ctl.set_phase(PHASE_SCREEN, 100, 100 * 1024);
        for i in 1..=100u64 {
            ctl.file_done(1024);
            if i % 25 == 0 {
                check(&mut last);
            }
        }
        ctl.set_phase(PHASE_VERIFY, 4, 4 * 1024 * 1024 * 1024);
        check(&mut last); // boundary
        for i in 1..=4u64 {
            ctl.file_done(1024 * 1024 * 1024);
            if i % 2 == 0 {
                check(&mut last);
            }
        }
        ctl.set_phase(PHASE_DONE, 0, 0);
        check(&mut last);
        assert!((ctl.snapshot().overall - 1.0).abs() < 1e-6, "done = 100%");
        // Cumulative counters survived every boundary (rate/ETA
        // source).
        let s = ctl.snapshot();
        assert_eq!(s.files_done_all, 104, "cumulative files");
        assert!(s.bytes_done_all > 0, "cumulative bytes");
    }

    #[cfg(windows)]
    mod windows_e2e {
        use super::*;
        use diskgenie_core::scan::scanner::{scan, Progress, ScanOutcome, ScanTarget};
        use parking_lot::Mutex;
        use std::fs;
        use std::os::windows::fs::OpenOptionsExt;
        use std::path::PathBuf;

        /// Unique scratch dir under %TEMP% (no tempfile dep).
        fn scratch(name: &str) -> PathBuf {
            let d =
                std::env::temp_dir().join(format!("db-dupes-e2e-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&d); // R7.1-allow: test-scratch
            fs::create_dir_all(&d).expect("scratch dir");
            d
        }

        /// Keeps a scratch dir alive for the test body.
        struct TempTree(PathBuf);
        impl Drop for TempTree {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0); // R7.1-allow: test-scratch
            }
        }

        /// Deterministic pseudo-random content (xorshift64) — fast;
        /// distinct seeds never collide under the screens + chains.
        fn blob(seed: u64, size: usize) -> Vec<u8> {
            let mut s = seed | 1;
            let mut v = Vec::with_capacity(size);
            while v.len() < size {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                v.extend_from_slice(&s.to_le_bytes());
            }
            v.truncate(size);
            v
        }

        /// Scan a REAL folder with the REAL Windows platform, then run
        /// the exact production pipeline over it. Planted content:
        /// - 3 identical 8 MiB files → one group, 3 copies
        /// - 2 same-size 8 MiB files with an IDENTICAL 64 KiB prefix
        ///   but differences inside the mid windows → screened out
        /// - 2 identical 300 KiB files → small-file path, one group
        /// - 3 zero-byte files → ignored (spec §10)
        /// - 1 file held open with share_mode(0) → unreadable, skipped
        #[test]
        fn pipeline_finds_planted_groups_and_screens_false_positives() {
            let root = scratch("main");
            let _keep = TempTree(root.clone());
            let mib = 1024 * 1024u64;
            let prefix = engine::DEFAULT_PREFIX;
            let big = blob(0xC0FF_EEEE, 8 * mib as usize);
            // Same 64 KiB prefix, one flipped byte INSIDE window A
            // (+512 KiB) and one INSIDE window B (size - 512 KiB) —
            // the screens must kill this pair before any verify read.
            let mut fp_a = blob(1, 8 * mib as usize);
            let mut fp_b = blob(1, 8 * mib as usize);
            fp_a[..prefix as usize].copy_from_slice(&big[..prefix as usize]);
            fp_b[..prefix as usize].copy_from_slice(&big[..prefix as usize]);
            fp_a[(prefix + mib / 2) as usize] ^= 0xFF;
            fp_b[(8 * mib - mib / 2) as usize] ^= 0xFF;
            let small = blob(0xABCD_CDEF, 300 * 1024);

            fs::write(root.join("dup-a.bin"), &big).unwrap();
            fs::write(root.join("dup-b.bin"), &big).unwrap();
            fs::write(root.join("dup-c.bin"), &big).unwrap();
            fs::write(root.join("fp-a.bin"), &fp_a).unwrap();
            fs::write(root.join("fp-b.bin"), &fp_b).unwrap();
            fs::write(root.join("small-1.bin"), &small).unwrap();
            fs::write(root.join("small-2.bin"), &small).unwrap();
            fs::write(root.join("z1.dat"), b"").unwrap();
            fs::write(root.join("z2.dat"), b"").unwrap();
            fs::write(root.join("z3.dat"), b"").unwrap();
            // Locked file (same size as the small pair → would group if
            // readable): hold it open with NO sharing for the whole
            // test.
            fs::write(root.join("small-locked.bin"), &small).unwrap();
            let _locked = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(root.join("small-locked.bin"))
                .expect("open locked");

            // REAL scan of the folder.
            let platform: Arc<crate::platform::HostPlatform> =
                Arc::new(crate::platform::HostPlatform);
            let cancel = Arc::new(AtomicBool::new(false));
            let progress = Arc::new(Mutex::new(Progress::default()));
            let tree = match scan(
                platform,
                &ScanTarget::Folder(root.to_string_lossy().into_owned()),
                1,
                &cancel,
                &progress,
            ) {
                ScanOutcome::Done(t) => Arc::new(t),
                other => panic!("scan failed: {other:?}"),
            };
            assert!(
                tree.len() >= 12,
                "scan must see the planted tree (got {})",
                tree.len()
            );

            // The exact production pipeline (quiet ctl, no events).
            let t0 = Instant::now();
            let ctl = DupesCtl::quiet(Arc::new(AtomicU64::new(0)));
            let result = compute_dupes(&tree, &ctl, tree.root).expect("pipeline");
            let elapsed = t0.elapsed();

            // Exactly 2 groups: the 8 MiB triple and the 300 KiB pair.
            assert_eq!(result.groups.len(), 2, "groups: {result:#?}");
            let mut by_size: Vec<(u64, u64, u64)> = result
                .groups
                .iter()
                .map(|g| (g.size, g.count, g.wasted))
                .collect();
            by_size.sort_unstable();
            assert_eq!(
                by_size,
                vec![(300 * 1024, 2, 300 * 1024), (8 * mib, 3, 2 * 8 * mib),],
                "groups: {result:#?}"
            );
            // Neither the screened pair, the locked file, nor the
            // zero-size files may appear anywhere.
            for g in &result.groups {
                for f in &g.files {
                    let p = &f.path;
                    assert!(!p.contains("fp-a"), "false positive leaked: {p}");
                    assert!(!p.contains("fp-b"), "false positive leaked: {p}");
                    assert!(!p.contains("locked"), "locked file leaked: {p}");
                    assert!(!p.contains("z1"), "zero-size leaked: {p}");
                }
            }
            // Candidate census: 8 sizeable files (3 dup + 2 fp + 2
            // small + 1 locked); the 3 zero-byte files never collect.
            assert_eq!(result.files, 8, "files considered");
            println!(
                "dupes E2E: 2 groups in {} ms (26 MiB staged)",
                elapsed.as_millis()
            );
        }

        /// Format magics (realistic headers so every family reads like a
        /// genuine file to the OS cache + Defender) — shared by the
        /// performance corpus below.
        const FORMATS: [(&str, &[u8]); 8] = [
            (
                "jpg",
                &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'],
            ),
            ("png", &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            (
                "mp4",
                &[
                    0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm',
                ],
            ),
            ("zip", &[b'P', b'K', 0x03, 0x04, 0x14, 0x00, 0x00, 0x00]),
            ("pdf", b"%PDF-1.7"),
            ("iso", &[0x01, b'C', b'D', 0x00, 0x01]),
            ("txt", b"DiskGenie"),
            ("bin", &[0x7F, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00]),
        ];

        /// Magic-headed pseudo-random payload (xorshift64 stream).
        fn fmt_blob(fmt: usize, seed: u64, size: usize) -> Vec<u8> {
            let mut s = seed | 1;
            let mut v = Vec::with_capacity(size);
            while v.len() < size {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                v.extend_from_slice(&s.to_le_bytes());
            }
            v.truncate(size);
            let (_, magic) = FORMATS[fmt % FORMATS.len()];
            let m = magic.len();
            if size >= m {
                v[..m].copy_from_slice(magic);
            }
            v
        }

        /// The PERFORMANCE corpus (owner ask: "spawn fake real-size
        /// multi-format files and then test"): ~1.9 GB of realistic
        /// multi-format content — magic-headed JPG/PNG/MP4/ZIP/PDF/
        /// ISO/TXT/BIN payloads — with planted exact-duplicate
        /// groups, same-prefix near-duplicates (must screen out), a
        /// hardlink pair (must exclude), unicode + deep nesting, and
        /// a locked file. Asserts CORRECTNESS of every planted group
        /// AND an effective throughput floor (the v3 engine must
        /// stay I/O-bound; the floor is generous for Defender-active
        /// CI hardware).
        #[test]
        #[allow(clippy::too_many_lines)]
        fn throughput_corpus_multi_format_finds_groups_fast() {
            let root = scratch("perf");
            let _keep = TempTree(root.clone());
            let t_stage = Instant::now();

            let kib = 1024u64;
            let mib = 1024 * 1024u64;
            let prefix = engine::DEFAULT_PREFIX;
            let mut expected_groups: Vec<(u64, u64, Vec<String>)> = Vec::new();

            // ── SMALL: 300 files (64–192 KiB) across the 8 formats,
            //    29 exact-duplicate pairs (i%10==0, i>0). ~45 MB.
            let small_dir = root.join("photos");
            fs::create_dir_all(&small_dir).unwrap();
            for i in 0..300u64 {
                let fmt = (i % 8) as usize;
                let size = (64 + i % 129) * kib;
                let name = format!("shot_{i:03}.{}", FORMATS[fmt].0);
                fs::write(
                    small_dir.join(&name),
                    fmt_blob(fmt, i * 7 + 1, size as usize),
                )
                .unwrap();
                if i % 10 == 0 && i > 0 {
                    // Duplicate of the PREVIOUS file (same content,
                    // same size) → an exact group of 2. The pair's
                    // size is the PREVIOUS index's (the copied file).
                    let prev = format!("shot_{:03}.{}", i - 1, FORMATS[((i - 1) % 8) as usize].0);
                    let pair_size = (64 + (i - 1) % 129) * kib;
                    fs::copy(
                        small_dir.join(&prev),
                        small_dir.join(name.replace("shot", "copy")),
                    )
                    .unwrap();
                    expected_groups.push((pair_size, 2, vec![prev, name.replace("shot", "copy")]));
                }
            }

            // ── Unicode + deep nesting (2 more small groups).
            let deep = root
                .join("备份")
                .join(" archival")
                .join("ännu")
                .join("-depth-")
                .join("₄");
            fs::create_dir_all(&deep).unwrap();
            let uni_a = fmt_blob(2, 0x0BAD_BEEF, 300 * 1024);
            fs::write(deep.join("🎞 video ñ.mp4"), &uni_a).unwrap();
            fs::write(deep.join("🎞 video ñ (copy).mp4"), &uni_a).unwrap();
            expected_groups.push((
                300 * kib,
                2,
                vec!["🎞 video ñ.mp4".into(), "🎞 video ñ (copy).mp4".into()],
            ));
            let uni_b = fmt_blob(4, 0x0FEE_FACE, 500 * 1024);
            fs::write(deep.join("документ.pdf"), &uni_b).unwrap();
            fs::write(deep.join("документ — копия.pdf"), &uni_b).unwrap();
            expected_groups.push((
                500 * kib,
                2,
                vec!["документ.pdf".into(), "документ — копия.pdf".into()],
            ));

            // ── MEDIUM: 60 files (4–16 MiB) + 10 duplicate groups.
            //    ~740 MB.
            let media_dir = root.join("media");
            fs::create_dir_all(&media_dir).unwrap();
            for i in 0..60u64 {
                let fmt = ((i + 2) % 8) as usize;
                let size = (4 + i % 13) * mib;
                let name = format!("clip_{i:02}.{}", FORMATS[fmt].0);
                fs::write(
                    media_dir.join(&name),
                    fmt_blob(fmt, i * 0x9E37 + 5, size as usize),
                )
                .unwrap();
                if i % 6 == 5 {
                    fs::copy(
                        media_dir.join(&name),
                        media_dir.join(name.replace("clip", "mirror")),
                    )
                    .unwrap();
                    expected_groups.push((
                        size,
                        2,
                        vec![name.clone(), name.replace("clip", "mirror")],
                    ));
                }
            }

            // ── NEAR-DUP: 6 pairs, same size + same 64 KiB prefix,
            //    different mid bytes → the screens must kill them.
            //    ~96 MB.
            let nd_dir = root.join("near-dups");
            fs::create_dir_all(&nd_dir).unwrap();
            for i in 0..6u64 {
                // Seed stride 2, never consecutive: fmt_blob folds the
                // seed with `| 1`, so consecutive seeds COLLAPSED into
                // one xorshift stream in an earlier round (the CI
                // found the a-pairs and b-pairs as six REAL duplicate
                // groups — the pipeline was right; the fixture planted
                // twins it never meant to).
                let base = fmt_blob(3, 0x51DE + i * 2, 8 * mib as usize);
                let mut twin = base.clone();
                twin[(prefix + mib / 2) as usize] ^= 0xA5;
                fs::write(nd_dir.join(format!("nd-a{i}.zip")), &base).unwrap();
                fs::write(nd_dir.join(format!("nd-b{i}.zip")), &twin).unwrap();
            }

            // ── GAP-DIFF pair: same screens, gap difference — the
            //    v3 lockstep verify must reject it (v2's SHA pass
            //    caught it by digest; v3 catches it by chain
            //    divergence in the first divergent block).
            let gap_dir = root.join("gap-diffs");
            fs::create_dir_all(&gap_dir).unwrap();
            let gap_base = fmt_blob(2, 0x6A7_8B9, 12 * mib as usize);
            let mut gap_twin = gap_base.clone();
            gap_twin[(6 * mib) as usize] ^= 0x5A;
            fs::write(gap_dir.join("gp-a.mp4"), &gap_base).unwrap();
            fs::write(gap_dir.join("gp-b.mp4"), &gap_twin).unwrap();

            // ── HARDLINK pair: same content twice on disk, but one
            //    file identity → NOT a duplicate (spec §10).
            let hl_dir = root.join("hardlinked");
            fs::create_dir_all(&hl_dir).unwrap();
            let hl_src = fmt_blob(5, 0x1BAD_B002, 6 * mib as usize);
            fs::write(hl_dir.join("hl-orig.iso"), &hl_src).unwrap();
            let hl_ok =
                fs::hard_link(hl_dir.join("hl-orig.iso"), hl_dir.join("hl-link.iso")).is_ok();
            if !hl_ok {
                println!("(hardlink creation unavailable — skipping that assertion)");
            }

            // ── LOCKED file (same size as a planted medium group —
            //    would group if readable): hold it unshared.
            fs::write(
                root.join("locked.zip"),
                // 9 MiB = the i=5 medium file's size (4 + 5%13): same
                // size bucket, DIFFERENT content — if it were readable
                // it would be screened out at the prefix tier.
                fmt_blob(3, 0x5EED + 5, (9 * mib) as usize),
            )
            .unwrap();
            let _locked = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(root.join("locked.zip"))
                .expect("open locked");

            // ── LARGE: 6 files × 128 MiB + ONE exact triple. ~1.2 GB.
            let big_dir = root.join("vms");
            fs::create_dir_all(&big_dir).unwrap();
            for i in 0..6u64 {
                let size = 128 * mib;
                let name = format!("vm-disk{i}.bin");
                fs::write(
                    big_dir.join(&name),
                    fmt_blob(7, 0xD15C + i * 3, size as usize),
                )
                .unwrap();
                if i == 3 {
                    fs::copy(
                        big_dir.join(&name),
                        big_dir.join(name.replace("vm-disk", "vm-copy")),
                    )
                    .unwrap();
                    fs::copy(
                        big_dir.join(&name),
                        big_dir.join(name.replace("vm-disk", "vm-clone")),
                    )
                    .unwrap();
                    expected_groups.push((
                        size,
                        3,
                        vec![
                            name.clone(),
                            name.replace("vm-disk", "vm-copy"),
                            name.replace("vm-disk", "vm-clone"),
                        ],
                    ));
                }
            }
            println!(
                "staged ~1.9 GB corpus in {:.1}s (hardlinks ok={hl_ok})",
                t_stage.elapsed().as_secs_f32()
            );

            // REAL scan + the REAL production pipeline.
            let platform: Arc<crate::platform::HostPlatform> =
                Arc::new(crate::platform::HostPlatform);
            let cancel = Arc::new(AtomicBool::new(false));
            let progress = Arc::new(Mutex::new(Progress::default()));
            let tree = match scan(
                platform,
                &ScanTarget::Folder(root.to_string_lossy().into_owned()),
                1,
                &cancel,
                &progress,
            ) {
                ScanOutcome::Done(t) => Arc::new(t),
                other => panic!("scan failed: {other:?}"),
            };

            let t0 = Instant::now();
            let ctl = DupesCtl::quiet(Arc::new(AtomicU64::new(0)));
            let result = compute_dupes(&tree, &ctl, tree.root).expect("pipeline");
            let elapsed = t0.elapsed();
            let read_bytes = ctl.bytes_all.load(Ordering::Relaxed);
            let mps = read_bytes as f64 / elapsed.as_secs_f64() / (mib as f64);
            println!(
                "dupes PERF (v3): {} files → {} groups, {:.0} MiB read in {:.2}s = {:.1} MiB/s effective",
                result.files,
                result.groups.len(),
                read_bytes / mib,
                elapsed.as_secs_f32(),
                mps,
            );

            // ── Correctness: every planted group is found, exactly.
            assert_eq!(
                result.groups.len(),
                expected_groups.len(),
                "planted {} groups, found {}: {:#?}",
                expected_groups.len(),
                result.groups.len(),
                result.groups
            );
            for (size, count, names) in &expected_groups {
                let hit = result
                    .groups
                    .iter()
                    .find(|g| g.size == *size && g.count == *count);
                assert!(hit.is_some(), "missing group size={size} count={count}");
                for n in names {
                    assert!(
                        hit.unwrap()
                            .files
                            .iter()
                            .any(|f| f.path.contains(n.as_str())),
                        "group member {n} missing: {:?}",
                        hit.unwrap().files
                    );
                }
            }
            // The near-dups, the gap-diff pair, the hardlink twin,
            // and the locked file must never appear.
            for g in &result.groups {
                for f in &g.files {
                    let p = &f.path;
                    assert!(!p.contains("nd-"), "near-dup leaked: {p}");
                    assert!(!p.contains("gp-"), "gap-diff leaked: {p}");
                    if hl_ok {
                        assert!(!p.contains("hl-link"), "hardlink pair leaked: {p}");
                    }
                    assert!(!p.contains("locked.zip"), "locked file leaked: {p}");
                }
            }

            // ── Throughput floor (the I/O-bound regression guard).
            //    v3 reads each byte at most once with memory-speed
            //    hashing; even Defender-active CI hardware must clear
            //    this comfortably (v2's floor was 40 MiB/s — v3's
            //    expected range is hundreds of MiB/s).
            assert!(
                mps >= 60.0,
                "effective throughput {mps:.1} MiB/s is below the 60 MiB/s floor — the engine became CPU- or open-bound"
            );
        }

        /// Cancellation: bump the shared generation after the run
        /// latched → the pipeline reports Err("cancelled").
        #[test]
        fn cancellation_mid_pipeline_is_reported() {
            let root = scratch("cancel");
            let _keep = TempTree(root.clone());
            for i in 0..6u64 {
                // Two content groups of 3 files each, all 4 MiB.
                let b = blob(i / 3, 4 * 1024 * 1024);
                fs::write(root.join(format!("c{i}.bin")), &b).unwrap();
            }
            let platform: Arc<crate::platform::HostPlatform> =
                Arc::new(crate::platform::HostPlatform);
            let cancel = Arc::new(AtomicBool::new(false));
            let progress = Arc::new(Mutex::new(Progress::default()));
            let tree = match scan(
                platform,
                &ScanTarget::Folder(root.to_string_lossy().into_owned()),
                1,
                &cancel,
                &progress,
            ) {
                ScanOutcome::Done(t) => Arc::new(t),
                other => panic!("scan failed: {other:?}"),
            };
            let gen = Arc::new(AtomicU64::new(0));
            let ctl = DupesCtl::quiet(Arc::clone(&gen));
            // Simulate a cancel landing after collect: bump before the
            // screen pass checks it.
            gen.fetch_add(1, Ordering::SeqCst);
            let out = compute_dupes(&tree, &ctl, tree.root);
            assert_eq!(out.err(), Some("cancelled".to_string()));
        }
    }
}
