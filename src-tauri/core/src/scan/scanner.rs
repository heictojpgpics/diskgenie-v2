//! The standard-engine scanner (spec §4): a worker pool over the
//! [`Platform`](crate::platform::Platform) seam.
//!
//! - One worker `std::thread` per logical CPU
//!   (`available_parallelism`), pulling jobs from a shared **LIFO job
//!   stack** guarded by `parking_lot::Mutex` + `Condvar`.
//! - The scan ends when the stack is empty and no worker is active.
//! - Each worker enumerates one directory into a local `Vec`, then takes
//!   **one lock** on the arena and appends the whole batch → contiguous
//!   id range; **a parent's id is always smaller than its children's**.
//! - Cancellation: an `AtomicBool` checked between directories; starting
//!   a new scan cancels the old one.
//! - Progress is a lock-protected snapshot updated once per directory
//!   batch, never per file.
//!
//! Rules applied while descending (spec §4):
//! - Never descend into reparse-point directories, EXCEPT cloud-files
//!   placeholders `(tag & 0xFFFF0FFF) == 0x9000_001A`.
//! - `FILE_ATTRIBUTE_OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS`
//!   entries are cloud placeholders (never opened or read later).
//! - Skip `System Volume Information` at drive roots; scan everything
//!   else including `pagefile.sys` / `hiberfil.sys` / `swapfile.sys`.
//! - Protected flag per `scan::categories::is_protected_name`
//!   (drive-root `Windows`/`Windows.old`/pagefiles; `WindowsApps`).
//! - Files under `\Windows\WinSxS` are counted once per
//!   `(volume serial, FileId)`.
//! - Access-denied folders are counted with up to 8 sample paths;
//!   vanished files are ignored silently.
//!
//! After the workers finish, `scan::rollup::finalize` (the reverse
//! linear pass + CSR order) completes the tree; the caller swaps the
//! finished `Arc<Tree>` into app state and drops the old one on a
//! background thread.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};

use crate::platform::{ListError, Platform};
use crate::scan::categories::FileCategory;
use crate::scan::node::{BatchEntry, Node, Tree};

/// Reparse-tag mask/compare for `OneDrive` / cloud-files placeholder
/// directories — the ONE kind of reparse point we descend (spec §4).
const CLOUD_REPARSE_MASKED: u32 = 0x9000_001A;
const CLOUD_REPARSE_MASK: u32 = 0xFFFF0FFF;

/// Cloud placeholder reparse tag test (spec §4).
#[must_use]
pub fn is_cloud_reparse_dir(reparse_tag: u32) -> bool {
    (reparse_tag & CLOUD_REPARSE_MASK) == CLOUD_REPARSE_MASKED
}

/// Progress snapshot (spec §4: updated once per directory batch).
///
/// Wire format: camelCase (`currentPath`, `deniedSamples`) — serialized
/// as-is inside `StatusResponse` and the `scan-progress` event; the JS
/// `ScanProgress` type + mock read camelCase.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    /// Files appended so far.
    pub files: u64,
    /// Folders appended so far.
    pub folders: u64,
    /// Logical bytes appended so far.
    pub bytes: u64,
    /// The directory being enumerated (display form, no `\\?\`).
    pub current_path: String,
    /// Access-denied folder count.
    pub denied: u64,
    /// Up to 8 denied sample paths (spec §4).
    pub denied_samples: Vec<String>,
}

/// What a scan targets (spec §4): a folder, a drive root, or This PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanTarget {
    /// A folder path (display form, e.g. `C:\Users\z\Downloads`).
    Folder(String),
    /// A drive root (display form, e.g. `C:\`).
    Drive(String),
    /// The synthetic This PC root: children are `DRIVE_FIXED` roots.
    ThisPc,
}

impl ScanTarget {
    /// The verbatim `\\?\` form of the target's root path (`None` for
    /// This PC, which is synthetic).
    #[must_use]
    pub fn root_path(&self) -> Option<String> {
        match self {
            Self::Folder(p) | Self::Drive(p) => Some(Tree::verbatim(p)),
            Self::ThisPc => None,
        }
    }
}

/// Context carried per directory job — everything the spec's per-entry
/// rules need to know about the directory being listed. The flags are
/// orthogonal spec §4 rule switches, not a modeling smell.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // spec §4 rules: drive-root / program-files / apps / windows / winsxs
struct DirCtx {
    /// The directory IS a drive root → its children get the protected
    /// drive-root name check + `System Volume Information` skip.
    is_drive_root: bool,
    /// The directory IS `Program Files` / `Program Files (x86)` → its
    /// children get the `WindowsApps` protected check.
    is_program_files: bool,
    /// The directory is at/inside an apps root → children inherit
    /// `IN_APPS` + `Apps` classification (spec §4).
    in_apps: bool,
    /// The directory is `\Windows` at a drive root.
    in_windows: bool,
    /// The directory is at/inside `\Windows\WinSxS` → files deduped by
    /// (volume serial, `FileId`).
    in_winsxs: bool,
    /// Volume serial for `WinSxS` dedup (resolved once when entering
    /// `WinSxS`; `0` = unresolved).
    volume_serial: u64,
}

/// One pending directory to enumerate.
struct Job {
    /// Node id of the directory.
    node: u32,
    /// Verbatim path of the directory.
    path: String,
    /// Display path (progress reporting).
    display: String,
    ctx: DirCtx,
}

/// Shared scanner state. The job stack AND the active-worker count live
/// behind ONE mutex so "stack empty && nobody active" is checked
/// atomically (the spec §4 termination rule — merging them removes the
/// lost-wakeup race where a worker pops the last job while another sees
/// an empty stack with a not-yet-incremented active count).
struct Shared<P: Platform> {
    platform: Arc<P>,
    /// (LIFO job stack, active worker count).
    state: Mutex<(Vec<Job>, usize)>,
    idle: Condvar,
    tree: Mutex<Tree>,
    progress: Arc<Mutex<Progress>>,
    winsxs_seen: Mutex<HashSet<(u64, u64)>>,
    /// Resolved spec §4 apps roots (compared when directories are
    /// enqueued, case-insensitively).
    apps_roots: Vec<String>,
    /// Worker population (bounds per-push wakeups in `process_job`).
    worker_count: usize,
}

/// How long idle workers sleep between cancel re-checks (they cannot be
/// woken by the canceller directly across the trait seam; 50 ms keeps
/// shutdown latency low and costs nothing while scanning).
const IDLE_RECHECK: std::time::Duration = std::time::Duration::from_millis(50);

/// Hard worker cap. Directory enumeration is I/O-bound: past ~16
/// workers the single job-stack mutex and the per-batch tree lock see
/// more contention than the extra parallelism returns (disktree's
/// measured 3.4M-file Windows walk: 6.5 s at 16 threads vs 8.1 s at
/// 24 — the reference project caps its walk pool for exactly this
/// reason). Also bounds per-thread enumeration-buffer memory.
const MAX_WORKERS: usize = 16;

/// The scan outcome.
#[derive(Debug)]
pub enum ScanOutcome {
    /// The finished tree (rolled up + ordered).
    Done(Tree),
    /// Cancelled (a newer scan replaced this one, or the caller asked).
    Cancelled,
    /// The target root could not be opened.
    RootFailed(String),
}

/// Run a full scan of `target` with `platform` at generation
/// `generation` (spec §4). Blocks until the scan completes, is
/// cancelled via `cancel`, or the root fails.
///
/// The caller owns the returned [`Tree`] (wrap in `Arc`, swap into
/// state, drop the old tree on a background thread).
pub fn scan<P: Platform>(
    platform: Arc<P>,
    target: &ScanTarget,
    generation: u64,
    cancel: &Arc<AtomicBool>,
    progress: &Arc<Mutex<Progress>>,
) -> ScanOutcome {
    let mut tree = match build_root(platform.as_ref(), target, generation) {
        Ok(t) => t,
        Err(e) => return ScanOutcome::RootFailed(e),
    };
    let initial = initial_jobs(&mut tree, target);
    let apps_roots = resolved_apps_roots(platform.as_ref());

    let workers = std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .min(MAX_WORKERS);
    let shared = Arc::new(Shared {
        platform,
        state: Mutex::new((initial, 0)),
        idle: Condvar::new(),
        tree: Mutex::new(tree),
        // The caller's sink: the 150 ms ticker / get_status read it live.
        progress: Arc::clone(progress),
        winsxs_seen: Mutex::new(HashSet::new()),
        apps_roots,
        worker_count: workers,
    });

    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let s = Arc::clone(&shared);
        let c = Arc::clone(cancel);
        handles.push(std::thread::spawn(move || worker_loop(&s, &c)));
    }
    for h in handles {
        let _ = h.join();
    }

    if cancel.load(Ordering::SeqCst) {
        return ScanOutcome::Cancelled;
    }
    let mut tree = shared.tree.lock();
    crate::scan::rollup::finalize(&mut tree);
    ScanOutcome::Done(std::mem::take(&mut *tree))
}

/// Resolve the spec §4 apps roots once (before any entry is classified).
fn resolved_apps_roots<P: Platform>(platform: &P) -> Vec<String> {
    use crate::platform::KnownFolder;
    let mut roots = Vec::with_capacity(4);
    for folder in [
        KnownFolder::ProgramFiles,
        KnownFolder::ProgramFilesX86,
        KnownFolder::UserPrograms,
        KnownFolder::ProgramFilesWindowsApps,
    ] {
        if let Some(p) = platform.known_folder(folder) {
            roots.push(p);
        }
    }
    roots
}

/// The synthetic whole-machine root's display label. The Windows
/// convention ("This PC", the Explorer name) and the Mac one ("Full
/// Mac" — the sidebar CTA "Scan Full Mac" vocabulary, Mac `BuildPrompt`
/// §5.1) both describe the same virtual root; the label is DATA (the
/// root node's name, the breadcrumb root crumb, the inspector title,
/// the snapshot root label, the sidebar storage card's aggregate
/// label) and flows from this ONE point.
#[must_use]
pub fn this_pc_display_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Full Mac"
    } else {
        "This PC"
    }
}

/// Build the initial tree: synthetic This PC root with drive children,
/// or a single root carrying its path.
fn build_root<P: Platform>(
    platform: &P,
    target: &ScanTarget,
    generation: u64,
) -> Result<Tree, String> {
    let mut t = Tree::new(generation);
    match target {
        ScanTarget::ThisPc => {
            let label = this_pc_display_label();
            t.set_name(0, label);
            t.this_pc_label = Some(label.into());
            let drives = platform.fixed_drive_roots();
            if drives.is_empty() {
                return Err("no fixed drives found".into());
            }
            let entries: Vec<BatchEntry> = drives
                .iter()
                .map(|d| {
                    let mut n = Node::new_dir();
                    n.modified = now_unix();
                    BatchEntry {
                        name: trim_root_name(d).encode_utf16().collect(),
                        node: n,
                    }
                })
                .collect();
            let base = t.append_batch(0, entries);
            for (i, d) in drives.iter().enumerate() {
                t.add_root_path(base + i as u32, d);
            }
            Ok(t)
        }
        ScanTarget::Folder(p) | ScanTarget::Drive(p) => {
            // The root NODE needs a display name (Folders header,
            // breadcrumb, inspector) — the RootRef path alone does not
            // provide one (CI caught the empty-name root: every header
            // showed "" for the scan root).
            t.set_name(0, &display_root_name(p));
            t.add_root_path(0, p);
            Ok(t)
        }
    }
}

/// The root node's display name: the final path component
/// (`C:\Base` → `Base`), or the drive letter for drive roots
/// (`C:\` → `C:`). Falls back to the whole path when no separator
/// is present.
fn display_root_name(p: &str) -> String {
    if p.len() == 3 && p.as_bytes().get(2) == Some(&b'\\') {
        return p.trim_end_matches('\\').to_string();
    }
    let last = p.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or(p);
    last.to_string()
}

/// `C:\` → `C:` (the drive node's name under This PC).
fn trim_root_name(root: &str) -> String {
    root.trim_end_matches('\\').to_string()
}

/// The first jobs: descend from the tree root.
fn initial_jobs(tree: &mut Tree, target: &ScanTarget) -> Vec<Job> {
    let mut jobs = Vec::new();
    match target {
        ScanTarget::ThisPc => {
            for r in &tree.roots.clone() {
                jobs.push(Job {
                    node: r.node,
                    path: Tree::verbatim(&r.path),
                    display: r.path.clone(),
                    ctx: DirCtx {
                        is_drive_root: true,
                        ..DirCtx::default()
                    },
                });
            }
        }
        ScanTarget::Folder(p) | ScanTarget::Drive(p) => {
            let verbatim = Tree::verbatim(p);
            let is_drive = p.len() == 3 && p.as_bytes().get(2) == Some(&b'\\');
            jobs.push(Job {
                node: 0,
                path: verbatim,
                display: p.clone(),
                ctx: DirCtx {
                    is_drive_root: is_drive,
                    ..DirCtx::default()
                },
            });
        }
    }
    jobs
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// One worker's loop (spec §4 LIFO protocol). Terminates when the stack
/// is empty AND no worker is active, or when cancelled.
fn worker_loop<P: Platform>(shared: &Shared<P>, cancel: &AtomicBool) {
    loop {
        // Take a job (or wait); active++ happens atomically with the pop
        // so no other worker can observe an empty-but-not-done stack.
        let job = {
            let mut g = shared.state.lock();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(j) = g.0.pop() {
                    g.1 += 1;
                    break j;
                }
                if g.1 == 0 {
                    return; // stack empty, nobody active: scan complete
                }
                // Sleep with a periodic cancel re-check.
                shared.idle.wait_for(&mut g, IDLE_RECHECK);
            }
        };
        if cancel.load(Ordering::Relaxed) {
            // Undo the active count on the early-out path.
            shared.state.lock().1 -= 1;
            shared.idle.notify_all();
            return;
        }
        process_job(shared, &job);
        {
            let mut g = shared.state.lock();
            g.1 -= 1;
            // Children were pushed by process_job under the same lock with
            // one notify per pushed job, so sleepers are already racing to
            // the stack. Wake EVERYONE only for the termination broadcast:
            // "stack empty && nobody active" is the one state every sleeper
            // must observe to exit. (The old unconditional notify_all woke
            // all workers after EVERY directory — pure futex + mutex
            // contention on many-core hosts.)
            if g.0.is_empty() && g.1 == 0 {
                drop(g);
                shared.idle.notify_all();
            }
        }
    }
}

/// Enumerate one directory, append the batch, enqueue children
/// (all spec §4 rules), update progress once.
#[allow(clippy::too_many_lines)] // one directory = one spec §4 rule pipeline; splitting scatters the spec
fn process_job<P: Platform>(shared: &Shared<P>, job: &Job) {
    let listing = shared.platform.list_dir(&job.path);

    // Progress/denied bookkeeping (once per batch).
    {
        let mut p = shared.progress.lock();
        p.current_path.clone_from(&job.display);
        match listing.error {
            Some(ListError::AccessDenied | ListError::Other(_)) => {
                p.denied += 1;
                if p.denied_samples.len() < 8 {
                    p.denied_samples.push(job.display.clone());
                }
            }
            Some(ListError::Vanished) | None => {}
        }
    }
    if listing.entries.is_empty() {
        return;
    }

    // Resolve the WinSxS volume serial once, when first needed.
    let mut ctx = job.ctx.clone();
    if ctx.in_winsxs && ctx.volume_serial == 0 {
        ctx.volume_serial = shared.platform.volume_serial(&job.path).unwrap_or(0);
    }

    let mut entries: Vec<BatchEntry> = Vec::with_capacity(listing.entries.len());
    // (entry_index, ctx, verbatim path, display path) — the index is the
    // position in `entries` (files interleaved with dirs), resolved to an
    // arena id via `base + entry_index` after the append.
    let mut children: Vec<(u32, DirCtx, String, String)> = Vec::new();
    let mut stats = (0u64, 0u64, 0u64); // files, folders, bytes

    for e in listing.entries {
        // Skip `System Volume Information` at drive roots (spec §4).
        if job.ctx.is_drive_root && eq_ci(&e.name, "System Volume Information") {
            continue;
        }

        let is_reparse_dir = e.is_dir && e.reparse_tag.is_some();
        let descend = if is_reparse_dir {
            // Cloud placeholder directories are the one exception.
            e.reparse_tag.is_some_and(is_cloud_reparse_dir)
        } else {
            e.is_dir
        };

        // WinSxS hardlink dedup (files only, spec §4).
        if job.ctx.in_winsxs && !e.is_dir && ctx.volume_serial != 0 {
            let key = (ctx.volume_serial, e.file_id);
            let mut seen = shared.winsxs_seen.lock();
            if !seen.insert(key) {
                continue; // already counted once
            }
        }

        let mut node = if e.is_dir {
            Node::new_dir()
        } else {
            Node::new_file()
        };
        node.logical = e.logical;
        node.on_disk = e.on_disk;
        node.modified = e.modified;
        node.created = e.created;
        if e.cloud {
            node.set_cloud_placeholder(true);
        }
        if e.is_dir {
            node.set_category(FileCategory::Other);
        } else {
            node.set_category(FileCategory::resolve(&e.name, job.ctx.in_apps));
        }
        // Protected names (spec §4): drive-root items + WindowsApps under
        // Program Files — via the core table.
        let name_protected = crate::scan::categories::is_protected_name(
            &e.name,
            job.ctx.is_drive_root,
            job.ctx.is_program_files,
        );
        if name_protected {
            node.set_protected(true);
        }
        if job.ctx.in_apps {
            node.set_in_apps_root(true);
        }

        if e.is_dir {
            stats.1 += 1;
        } else {
            stats.0 += 1;
            stats.2 += e.logical;
        }

        // Child job construction happens BEFORE the entry push so `e.name`
        // can be MOVED into the batch (one name allocation per file, not
        // two — a million-file scan saves a million Vec clones). The
        // display name is converted exactly once and reused for both the
        // display path and the verbatim path (the old code converted the
        // same UTF-16 name twice per child directory).
        let entry_index = entries.len() as u32;
        let child = if descend {
            let name_utf8 = String::from_utf16_lossy(&e.name);
            let mut child_display = job.display.clone();
            if !child_display.ends_with('\\') {
                child_display.push('\\');
            }
            child_display.push_str(&name_utf8);
            // Verbatim paths are NOT normalized by the OS: a drive-root
            // job path ends with `\` already, so the separator is added
            // only when missing (a doubled `\\` would be invalid).
            let child_path = if job.path.ends_with('\\') {
                format!("{}{}", job.path, name_utf8)
            } else {
                format!(r"{}\{}", job.path, name_utf8)
            };
            let child_ctx =
                ctx_for_child(&job.ctx, &ctx, &e.name, &child_display, &shared.apps_roots);
            Some((child_ctx, child_path, child_display))
        } else {
            None
        };

        entries.push(BatchEntry { name: e.name, node });

        if let Some((child_ctx, child_path, child_display)) = child {
            // The id this child will receive: its entry was pushed above,
            // so the batch position is entries.len()-1 (files preceding
            // directories shift positions — indexing a stale len attached
            // every subtree to a sibling; pinned by tests).
            children.push((entry_index, child_ctx, child_path, child_display));
        }
    }

    // ONE lock per directory batch (spec §4): append + record child ids,
    // then push the child jobs under the SAME state lock that owns the
    // stack (ids exist before jobs referencing them are visible).
    let mut new_jobs: Vec<Job> = Vec::new();
    {
        let mut tree = shared.tree.lock();
        let base = tree.append_batch(job.node, entries);
        for c in &children {
            new_jobs.push(Job {
                node: base + c.0,
                path: c.2.clone(),
                display: c.3.clone(),
                ctx: c.1.clone(),
            });
        }
    }

    // Progress once per batch.
    {
        let mut p = shared.progress.lock();
        p.files += stats.0;
        p.folders += stats.1;
        p.bytes += stats.2;
    }

    // Push child jobs (LIFO). One notify per pushed job (bounded by the
    // worker count — a notify with no sleeper is a no-op): each woken
    // worker pops one job; workers already active drain the stack on
    // their next loop, so the pipeline never starves. Notifying under
    // the state mutex keeps the predicate check + futex handshake
    // atomic (no lost wakeup window).
    if !new_jobs.is_empty() {
        let mut g = shared.state.lock();
        g.0.extend(new_jobs);
        let wakers = g.0.len().min(shared.worker_count);
        for _ in 0..wakers {
            shared.idle.notify_one();
        }
    }
}

/// The child directory's context from the parent's + the child's name
/// (spec §4 apps-roots/WinSxS/protected rules). `child_display` is the
/// child's display path, compared case-insensitively against the
/// resolved apps roots (spec: "compare each directory as it is
/// enqueued").
fn ctx_for_child(
    parent: &DirCtx,
    current: &DirCtx,
    name: &[u16],
    child_display: &str,
    apps_roots: &[String],
) -> DirCtx {
    let name_eq = |a: &str| eq_ci(name, a);
    let at_apps_root = apps_roots
        .iter()
        .any(|r| r.eq_ignore_ascii_case(child_display));
    DirCtx {
        is_drive_root: false,
        is_program_files: parent.is_drive_root
            && (name_eq("Program Files") || name_eq("Program Files (x86)")),
        in_apps: parent.in_apps || at_apps_root,
        in_windows: parent.is_drive_root && name_eq("Windows"),
        in_winsxs: current.in_winsxs || parent.in_windows && name_eq("WinSxS"),
        // volume serial rides along once resolved (WinSxS only).
        volume_serial: if current.in_winsxs || parent.in_windows && name_eq("WinSxS") {
            current.volume_serial
        } else {
            0
        },
    }
}

/// Case-insensitive ASCII compare of a UTF-16 name against a `&str`.
fn eq_ci(utf16: &[u16], ascii: &str) -> bool {
    if utf16.len() != ascii.len() {
        return false;
    }
    utf16
        .iter()
        .zip(ascii.bytes())
        .all(|(&c, b)| lower_ascii_u16(c) == u16::from(b.to_ascii_lowercase()))
}

/// `A`–`Z` (ASCII) → `a`–`z`; everything else unchanged.
#[inline]
fn lower_ascii_u16(c: u16) -> u16 {
    if (0x41..=0x5A).contains(&c) {
        c + 0x20
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{DirEntryData, DirListing, Platform};
    use std::collections::HashMap;

    /// Test double platform: an in-memory filesystem with the rules the
    /// scanner needs (dirs, files, sizes, reparse/cloud attributes,
    /// access-denied simulation). The REAL engine is the app crate's
    /// `platform/win.rs` (`NtQueryDirectoryFile` class 38).
    struct TestFs {
        /// verbatim path (lowercase, no trailing separator) → children.
        dirs: HashMap<String, Vec<DirEntryData>>,
        failures: HashMap<String, ListError>,
    }

    impl TestFs {
        fn new() -> Self {
            Self {
                dirs: HashMap::new(),
                failures: HashMap::new(),
            }
        }

        fn add(&mut self, dir: &str, children: Vec<(&str, u64, u64)>) {
            let entries = children
                .into_iter()
                .map(|(name, logical, on_disk)| {
                    let is_dir = name.ends_with('\\');
                    let clean = name.trim_end_matches('\\');
                    let e = DirEntryData {
                        name: clean.encode_utf16().collect(),
                        is_dir,
                        logical,
                        on_disk,
                        modified: 1_700_000_000,
                        created: 1_700_000_000,
                        reparse_tag: None,
                        cloud: false,
                        file_id: 0,
                    };
                    if is_dir {
                        // Ensure the child dir exists in the map.
                        self.dirs
                            .entry(Self::key(&format!("{dir}\\{clean}")))
                            .or_default();
                    }
                    e
                })
                .collect();
            self.dirs.insert(Self::key(dir), entries);
        }

        fn key(path: &str) -> String {
            // Strip an optional `\\?\` verbatim prefix (raw strings
            // cannot end in a backslash, hence the escaped literal).
            path.strip_prefix("\\\\?\\")
                .unwrap_or(path)
                .trim_end_matches(['\\', '/'])
                .to_ascii_lowercase()
        }
    }

    impl Platform for TestFs {
        fn list_dir(&self, verbatim_dir: &str) -> DirListing {
            if let Some(err) = self.failures.get(&Self::key(verbatim_dir)) {
                return DirListing {
                    entries: Vec::new(),
                    error: Some(err.clone()),
                };
            }
            DirListing {
                entries: self
                    .dirs
                    .get(&Self::key(verbatim_dir))
                    .cloned()
                    .unwrap_or_default(),
                error: None,
            }
        }
        fn fixed_drive_roots(&self) -> Vec<String> {
            vec!["C:\\".into(), "D:\\".into()]
        }
        fn known_folder(&self, _folder: crate::platform::KnownFolder) -> Option<String> {
            None
        }
        fn volume_serial(&self, _verbatim_path: &str) -> Option<u64> {
            Some(0xABCD)
        }
    }

    fn entry(name: &str, is_dir: bool, logical: u64, on_disk: u64) -> DirEntryData {
        DirEntryData {
            name: name.encode_utf16().collect(),
            is_dir,
            logical,
            on_disk,
            modified: 5,
            created: 5,
            reparse_tag: None,
            cloud: false,
            file_id: 0,
        }
    }

    #[test]
    fn scans_a_tree_and_rolls_up() {
        let mut fs = TestFs::new();
        fs.add(
            r"\\?\C:",
            vec![("Alpha\\", 0, 0), ("Beta\\", 0, 0), ("root.txt", 10, 12)],
        );
        fs.add(
            r"\\?\C:\Alpha",
            vec![("a.bin", 100, 128), ("b.mp4", 500, 512)],
        );
        fs.add(r"\\?\C:\Beta", vec![("c.txt", 30, 32)]);
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(
            platform,
            &ScanTarget::Drive("C:\\".into()),
            7,
            &cancel,
            &progress,
        );
        let tree = match out {
            ScanOutcome::Done(t) => t,
            other => panic!("expected Done, got {other:?}"),
        };
        assert_eq!(tree.generation, 7);
        let (logical, on_disk, files, folders) = tree.root_stats();
        assert_eq!((logical, on_disk, files, folders), (640, 684, 4, 2));
        assert_eq!(tree.node_path(0), "C:\\");
        // Sorted children: Beta (512) before Alpha (628)? on_disk: Alpha
        // 640, Beta 32 → Alpha first.
        assert_eq!(tree.children_sorted(0)[0], 1);
    }

    #[test]
    fn files_before_dirs_keep_children_ids_aligned() {
        // Regression (CI screenshots, session 005): the descend-jobs used
        // `base + position_in_children_vec`, but `children` only holds
        // DESCENDABLE entries — when files precede directories (the real
        // NTFS enumeration order: pagefile.sys sorts before the folders),
        // every job pointed at the wrong arena node and subtrees folded
        // into siblings (Program Files showed Users' bytes, Windows stayed
        // 0 B, pagefile.sys absorbed Program Files' dlls). The old tests
        // passed only because their files sorted AFTER the directories.
        let mut fs = TestFs::new();
        // NTFS-style enumeration: file first, then the three folders.
        fs.add(
            r"\\?\C:",
            vec![
                ("pagefile.sys", 4_000, 4_000),
                ("Program Files\\", 0, 0),
                ("Users\\", 0, 0),
                ("Windows\\", 0, 0),
            ],
        );
        fs.add(r"\\?\C:\Program Files", vec![("tool.dll", 5, 5)]);
        fs.add(r"\\?\C:\Users", vec![("video.mp4", 1_100, 1_100)]);
        fs.add(r"\\?\C:\Windows", vec![("driver.sys", 40, 40)]);
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(
            platform,
            &ScanTarget::Drive("C:\\".into()),
            3,
            &cancel,
            &progress,
        );
        let tree = match out {
            ScanOutcome::Done(t) => t,
            other => panic!("expected Done, got {other:?}"),
        };
        let (logical, on_disk, files, folders) = tree.root_stats();
        assert_eq!((logical, on_disk, files, folders), (5_145, 5_145, 4, 3));
        // Every child carries ITS OWN subtree, by name (arena ids are an
        // implementation detail — the UI reads name + on_disk pairs).
        let by_name: std::collections::HashMap<String, u64> = tree
            .children_sorted(0)
            .iter()
            .map(|&id| (tree.name(id).clone(), tree.node(id).unwrap().on_disk))
            .collect();
        assert_eq!(
            by_name["pagefile.sys"], 4_000,
            "file must not absorb siblings"
        );
        assert_eq!(by_name["Program Files"], 5);
        assert_eq!(by_name["Users"], 1_100);
        assert_eq!(by_name["Windows"], 40);
        // Largest-first order: pagefile, Users, Windows, Program Files.
        let names: Vec<String> = tree
            .children_sorted(0)
            .iter()
            .map(|&id| tree.name(id).clone())
            .collect();
        assert_eq!(
            names,
            vec![
                "pagefile.sys".to_string(),
                "Users".into(),
                "Windows".into(),
                "Program Files".into()
            ]
        );
    }

    #[test]
    fn this_pc_synthetic_root_lists_drives() {
        let fs = TestFs::new();
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(platform, &ScanTarget::ThisPc, 1, &cancel, &progress);
        let tree = match out {
            ScanOutcome::Done(t) => t,
            other => panic!("expected Done, got {other:?}"),
        };
        assert_eq!(tree.node_path(0), this_pc_display_label());
        assert_eq!(tree.roots.len(), 2);
        assert_eq!(tree.node_path(1), "C:\\");
        assert_eq!(tree.node_path(2), "D:\\");
    }

    #[test]
    fn wake_one_protocol_terminates_on_every_shape() {
        // Guard for the notify-discipline change: wakeups are now
        // per-pushed-job (notify_one under the state lock) plus the
        // termination broadcast (notify_all when the stack empties and
        // the last worker goes idle). Every tree shape must still reach
        // "stack empty && nobody active" — a missed wakeup would hang
        // this test forever (the 50 ms IDLE_RECHECK is the self-heal, so
        // a hang means a protocol bug, not a slow wakeup).
        fn dir_name(name: String) -> (&'static str, u64, u64) {
            (Box::leak(name.into_boxed_str()), 0, 0)
        }
        fn file_row(name: &str, logical: u64, on_disk: u64) -> (&'static str, u64, u64) {
            (
                Box::leak(name.to_string().into_boxed_str()),
                logical,
                on_disk,
            )
        }
        for shape in 0..6 {
            let mut fs = TestFs::new();
            match shape {
                // Single directory, no children.
                0 => {
                    fs.add(r"\\?\C:", vec![file_row("only.txt", 1, 1)]);
                }
                // Wide fan-out: 120 directories in the root, each with one
                // file (one worker pushes 120 jobs + 120 notify_ones).
                1 => {
                    let children: Vec<(&str, u64, u64)> =
                        (0..120).map(|i| dir_name(format!("d{i}\\"))).collect();
                    fs.add(r"\\?\C:", children);
                    for i in 0..120 {
                        fs.add(&format!(r"\\?\C:\d{i}"), vec![file_row("f.bin", 10, 10)]);
                    }
                }
                // Deep chain: 40 levels, one child each (the LIFO stack
                // never grows past 1 — the classic lost-wakeup trap).
                2 => {
                    fs.add(r"\\?\C:", vec![dir_name("l0\\".to_string())]);
                    for i in 0..40 {
                        let parent = if i == 0 {
                            r"\\?\C:".to_string()
                        } else {
                            format!(r"\\?\C:\l{}", i - 1)
                        };
                        fs.add(&parent, vec![dir_name(format!("l{i}\\"))]);
                    }
                    fs.add(r"\\?\C:\l39", vec![file_row("deep.bin", 7, 7)]);
                }
                // Mixed: fan-out then chains then leaves.
                3 => {
                    fs.add(
                        r"\\?\C:",
                        vec![
                            dir_name("a\\".to_string()),
                            dir_name("b\\".to_string()),
                            dir_name("c\\".to_string()),
                        ],
                    );
                    for b in ["a", "b", "c"] {
                        let kids: Vec<(&str, u64, u64)> =
                            (0..30).map(|i| dir_name(format!("{b}{i}\\"))).collect();
                        fs.add(&format!(r"\\?\C:\{b}"), kids);
                        for i in 0..30 {
                            fs.add(
                                &format!(r"\\?\C:\{b}\{b}{i}"),
                                vec![file_row("leaf.txt", 3, 3)],
                            );
                        }
                    }
                }
                // Empty directories only (immediate empty listings).
                4 => {
                    let kids: Vec<(&str, u64, u64)> =
                        (0..50).map(|i| dir_name(format!("e{i}\\"))).collect();
                    fs.add(r"\\?\C:", kids);
                }
                _ => {
                    // Roots + nested files interleaved (files before dirs).
                    fs.add(
                        r"\\?\C:",
                        vec![
                            file_row("z_file.txt", 100, 100),
                            dir_name("y_dir\\".to_string()),
                            file_row("x_file.txt", 50, 50),
                        ],
                    );
                    fs.add(r"\\?\C:\y_dir", vec![file_row("inner.bin", 9, 9)]);
                }
            }
            let platform: Arc<TestFs> = Arc::new(fs);
            let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
            let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
            let out = scan(
                platform,
                &ScanTarget::Drive("C:\\".into()),
                100 + shape as u64,
                &cancel,
                &progress,
            );
            let tree = match out {
                ScanOutcome::Done(t) => t,
                other => panic!("shape {shape}: expected Done, got {other:?}"),
            };
            assert_eq!(tree.generation, 100 + shape as u64);
        }
    }

    #[test]
    fn root_nodes_carry_display_names() {
        // Regression (CI): folder/drive/This-PC roots must have a real
        // arena name — the Folders header, breadcrumb root crumb and
        // inspector title all read `tree.name(root)`.
        assert_eq!(display_root_name(r"C:\Base"), "Base");
        assert_eq!(display_root_name(r"C:\"), "C:");
        assert_eq!(display_root_name(r"D:\"), "D:");
        assert_eq!(display_root_name(r"C:\Users\eve\Documents"), "Documents");
        assert_eq!(display_root_name("Relative"), "Relative");

        let mut fs = TestFs::new();
        fs.add(r"\\?\C:", vec![("a.txt", 5, 5)]);
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(
            platform,
            &ScanTarget::Drive("C:\\".into()),
            7,
            &cancel,
            &progress,
        );
        let tree = match out {
            ScanOutcome::Done(t) => t,
            other => panic!("expected Done, got {other:?}"),
        };
        assert_eq!(tree.name(0), "C:");
        // And This PC keeps its label as the root name too.
        let fs2 = TestFs::new();
        let platform2: Arc<TestFs> = Arc::new(fs2);
        let cancel2: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress2: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out2 = scan(platform2, &ScanTarget::ThisPc, 8, &cancel2, &progress2);
        if let ScanOutcome::Done(t2) = out2 {
            assert_eq!(t2.name(0), this_pc_display_label());
        }
    }

    #[test]
    fn cancellation_stops_the_scan() {
        // A deep chain that would take forever without cancellation.
        let mut fs = TestFs::new();
        let mut path = r"\\?\C:".to_string();
        for depth in 0..40 {
            let child = format!("d{depth}\\");
            fs.add(&path, vec![(child.as_str(), 0, 0)]);
            path = format!("{path}\\d{depth}");
        }
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(true)); // already cancelled
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(
            platform,
            &ScanTarget::Drive("C:\\".into()),
            2,
            &cancel,
            &progress,
        );
        assert!(matches!(out, ScanOutcome::Cancelled));
    }

    #[test]
    fn access_denied_is_counted_and_sampled() {
        let mut fs = TestFs::new();
        fs.add(r"\\?\C:", vec![("Locked\\", 0, 0), ("ok.txt", 1, 1)]);
        fs.failures
            .insert(TestFs::key(r"\\?\C:\Locked"), ListError::AccessDenied);
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(
            platform,
            &ScanTarget::Drive("C:\\".into()),
            3,
            &cancel,
            &progress,
        );
        assert!(matches!(out, ScanOutcome::Done(_)));
    }

    #[test]
    fn protected_and_system_entries_flagged() {
        let mut fs = TestFs::new();
        fs.add(
            r"\\?\C:",
            vec![
                ("Windows\\", 0, 0),
                ("pagefile.sys", 8_000_000_000, 8_000_000_000),
                ("System Volume Information\\", 1, 1),
                ("Users\\", 0, 0),
            ],
        );
        fs.add(r"\\?\C:\Windows", vec![("explorer.exe", 100, 128)]);
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(
            Arc::clone(&platform),
            &ScanTarget::Drive("C:\\".into()),
            4,
            &cancel,
            &progress,
        );
        let tree = match out {
            ScanOutcome::Done(t) => t,
            other => panic!("expected Done, got {other:?}"),
        };
        // Windows (id 1) + pagefile.sys (id 2) protected; SVI (id 3)
        // skipped entirely; Users (id 4... after skip) present.
        assert!(tree.node(1).unwrap().is_protected(), "Windows");
        assert!(tree.node(2).unwrap().is_protected(), "pagefile.sys");
        let names: Vec<String> = (1..tree.len() as u32).map(|i| tree.name(i)).collect();
        assert!(!names.iter().any(|n| n == "System Volume Information"));
        assert!(names.iter().any(|n| n == "Users"));
        // pagefile counted (sizes visible).
        assert_eq!(tree.node(2).unwrap().logical, 8_000_000_000);
    }

    #[test]
    fn reparse_dirs_not_descended_except_cloud() {
        let cloud_tag = 0x9000001A; // OneDrive placeholder dir
        let junction_tag = 0xA0000003; // name surrogate (junction)
        let mut fs = TestFs::new();
        fs.add(
            r"\\?\C:",
            vec![
                ("CloudDir\\", 0, 0),
                ("Junction\\", 0, 0),
                ("plain\\", 0, 0),
            ],
        );
        // Mark the entries with reparse tags.
        let entries = fs.dirs.get_mut(&TestFs::key(r"\\?\C:")).unwrap();
        entries[0].reparse_tag = Some(cloud_tag);
        entries[1].reparse_tag = Some(junction_tag);
        // Children exist for all three — only CloudDir + plain descend.
        fs.add(r"\\?\C:\CloudDir", vec![("inner.txt", 5, 5)]);
        fs.add(r"\\?\C:\Junction", vec![("inner.txt", 7, 7)]);
        fs.add(r"\\?\C:\plain", vec![("inner.txt", 9, 9)]);
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(
            platform,
            &ScanTarget::Drive("C:\\".into()),
            5,
            &cancel,
            &progress,
        );
        let tree = match out {
            ScanOutcome::Done(t) => t,
            other => panic!("expected Done, got {other:?}"),
        };
        let names: Vec<String> = (0..tree.len() as u32).map(|i| tree.name(i)).collect();
        assert!(names.iter().any(|n| n == "inner.txt"));
        // Junction node exists but its child never got scanned: total
        // files = 3 inner files? No — only CloudDir + plain children.
        let (_, _, files, _) = tree.root_stats();
        assert_eq!(files, 2);
        assert!(is_cloud_reparse_dir(cloud_tag));
        assert!(!is_cloud_reparse_dir(junction_tag));
    }

    #[test]
    fn cloud_placeholder_files_are_flagged_never_opened() {
        let mut fs = TestFs::new();
        fs.add(r"\\?\C:", vec![("a.txt", 100, 0)]);
        let entries = fs.dirs.get_mut(&TestFs::key(r"\\?\C:")).unwrap();
        entries[0].cloud = true;
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(
            platform,
            &ScanTarget::Drive("C:\\".into()),
            6,
            &cancel,
            &progress,
        );
        let tree = match out {
            ScanOutcome::Done(t) => t,
            other => panic!("expected Done, got {other:?}"),
        };
        assert!(tree.node(1).unwrap().is_cloud_placeholder());
    }

    #[test]
    fn winsxs_hardlinks_counted_once() {
        let mut fs = TestFs::new();
        fs.add(r"\\?\C:", vec![("Windows\\", 0, 0)]);
        fs.add(r"\\?\C:\Windows", vec![("WinSxS\\", 0, 0)]);
        // Two hardlinks: same file id, different names, under WinSxS.
        let mut listing = vec![
            entry("link1.dll", false, 1000, 1000),
            entry("link2.dll", false, 1000, 1000),
            entry("unique.dll", false, 50, 50),
        ];
        listing[0].file_id = 42;
        listing[1].file_id = 42;
        listing[2].file_id = 43;
        fs.dirs
            .insert(TestFs::key(r"\\?\C:\Windows\WinSxS"), listing);
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(
            platform,
            &ScanTarget::Drive("C:\\".into()),
            8,
            &cancel,
            &progress,
        );
        let tree = match out {
            ScanOutcome::Done(t) => t,
            other => panic!("expected Done, got {other:?}"),
        };
        // link2 dropped: 2 files, logical 1050.
        let (logical, _, files, _) = tree.root_stats();
        assert_eq!(files, 2);
        assert_eq!(logical, 1050);
    }

    #[test]
    fn parent_ids_always_smaller_than_children() {
        let mut fs = TestFs::new();
        fs.add(
            r"\\?\C:",
            vec![("a\\", 0, 0), ("b\\", 0, 0), ("r.txt", 1, 1)],
        );
        fs.add(r"\\?\C:\a", vec![("x\\", 0, 0)]);
        fs.add(r"\\?\C:\a\x", vec![("deep.bin", 2, 2)]);
        fs.add(r"\\?\C:\b", vec![("y.bin", 3, 3)]);
        let platform: Arc<TestFs> = Arc::new(fs);
        let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let progress: Arc<Mutex<Progress>> = Arc::new(Mutex::new(Progress::default()));
        let out = scan(
            platform,
            &ScanTarget::Drive("C:\\".into()),
            9,
            &cancel,
            &progress,
        );
        let tree = match out {
            ScanOutcome::Done(t) => t,
            other => panic!("expected Done, got {other:?}"),
        };
        for id in 1..tree.len() as u32 {
            let p = tree.node(id).unwrap().parent;
            assert!(p < id, "node {id} has parent {p} (parent must be smaller)");
        }
    }
}
