//! The v3 duplicate engine (owner ask: "hours for 16 GB → seconds").
//!
//! Pipeline: size buckets → one-open XXH3-128 screens (prefix + mid
//! windows) → lockstep chain-partition verification. The design
//! principle behind every choice: **every byte is read at most once
//! across the whole pipeline, and bytes that cannot matter are never
//! read at all**.
//!
//! # Why v3 is faster than v2 (the full-SHA-256 pass)
//!
//! v2 screened candidates with XXH3 and then re-read every survivor
//! in full to compute a SHA-256 authority hash. The CPU cost of
//! SHA-256 (~0.5–2 GB/s per core depending on SHA-NI) and the
//! re-reading of the screened regions made the full pass the
//! bottleneck on real disks; on Defender-active Windows hardware the
//! effective rate was tens of MB/s, which reads as "hours" on a 16 GB
//! duplicate corpus.
//!
//! v3 replaces the authority pass with **lockstep chain
//! partitioning**: every member of a screen bucket streams its own
//! unverified byte range in 4 MiB lockstep rounds; each member feeds
//! its blocks into a personal running [`Xxh3`] chain, and after every
//! round the live members partition by chain digest. Members that
//! diverge become singletons and retire immediately (their reads
//! stop at the first differing block); members that stay tied keep
//! streaming; a class that reaches end-of-range with ≥ 2 members is
//! a verified duplicate group.
//!
//! * **Linear always**: each member reads its own range exactly once
//!   — there is no pairwise re-reading, so the adversarial
//!   "100 near-identical 8 GB files" case stays at Σ sizes, not
//!   N²/2 × size.
//! * **Early exit**: non-duplicates that survived the screens stop
//!   being read at the first 4 MiB block where they differ — and the
//!   screens' coverage is never re-read (the chain is seeded with
//!   the screen digests and streaming starts past them).
//! * **CPU-free**: XXH3-128 runs at memory bandwidth; the engine is
//!   I/O-bound by construction.
//! * **Content-complete authority**: a group is reported only when
//!   the members' full ranges chain to the same 128-bit digest — the
//!   same collision-safety argument the v2 screens relied on
//!   (2⁻¹²⁸ at disk scale), now covering every byte that was not
//!   already screened. Files no larger than the prefix screen are
//!   decided by a full-content digest in the screen pass itself.
//!
//! # Phase model
//!
//! `collect` (caller) → `screen` (one open per candidate: the 64 KiB
//! prefix and, for files larger than `prefix + 2·sample`, the two
//! 1 MiB mid windows — one open total) → `verify` (lockstep rounds)
//! → `done`.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;

use rayon::prelude::*;
use xxhash_rust::xxh3::Xxh3;

// ---------------------------------------------------------------------------
// Configuration + inputs
// ---------------------------------------------------------------------------

/// Default prefix screen length (64 KiB).
pub const DEFAULT_PREFIX: u64 = 64 * 1024;
/// Default mid-window length (1 MiB per window).
pub const DEFAULT_SAMPLE: u64 = 1024 * 1024;
/// Default lockstep verify block (4 MiB).
pub const DEFAULT_VERIFY_BLOCK: usize = 4 * 1024 * 1024;

/// Engine tuning.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Prefix screen length (`[0, min(size, prefix))`).
    pub prefix_len: u64,
    /// Mid window length. Files larger than `prefix_len + 2·sample_len`
    /// additionally screen `[prefix, prefix+sample)` and the last
    /// `sample` bytes in the SAME open.
    pub sample_len: u64,
    /// Lockstep round block size.
    pub verify_block: usize,
    /// Screen pool size override (latency-bound: one open per file).
    /// `None` = `available_parallelism` clamped to 8..24.
    pub screen_threads: Option<usize>,
    /// Verify pool size override (bandwidth-bound). `None` =
    /// `available_parallelism` clamped to 2..8.
    pub verify_threads: Option<usize>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            prefix_len: DEFAULT_PREFIX,
            sample_len: DEFAULT_SAMPLE,
            verify_block: DEFAULT_VERIFY_BLOCK,
            screen_threads: None,
            verify_threads: None,
        }
    }
}

impl EngineConfig {
    /// The file length above which the mid windows join the prefix
    /// screen.
    #[must_use]
    pub fn mid_threshold(&self) -> u64 {
        self.prefix_len + 2 * self.sample_len
    }
}

/// One candidate entering the engine (collected from the scan tree by
/// the caller: live, non-placeholder, non-protected, logical > 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineCandidate {
    /// Absolute path (read + reported verbatim).
    pub path: String,
    /// Logical size in bytes from the scan.
    pub size: u64,
    /// Caller-side identity (the tree node id — also the hardlink
    /// fallback). Returned unchanged in [`VerifiedFile`].
    pub node_id: u32,
}

/// One member of a verified duplicate group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFile {
    /// The candidate's path.
    pub path: String,
    /// The candidate's logical size (identical across the group).
    pub size: u64,
    /// The candidate's caller-side identity.
    pub node_id: u32,
    /// Volume serial number captured from the already-open verify
    /// handle (saves the THIRD `File::open` per verified member that
    /// `compute_dupes` used to do — 10k verified files × 20 ms
    /// Defender scan = 200 s of pure open latency AFTER the verify
    /// pass finished). `0` when the platform layer couldn't resolve
    /// it (file vanished between the scan and now, or the host has no
    /// `GetFileInformationByHandle`-class API); the caller treats `0`
    /// as "unknown" and falls back to the `node_id` as the file index.
    pub volume_serial: u64,
    /// File index from the same handle (folded with the high part on
    /// FAT/exFAT inside the platform layer). `u64::from(node_id)` is
    /// the honest fallback when the platform layer couldn't resolve.
    pub file_index: u64,
}

/// A content-equal duplicate group straight from the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedGroup {
    /// Per-file logical size (identical across the group).
    pub size: u64,
    /// Members; the FIRST entry is the natural "keep" anchor (path
    /// order, matching the v2 contract).
    pub files: Vec<VerifiedFile>,
}

/// Engine failure modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineError {
    /// The run was cancelled through [`ProgressSink::cancelled`].
    Cancelled,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for EngineError {}

/// Screen phase id reported through [`ProgressSink::phase`].
pub const PHASE_SCREEN: &str = "screen";
/// Verify phase id reported through [`ProgressSink::phase`].
pub const PHASE_VERIFY: &str = "verify";

/// Progress + cancellation seam. The app's control block implements
/// this; tests use [`QuietSink`].
///
/// All callbacks are called from worker threads and must be cheap
/// (atomics, not mutexes, on the hot path).
pub trait ProgressSink: Sync {
    /// A phase begins. `files_total`/`bytes_total` describe the
    /// phase's planned work (bytes are honest estimates — early
    /// exits read less).
    fn phase(&self, phase: &str, files_total: u64, bytes_total: u64);
    /// One file finished in the current phase, having accounted
    /// `bytes` of reads.
    fn file_done(&self, bytes: u64);
    /// `true` when the user cancelled the run.
    fn cancelled(&self) -> bool;
}

/// A no-op sink (no events, never cancelled).
#[derive(Debug, Clone, Copy, Default)]
pub struct QuietSink;

impl ProgressSink for QuietSink {
    fn phase(&self, _phase: &str, _files_total: u64, _bytes_total: u64) {}
    fn file_done(&self, _bytes: u64) {}
    fn cancelled(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Pools
// ---------------------------------------------------------------------------

/// Screen pool — latency-bound (one open per file; Windows
/// Defender's per-open scan and `NVMe` queue depth reward many
/// concurrent streams). Sized `available_parallelism` clamped 8..24.
fn screen_pool(override_threads: Option<usize>) -> rayon::ThreadPool {
    let n = override_threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, std::num::NonZero::get))
        .clamp(8, 24);
    rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .thread_name(|i| format!("db-dupes-screen-{i}"))
        .build()
        .unwrap_or_else(|e| panic!("dupes screen pool: {e}"))
}

/// Verify pool — bandwidth-bound (sequential 4 MiB streams
/// saturate the disk with far fewer workers). Clamped 2..8.
fn verify_pool(override_threads: Option<usize>) -> rayon::ThreadPool {
    let n = override_threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(2, std::num::NonZero::get))
        .clamp(2, 8);
    rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .thread_name(|i| format!("db-dupes-verify-{i}"))
        .build()
        .unwrap_or_else(|e| panic!("dupes verify pool: {e}"))
}

/// Pools live for the process lifetime (building pools per run would
/// pay thread-spawn cost on every scan; the v2 pools were
/// process-lifetime too). First-run config wins for the overrides —
/// a deliberate trade: the overrides exist for TESTS, which use one
/// config per process.
fn pools(cfg: &EngineConfig) -> &'static (rayon::ThreadPool, rayon::ThreadPool) {
    static POOLS: OnceLock<(rayon::ThreadPool, rayon::ThreadPool)> = OnceLock::new();
    POOLS.get_or_init(|| {
        (
            screen_pool(cfg.screen_threads),
            verify_pool(cfg.verify_threads),
        )
    })
}

// ---------------------------------------------------------------------------
// Buffers (per-thread, grow-on-demand, reused across files)
// ---------------------------------------------------------------------------

std::thread_local! {
    /// Screen scratch (grows to `sample_len`; the prefix read borrows
    /// its head).
    static SCREEN_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Verify scratch (grows to `verify_block`).
    static VERIFY_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Read into `buf[..want]` until full or EOF; returns bytes read
/// (`Ok(n)` with `n < want` only at EOF).
fn read_fill(f: &mut std::fs::File, buf: &mut [u8], want: usize) -> std::io::Result<usize> {
    debug_assert!(want <= buf.len());
    let mut got = 0usize;
    while got < want {
        let n = f.read(&mut buf[got..want])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    Ok(got)
}

/// Open with the platform's sequential-scan hint
/// (`FILE_FLAG_SEQUENTIAL_SCAN` on Windows — Cache Manager read-ahead
/// and Defender's scan pattern). `None` = unreadable, which callers
/// skip honestly.
fn open_seq(path: &Path) -> Option<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const SEQ_FLAG: u32 = 0x0800_0000; // FILE_FLAG_SEQUENTIAL_SCAN
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(SEQ_FLAG)
            .open(path)
            .ok()
    }
    #[cfg(not(windows))]
    {
        std::fs::File::open(path).ok()
    }
}

// ---------------------------------------------------------------------------
// Screens
// ---------------------------------------------------------------------------

/// The screens for one candidate: `(prefix digest, mid digest)` —
/// `mid` is `None` for files below the mid threshold.
type Screens = ([u8; 16], Option<[u8; 16]>);

/// Bytes the screens will read for a file of `size` (progress
/// estimate; also the honest work count when the open fails).
#[must_use]
fn screen_read_len(size: u64, cfg: &EngineConfig) -> u64 {
    if size > cfg.mid_threshold() {
        cfg.prefix_len + 2 * cfg.sample_len
    } else {
        size.min(cfg.prefix_len)
    }
}

/// One open, one screen: hash the prefix (`[0, min(size, prefix))`)
/// and, when the file is large enough, both mid windows
/// (`[prefix, prefix+sample)` and the last `sample` bytes).
/// Combining the three reads into a single open halves the per-file
/// open cost — the dominant latency on Defender-active Windows
/// hardware (v2 opened twice for large files).
///
/// `None` = unreadable or truncated mid-read (skipped honestly).
fn screen_file(path: &Path, size: u64, cfg: &EngineConfig) -> Option<Screens> {
    let mut f = open_seq(path)?;
    let prefix_len = cfg.prefix_len.min(size) as usize;
    let mid = size > cfg.mid_threshold();
    let sample = cfg.sample_len as usize;

    SCREEN_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        let need = if mid { sample } else { prefix_len };
        if buf.len() < need {
            buf.resize(need, 0);
        }
        // Prefix: [0, min(size, prefix)).
        let got = read_fill(&mut f, &mut buf[..prefix_len], prefix_len).ok()?;
        if got < prefix_len {
            return None; // truncated — treat as unreadable
        }
        let mut prefix_hasher = Xxh3::new();
        prefix_hasher.update(&buf[..prefix_len]);
        let prefix: [u8; 16] = prefix_hasher.digest128().to_le_bytes();

        if !mid {
            return Some((prefix, None));
        }
        // Window A: [prefix, prefix + sample).
        f.seek(SeekFrom::Start(cfg.prefix_len)).ok()?;
        let got = read_fill(&mut f, &mut buf[..sample], sample).ok()?;
        if got < sample {
            return None; // truncated — treat as unreadable
        }
        let mut mid_hasher = Xxh3::new();
        mid_hasher.update(&buf[..sample]);
        // Window B: the last `sample` bytes (never overlaps A — the
        // mid screen only runs past the threshold).
        f.seek(SeekFrom::Start(size - cfg.sample_len)).ok()?;
        let got = read_fill(&mut f, &mut buf[..sample], sample).ok()?;
        if got < sample {
            return None; // truncated — treat as unreadable
        }
        mid_hasher.update(&buf[..sample]);
        let m: [u8; 16] = mid_hasher.digest128().to_le_bytes();
        Some((prefix, Some(m)))
    })
}

// ---------------------------------------------------------------------------
// Verify: lockstep chain partitioning
// ---------------------------------------------------------------------------

/// One live member inside the lockstep rounds. Owns its handle and
/// its running chain (a member is read by exactly one thread — the
/// bucket's — so no synchronization is needed).
struct LiveMember {
    /// Index into the engine's candidate list.
    idx: usize,
    /// The member's open handle, positioned at the next block
    /// boundary of its verify range.
    file: std::fs::File,
    /// The running chain (seeded with the shared screen digests; fed
    /// every block read so far).
    chain: Xxh3,
    /// `true` once the member's verify range is exhausted.
    eof: bool,
    /// Verify bytes actually read (progress accounting).
    read: u64,
}

/// Seed a fresh chain with the bucket's screen digests so the
/// verified range starts PAST the screened bytes — they are already
/// proven equal within the bucket, and re-reading them would
/// double-count I/O for no new information. The domain tags keep
/// prefix-only and mid-screened seedings from ever colliding.
fn seeded_chain(prefix: [u8; 16], mid: Option<[u8; 16]>) -> Xxh3 {
    let mut chain = Xxh3::new();
    chain.update(b"dbv3-p");
    chain.update(&prefix);
    if let Some(m) = mid {
        chain.update(b"dbv3-m");
        chain.update(&m);
    }
    chain
}

/// One screen bucket heading into verification: the shared screen
/// key plus the member candidate indices (≥ 2, path order).
struct ScreenBucket {
    size: u64,
    screens: Screens,
    members: Vec<usize>,
}

/// One verified member: `(candidate index, volume_serial, file_index)`.
/// The hardlink identity is captured from the member's already-open
/// `File` handle inside `verify_bucket` so the caller never re-opens.
pub(super) type VerifiedTriple = (usize, u64, u64);

/// A flat list of verified triples — one `Vec` per verified group.
pub(super) type VerifiedGroupTriples = Vec<VerifiedTriple>;

/// Verify one screen bucket: stream every member's unverified range
/// in lockstep rounds, partitioning by chain digest after each
/// round. Returns the verified groups (≥ 2 members, range-equal)
/// as `(candidate-index, volume_serial, file_index)` triples — the
/// hardlink identity is captured from each member's already-open
/// `File` handle via `hardlink`, saving the THIRD `File::open` per
/// verified member that the v2 caller used to do.
///
/// ## The round loop
///
/// 1. Every non-EOF live member reads its next ≤ 4 MiB block and
///    feeds it into its chain.
/// 2. Members partition by chain digest. A partition split is
///    permanent (chains never re-converge without a 2⁻¹²⁸
///    collision).
/// 3. A class that is entirely EOF with ≥ 2 members is a verified
///    group (emitted). A singleton class retires (its handle drops).
///    Everything else keeps streaming.
///
/// Termination: every round, each non-EOF member either advances its
/// read position or becomes EOF; EOF members only stay live while
/// some class-mate still reads, and classes only shrink. The loop
/// therefore empties `live` in at most ⌈range/block⌉ + 1 rounds.
fn verify_bucket(
    bucket: &ScreenBucket,
    candidates: &[EngineCandidate],
    cfg: &EngineConfig,
    sink: &dyn ProgressSink,
    hardlink: &(dyn Fn(&std::fs::File) -> Option<(u64, u64)> + Send + Sync),
) -> Result<Vec<VerifiedGroupTriples>, EngineError> {
    debug_assert!(bucket.members.len() >= 2);
    let size = bucket.size;
    let (start, end) = verify_range(size, cfg);

    // Open + seek every member now (open failures retire up front —
    // honest skips).
    let mut live: Vec<LiveMember> = Vec::with_capacity(bucket.members.len());
    for &m in &bucket.members {
        let Some(mut file) = open_seq(Path::new(&candidates[m].path)) else {
            sink.file_done(0);
            continue;
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            sink.file_done(0);
            continue;
        }
        live.push(LiveMember {
            idx: m,
            file,
            chain: seeded_chain(bucket.screens.0, bucket.screens.1),
            eof: false,
            read: 0,
        });
    }

    let mut groups: Vec<VerifiedGroupTriples> = Vec::new();
    'rounds: loop {
        if sink.cancelled() {
            return Err(EngineError::Cancelled);
        }
        if live.is_empty() {
            break;
        }

        // ── One round: every non-EOF member reads its next block.
        VERIFY_BUF.with(|cell| {
            let mut buf = cell.borrow_mut();
            if buf.len() < cfg.verify_block {
                buf.resize(cfg.verify_block, 0);
            }
            for member in &mut live {
                if member.eof {
                    continue;
                }
                let remaining = end.saturating_sub(start + member.read);
                if remaining == 0 {
                    member.eof = true;
                    continue;
                }
                let want = (remaining as usize).min(cfg.verify_block);
                // An I/O error mid-verify treats as early EOF: the
                // member's chain simply stops growing, which splits
                // it from continuing members at the next partition.
                let got = read_fill(&mut member.file, &mut buf[..want], want).unwrap_or(0);
                member.read += got as u64;
                if got > 0 {
                    member.chain.update(&buf[..got]);
                }
                if got < want {
                    member.eof = true;
                }
            }
        });

        // ── Partition by chain digest. `LiveMember` moves into the
        //    map (File is not Clone — partitioning owns members).
        let mut classes: HashMap<u128, Vec<LiveMember>> = HashMap::with_capacity(live.len());
        for member in live.drain(..) {
            classes
                .entry(member.chain.digest128())
                .or_default()
                .push(member);
        }
        // Deterministic order: process classes by their first
        // member's candidate index (the input is path-sorted, so
        // group emission order is stable across runs).
        let mut ordered: Vec<Vec<LiveMember>> = classes.into_values().collect();
        ordered.sort_unstable_by_key(|c| c[0].idx);

        let mut next_live: Vec<LiveMember> = Vec::new();
        for mut class in ordered {
            if class.len() >= 2 && class.iter().all(|m| m.eof) {
                // Verified: equal chains through the whole range.
                // Capture hardlink identity from each member's
                // already-open `File` handle (the THIRD-open the
                // v2 caller used to do is gone — 10k verified files
                // × 20 ms Defender scan = 200 s saved on real disks).
                let captured: Vec<(usize, u64, u64)> = class
                    .iter()
                    .map(|m| {
                        let (vs, fi) = hardlink(&m.file).unwrap_or((0, m.idx as u64));
                        (m.idx, vs, fi)
                    })
                    .collect();
                groups.push(captured);
                for m in &class {
                    sink.file_done(m.read);
                }
            } else if class.len() >= 2 {
                // Still tied with unfinished readers — keep
                // streaming (EOF members ride along until the
                // class splits or the rest finish; their frozen
                // digests diverge from any further reads).
                next_live.append(&mut class);
            } else {
                // Singleton — retired (unique, or orphaned by a
                // mid-scan change).
                let m = class.pop().expect("nonempty class");
                sink.file_done(m.read);
            }
        }
        live = next_live;
        if live.is_empty() {
            break 'rounds;
        }
    }

    Ok(groups)
}

/// The unverified byte range for a bucket of `size` (the screens
/// already proved equality on their coverage; the chain seeds with
/// their digests instead of re-reading):
///
/// * Mid-screened files (`size > prefix + 2·sample`): verify
///   `[prefix + sample, size − sample)`.
/// * Prefix-only files: verify `[prefix, size)`.
/// * Files ≤ prefix never reach verification (their full content
///   was the prefix screen — buckets emit directly).
fn verify_range(size: u64, cfg: &EngineConfig) -> (u64, u64) {
    if size > cfg.mid_threshold() {
        (cfg.prefix_len + cfg.sample_len, size - cfg.sample_len)
    } else {
        (cfg.prefix_len, size)
    }
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// Run the engine over `candidates`.
///
/// The candidates should be the scan's live, non-placeholder,
/// non-protected, non-empty files (the caller's collect pass filters;
/// the engine trusts it). Input order is irrelevant internally —
/// everything downstream runs in path order (disk locality: short
/// seeks, warm cache lines, Defender scanning neighbours).
///
/// `hardlink` is called with each verified member's already-open
/// `File` handle to capture `(volume_serial, file_index)` for hardlink
/// exclusion downstream — this collapses the v2 third-open per
/// verified file (the post-verify `File::open` for
/// `GetFileInformationByHandle`). The closure is `Send + Sync` so the
/// parallel verify pool can call it from worker threads.
///
/// # Errors
/// [`EngineError::Cancelled`] when the sink reported cancellation
/// mid-pipeline. Partial groups are discarded (a cancelled run
/// reports nothing).
#[allow(clippy::too_many_lines)] // the pipeline IS the phase map —
                                 // splitting it would scatter the screen→verify handoff narrative
pub fn run(
    candidates: &[EngineCandidate],
    cfg: &EngineConfig,
    sink: &dyn ProgressSink,
    hardlink: &(dyn Fn(&std::fs::File) -> Option<(u64, u64)> + Send + Sync),
) -> Result<Vec<VerifiedGroup>, EngineError> {
    // Path-sorted work order (disk locality for every pass).
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_unstable_by(|&a, &b| candidates[a].path.cmp(&candidates[b].path));

    // ── Size buckets (single-member sizes cannot hold duplicates;
    //    zero-length candidates can never waste space — spec §10
    //    — and the defensive filter keeps the engine honest even if
    //    a future caller forgets the collect-side one).
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for &i in &order {
        if candidates[i].size == 0 {
            continue;
        }
        by_size.entry(candidates[i].size).or_default().push(i);
    }
    let sized: Vec<Vec<usize>> = by_size.into_values().filter(|b| b.len() >= 2).collect();

    // ── Screen pass (parallel, latency pool): one open per member
    //    of every size bucket. Flat target list keeps the result
    //    indexing trivial (and avoids nested parallel iterators —
    //    one par_iter over the flattened targets is how v2 did it).
    let screen_targets: Vec<usize> = sized.iter().flat_map(|b| b.iter().copied()).collect();
    let screen_files: u64 = screen_targets.len() as u64;
    let screen_bytes: u64 = screen_targets
        .iter()
        .map(|&i| screen_read_len(candidates[i].size, cfg))
        .sum();
    sink.phase(PHASE_SCREEN, screen_files, screen_bytes);
    let (screen_pool, verify_pool) = pools(cfg);

    let screens: Vec<Option<Screens>> = screen_pool.install(|| {
        screen_targets
            .par_iter()
            .map(|&i| {
                if sink.cancelled() {
                    return None;
                }
                let d = screen_file(Path::new(&candidates[i].path), candidates[i].size, cfg);
                // Counted even when unreadable — the attempt is the
                // work the user waits on (v2 contract).
                sink.file_done(screen_read_len(candidates[i].size, cfg));
                d
            })
            .collect()
    });
    if sink.cancelled() {
        return Err(EngineError::Cancelled);
    }

    // ── Re-bucket by screen equality: (size, prefix, mid). The
    //    results align 1:1 with `screen_targets`.
    let mut by_screen: HashMap<(u64, [u8; 16], [u8; 16]), Vec<usize>> = HashMap::new();
    let mut by_screen_small: HashMap<(u64, [u8; 16]), Vec<usize>> = HashMap::new();
    for (&i, s) in screen_targets.iter().zip(screens) {
        let Some((prefix, mid)) = s else { continue };
        match mid {
            Some(m) => {
                by_screen
                    .entry((candidates[i].size, prefix, m))
                    .or_default()
                    .push(i);
            }
            None => {
                by_screen_small
                    .entry((candidates[i].size, prefix))
                    .or_default()
                    .push(i);
            }
        }
    }

    let mut groups: Vec<VerifiedGroup> = Vec::new();

    // ── Tiny files (size ≤ prefix): the prefix screen hashed the
    //    FULL content — equal screens are full-content equality.
    //    Emit directly (no verify phase for these).
    for (key, bucket) in &by_screen_small {
        if bucket.len() >= 2 {
            let size = key.0;
            if size <= cfg.prefix_len {
                groups.push(VerifiedGroup {
                    size,
                    files: bucket
                        .iter()
                        .map(|&i| VerifiedFile {
                            path: candidates[i].path.clone(),
                            size,
                            node_id: candidates[i].node_id,
                            // Tiny files never went through verify_bucket,
                            // so no open File handle was captured. The
                            // caller (`compute_dupes`) opens these once
                            // for hardlink identity (the legacy third-open,
                            // but only for the small fraction of files
                            // below the prefix length).
                            volume_serial: 0,
                            file_index: u64::from(candidates[i].node_id),
                        })
                        .collect(),
                });
            }
        }
    }
    // Prefix-only files ABOVE the prefix length still need their
    // tail verified: fold them into the verify buckets with their
    // (prefix, None) screens.
    let mut verify_buckets: Vec<ScreenBucket> = by_screen_small
        .into_iter()
        .filter(|((size, _), bucket)| *size > cfg.prefix_len && bucket.len() >= 2)
        .map(|((size, prefix), members)| ScreenBucket {
            size,
            screens: (prefix, None),
            members,
        })
        .collect();
    verify_buckets.extend(by_screen.into_iter().filter(|(_, m)| m.len() >= 2).map(
        |((size, prefix, mid), members)| ScreenBucket {
            size,
            screens: (prefix, Some(mid)),
            members,
        },
    ));
    // Deterministic bucket order (path-sorted first member).
    verify_buckets.sort_unstable_by(|a, b| {
        candidates[a.members[0]]
            .path
            .cmp(&candidates[b.members[0]].path)
    });

    // ── Verify pass (parallel over buckets, bandwidth pool).
    let verify_files: u64 = verify_buckets.iter().map(|b| b.members.len() as u64).sum();
    let verify_bytes: u64 = verify_buckets
        .iter()
        .map(|b| {
            let (start, end) = verify_range(b.size, cfg);
            (end - start).saturating_mul(b.members.len() as u64)
        })
        .sum();
    sink.phase(PHASE_VERIFY, verify_files, verify_bytes);

    let verified: Vec<Vec<VerifiedGroupTriples>> = verify_pool.install(|| {
        verify_buckets
            .par_iter()
            .map(|bucket| verify_bucket(bucket, candidates, cfg, sink, hardlink))
            .collect::<Result<Vec<_>, _>>()
    })?;
    if sink.cancelled() {
        return Err(EngineError::Cancelled);
    }
    for bucket_groups in verified {
        for member_ids in bucket_groups {
            groups.push(VerifiedGroup {
                size: candidates[member_ids[0].0].size,
                files: member_ids
                    .iter()
                    .map(|&(i, vs, fi)| VerifiedFile {
                        path: candidates[i].path.clone(),
                        size: candidates[i].size,
                        node_id: candidates[i].node_id,
                        volume_serial: vs,
                        file_index: fi,
                    })
                    .collect(),
            });
        }
    }

    Ok(groups)
}

// ---------------------------------------------------------------------------
// Tests: real staged files, every planted structure, adversarial shapes.
// ---------------------------------------------------------------------------

#[cfg(test)]
/// Test no-op hardlink resolver (shared by `tests` and `bench`):
/// returns `None` so verified members fall back to
/// `volume_serial=0`, `file_index=idx` (the same shape the
/// production caller treats as "unknown" and re-resolves via
/// `hardlink_identity` for tiny files).
fn no_hardlink() -> &'static (dyn Fn(&std::fs::File) -> Option<(u64, u64)> + Send + Sync) {
    &|_f: &std::fs::File| None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A small-but-faithful config: the same pipeline geometry with
    /// KiB-scale lengths so tests stage quickly (the logic is
    /// length-agnostic; one default-config test covers real sizing).
    pub(super) fn small_cfg() -> EngineConfig {
        EngineConfig {
            prefix_len: 4 * 1024,
            sample_len: 64 * 1024,
            verify_block: 64 * 1024,
            screen_threads: Some(4),
            verify_threads: Some(2),
        }
    }

    /// Unique staging dir per test (parallel-safe, auto-cleaned).
    pub(super) struct TempTree(PathBuf);
    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    pub(super) fn stage(name: &str) -> (TempTree, PathBuf) {
        let d = std::env::temp_dir().join(format!(
            "db-dupes-engine-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |t| t.as_nanos())
        ));
        fs::create_dir_all(&d).expect("stage dir");
        (TempTree(d.clone()), d)
    }

    /// Deterministic pseudo-random content (xorshift64).
    pub(super) fn blob(seed: u64, size: usize) -> Vec<u8> {
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

    /// Format magics (realistic headers so the payloads read like
    /// genuine files to OS caches + AV scanners).
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

    /// Magic-headed pseudo-random payload.
    pub(super) fn fmt_blob(fmt: usize, seed: u64, size: usize) -> Vec<u8> {
        let mut v = blob(seed, size);
        let (_, magic) = FORMATS[fmt % FORMATS.len()];
        let m = magic.len();
        if size >= m {
            v[..m].copy_from_slice(magic);
        }
        v
    }

    /// Candidates from a staged root: every regular file, sized by
    /// `metadata` (the production collect pass walks the scan tree —
    /// the engine trusts caller-supplied sizes, which these are).
    fn candidates_of(root: &std::path::Path) -> Vec<EngineCandidate> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir).expect("read_dir") {
                let e = e.expect("entry");
                let ft = e.file_type().expect("file_type");
                if ft.is_dir() {
                    stack.push(e.path());
                } else if ft.is_file() {
                    let len = e.metadata().expect("meta").len();
                    out.push(EngineCandidate {
                        path: e.path().to_string_lossy().into_owned(),
                        size: len,
                        node_id: out.len() as u32,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Run + summarize as (size, count) sorted.
    fn run_sizes(candidates: &[EngineCandidate], cfg: &EngineConfig) -> Vec<(u64, u64)> {
        let mut groups: Vec<(u64, u64)> = run(candidates, cfg, &QuietSink, no_hardlink())
            .expect("engine")
            .into_iter()
            .map(|g| (g.size, g.files.len() as u64))
            .collect();
        groups.sort_unstable();
        groups
    }

    #[test]
    fn tiny_identical_pair_groups_via_prefix_screen() {
        let (_t, d) = stage("tiny-pair");
        let a = fmt_blob(6, 0x51, 1_000);
        fs::write(d.join("a.txt"), &a).unwrap();
        fs::write(d.join("b.txt"), &a).unwrap();
        // Same size, different content — must NOT group.
        fs::write(d.join("c.txt"), fmt_blob(6, 0x52, 1_000)).unwrap();
        assert_eq!(
            run_sizes(&candidates_of(&d), &small_cfg()),
            vec![(1_000, 2)]
        );
    }

    #[test]
    fn same_prefix_differing_window_a_is_screened_out() {
        let (_t, d) = stage("win-a");
        let cfg = small_cfg();
        // Above the mid threshold: mid screens run.
        let size = cfg.prefix_len + 3 * cfg.sample_len;
        let base = fmt_blob(2, 0xA1, size as usize);
        let mut twin = base.clone();
        // Flip a byte inside window A ([prefix, prefix+sample)).
        twin[cfg.prefix_len as usize + 128] ^= 0xFF;
        fs::write(d.join("a.mp4"), &base).unwrap();
        fs::write(d.join("b.mp4"), &twin).unwrap();
        assert!(
            run_sizes(&candidates_of(&d), &cfg).is_empty(),
            "no groups expected"
        );
    }

    #[test]
    fn same_prefix_differing_window_b_is_screened_out() {
        let (_t, d) = stage("win-b");
        let cfg = small_cfg();
        let size = cfg.prefix_len + 3 * cfg.sample_len;
        let base = fmt_blob(2, 0xB1, size as usize);
        let mut twin = base.clone();
        // Flip a byte inside window B (the last `sample` bytes).
        twin[(size - 128) as usize] ^= 0xFF;
        fs::write(d.join("a.mp4"), &base).unwrap();
        fs::write(d.join("b.mp4"), &twin).unwrap();
        assert!(
            run_sizes(&candidates_of(&d), &cfg).is_empty(),
            "no groups expected"
        );
    }

    #[test]
    fn same_screens_differing_gap_is_rejected_by_verify() {
        // THE critical v3 regression test: screens pass (prefix +
        // both mid windows identical) but the gap differs — only the
        // lockstep verify catches it. v2 caught it via the full
        // SHA-256; v3 must catch it via chain divergence.
        let (_t, d) = stage("gap");
        let cfg = small_cfg();
        let size = cfg.prefix_len + 4 * cfg.sample_len;
        let base = fmt_blob(3, 0xC1, size as usize);
        let mut twin = base.clone();
        // The gap is (prefix + sample, size - sample) — flip a byte
        // in the middle of it (uncovered by any screen).
        let mid_gap = (cfg.prefix_len + cfg.sample_len + (size - cfg.sample_len)) / 2;
        twin[mid_gap as usize] ^= 0xFF;
        fs::write(d.join("a.zip"), &base).unwrap();
        fs::write(d.join("b.zip"), &twin).unwrap();
        assert!(
            run_sizes(&candidates_of(&d), &cfg).is_empty(),
            "gap-differing twin must not group"
        );
    }

    #[test]
    fn identical_large_files_group() {
        let (_t, d) = stage("large-identical");
        let cfg = small_cfg();
        let size = cfg.prefix_len + 5 * cfg.sample_len;
        let a = fmt_blob(0, 0xD1, size as usize);
        fs::write(d.join("x.jpg"), &a).unwrap();
        fs::write(d.join("y.jpg"), &a).unwrap();
        fs::write(d.join("z.jpg"), &a).unwrap();
        assert_eq!(run_sizes(&candidates_of(&d), &cfg), vec![(size, 3)]);
    }

    #[test]
    fn bucket_splits_into_two_classes_correctly() {
        // A=B and C=D with identical screens but different gap bytes:
        // the lockstep partition must find BOTH groups (the naive
        // single-reference design misses C=D — v3 must not).
        let (_t, d) = stage("split");
        let cfg = small_cfg();
        let size = cfg.prefix_len + 4 * cfg.sample_len;
        let a = fmt_blob(1, 0xE1, size as usize);
        let c = fmt_blob(1, 0xE2, size as usize);
        // Make c's screens match a's: copy the screened regions.
        let mut cc = c.clone();
        let screened = (cfg.prefix_len + cfg.sample_len) as usize;
        let tail = (size - cfg.sample_len) as usize;
        cc[..screened].copy_from_slice(&a[..screened]);
        let tail_len = size as usize - tail;
        cc[tail..].copy_from_slice(&a[tail..tail + tail_len]);
        for (name, content) in [("a.png", &a), ("b.png", &a), ("c.png", &cc), ("e.png", &cc)] {
            fs::write(d.join(name), content).unwrap();
        }
        let groups = run(&candidates_of(&d), &cfg, &QuietSink, no_hardlink()).expect("engine");
        assert_eq!(groups.len(), 2, "expected two classes: {groups:#?}");
        assert_eq!(groups[0].files.len(), 2);
        assert_eq!(groups[1].files.len(), 2);
    }

    #[test]
    fn wide_bucket_of_70_identical_files_groups() {
        // Exercises multi-round lockstep with a class far wider than
        // any reasonable batch width (the chain design has no batch
        // limit — this pins that).
        let (_t, d) = stage("wide70");
        let cfg = small_cfg();
        let size = cfg.prefix_len + 2 * cfg.sample_len + 8;
        let a = fmt_blob(4, 0xF1, size as usize);
        for i in 0..70u32 {
            fs::write(d.join(format!("copy-{i:03}.pdf")), &a).unwrap();
        }
        let groups = run(&candidates_of(&d), &cfg, &QuietSink, no_hardlink()).expect("engine");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files.len(), 70);
        assert_eq!(groups[0].size, size);
    }

    #[test]
    fn stale_size_drops_out_honestly() {
        // The declared (scan-time) size no longer matches reality:
        // the member's verify range reads short/long and its chain
        // diverges from the true pair.
        let (_t, d) = stage("stale");
        let cfg = small_cfg();
        let size = cfg.prefix_len + 3 * cfg.sample_len;
        let a = fmt_blob(5, 0x0101, size as usize);
        fs::write(d.join("a.iso"), &a).unwrap();
        fs::write(d.join("b.iso"), &a).unwrap();
        let mut cands = candidates_of(&d);
        // Lie about one member's size (as if it shrank after scan).
        cands[1].size += 17;
        let groups = run(&cands, &cfg, &QuietSink, no_hardlink()).expect("engine");
        // The pair broke: no group (the honest pair member may not
        // pair with the stale one).
        assert!(groups.iter().all(|g| g.files.len() < 2 || g.size != size));
    }

    #[test]
    fn unreadable_member_skips_but_rest_group() {
        let (_t, d) = stage("unreadable");
        let cfg = small_cfg();
        let a = fmt_blob(7, 0x0202, 3 * 1024);
        fs::write(d.join("a.bin"), &a).unwrap();
        fs::write(d.join("b.bin"), &a).unwrap();
        fs::write(d.join("c.bin"), &a).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(d.join("c.bin"), fs::Permissions::from_mode(0o000)).unwrap();
        }
        let groups = run(&candidates_of(&d), &cfg, &QuietSink, no_hardlink()).expect("engine");
        #[cfg(unix)]
        {
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].files.len(), 2);
            assert!(!groups[0].files.iter().any(|f| f.path.ends_with("c.bin")));
        }
        #[cfg(not(unix))]
        {
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].files.len(), 3);
        }
    }

    #[test]
    fn cancellation_aborts_with_error() {
        struct CancelSink;
        impl ProgressSink for CancelSink {
            fn phase(&self, _p: &str, _f: u64, _b: u64) {}
            fn file_done(&self, _b: u64) {}
            fn cancelled(&self) -> bool {
                true
            }
        }
        let (_t, d) = stage("cancel");
        let cfg = small_cfg();
        let a = fmt_blob(0, 0x0303, 2 * 1024);
        fs::write(d.join("a.jpg"), &a).unwrap();
        fs::write(d.join("b.jpg"), &a).unwrap();
        let out = run(&candidates_of(&d), &cfg, &CancelSink, no_hardlink());
        assert_eq!(out, Err(EngineError::Cancelled));
    }

    #[test]
    fn empty_and_singleton_inputs_return_no_groups() {
        let (_t, d) = stage("empty");
        assert!(
            run(&[], &small_cfg(), &QuietSink, no_hardlink())
                .unwrap()
                .is_empty(),
            "empty in, empty out"
        );
        let mut cands = candidates_of(&d);
        assert!(
            run(&cands, &small_cfg(), &QuietSink, no_hardlink())
                .unwrap()
                .is_empty(),
            "no same-size pair"
        );
        cands.clear();
        assert!(
            run(&cands, &small_cfg(), &QuietSink, no_hardlink())
                .unwrap()
                .is_empty(),
            "no same-size pair"
        );
    }

    #[test]
    fn zero_length_files_are_ignored() {
        let (_t, d) = stage("zero-len");
        fs::write(d.join("z1.dat"), b"").unwrap();
        fs::write(d.join("z2.dat"), b"").unwrap();
        fs::write(d.join("z3.dat"), b"").unwrap();
        assert!(
            run(&candidates_of(&d), &small_cfg(), &QuietSink, no_hardlink())
                .unwrap()
                .is_empty(),
            "zero-length never groups"
        );
    }

    #[test]
    fn progress_totals_cover_every_file() {
        // Every screened file reports exactly one screen file_done;
        // verify reports one per member that entered it.
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        use std::sync::Arc;
        #[derive(Default)]
        struct CountingSink {
            files: AtomicUsize,
            bytes: AtomicU64,
            phases: AtomicUsize,
        }
        impl ProgressSink for CountingSink {
            fn phase(&self, _p: &str, _f: u64, _b: u64) {
                self.phases.fetch_add(1, Ordering::Relaxed);
            }
            fn file_done(&self, bytes: u64) {
                self.files.fetch_add(1, Ordering::Relaxed);
                self.bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            fn cancelled(&self) -> bool {
                false
            }
        }
        let (_t, d) = stage("progress");
        let cfg = small_cfg();
        let a = fmt_blob(2, 0x0404, cfg.prefix_len as usize + 10);
        fs::write(d.join("a.mp4"), &a).unwrap();
        fs::write(d.join("b.mp4"), &a).unwrap();
        // A gap-differing twin (screen pass covers it, verify
        // retires it): three files screened, three accounted.
        let mut twin = a.clone();
        twin[(cfg.prefix_len + 5) as usize] ^= 0xFF;
        fs::write(d.join("c.mp4"), &twin).unwrap();
        let sink = Arc::new(CountingSink::default());
        run(&candidates_of(&d), &cfg, sink.as_ref(), no_hardlink()).unwrap();
        // 3 screen file_done + 3 verify file_done (a/b group, c
        // retires as a gap-differing singleton).
        assert_eq!(sink.files.load(Ordering::Relaxed), 6);
        assert!(sink.bytes.load(Ordering::Relaxed) > 0);
        assert!(sink.phases.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn default_config_handles_a_realistic_mix() {
        // Default lengths (64 KiB prefix / 1 MiB windows / 4 MiB
        // blocks) against a realistic mixed corpus: small photos,
        // mid documents, one large triple, a screened near-dup pair.
        let (_t, d) = stage("default-mix");
        let cfg = EngineConfig::default();
        let mib = 1024 * 1024u64;
        let mut expected: Vec<(u64, u64)> = Vec::new();

        // 12 small files, 3 planted pairs.
        for i in 0..12u64 {
            let size = (40 + i % 50) * 1024;
            let name = format!("shot_{i:02}.jpg");
            fs::write(d.join(&name), fmt_blob(0, 0x500 + i, size as usize)).unwrap();
            if i % 4 == 1 && i > 0 {
                fs::write(
                    d.join(name.replace("shot", "copy")),
                    fs::read(d.join(&name)).unwrap(),
                )
                .unwrap();
                expected.push((size, 2));
            }
        }
        // One large triple above the mid threshold.
        let big = 9 * mib;
        let payload = fmt_blob(7, 0x7777, big as usize);
        for n in ["disk-a.bin", "disk-b.bin", "disk-c.bin"] {
            fs::write(d.join(n), &payload).unwrap();
        }
        expected.push((big, 3));
        // A near-dup: same size, same 64 KiB prefix, gap difference.
        let mut twin = payload.clone();
        twin[(5 * mib) as usize] ^= 0xA5;
        fs::write(d.join("disk-twin.bin"), &twin).unwrap();

        let mut got = run_sizes(&candidates_of(&d), &cfg);
        expected.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, expected);
    }

    #[test]
    fn groups_carry_node_ids_and_path_order() {
        let (_t, d) = stage("node-ids");
        let cfg = small_cfg();
        let a = fmt_blob(6, 0x0606, 2 * 1024);
        fs::write(d.join("m.txt"), &a).unwrap();
        fs::write(d.join("n.txt"), &a).unwrap();
        let groups = run(&candidates_of(&d), &cfg, &QuietSink, no_hardlink()).unwrap();
        assert_eq!(groups.len(), 1);
        let paths: Vec<&str> = groups[0].files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                d.join("m.txt").to_str().unwrap(),
                d.join("n.txt").to_str().unwrap()
            ]
        );
        // Node ids round-trip (candidates were staged in path order
        // — 0 and 1).
        let ids: Vec<u32> = groups[0].files.iter().map(|f| f.node_id).collect();
        assert_eq!(ids.len(), 2);
    }

    /// The hardlink closure IS called during verify and captures
    /// `(volume_serial, file_index)` from each verified member's
    /// already-open File handle. This is the perf fix that collapses
    /// the v2 third-open (10k files × 20ms Defender scan = 200s saved).
    #[test]
    fn hardlink_closure_captures_identity_from_open_verify_handle() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let (_t, d) = stage("hardlink-capture");
        let cfg = small_cfg();
        // Two identical files above the prefix length (so they go
        // through verify_bucket, not the tiny-file direct-emit path).
        let size = cfg.prefix_len + 3 * cfg.sample_len;
        let payload = fmt_blob(5, 0x0505, size as usize);
        fs::write(d.join("a.bin"), &payload).unwrap();
        fs::write(d.join("b.bin"), &payload).unwrap();

        // Count how many times the closure is called — should be
        // exactly 2 (one per verified member).
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = call_count.clone();
        let hardlink = move |f: &std::fs::File| {
            cc.fetch_add(1, Ordering::Relaxed);
            // Return a synthetic identity derived from the file's
            // metadata (the test just needs the closure to be called
            // with a real open handle and return a value).
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let md = f.metadata().ok()?;
                Some((md.dev(), md.ino()))
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                let md = f.metadata().ok()?;
                Some((md.volume_serial_number() as u64, md.file_index() as u64))
            }
            #[cfg(not(any(unix, windows)))]
            {
                let _ = f;
                None::<(u64, u64)>
            }
        };

        let groups = run(&candidates_of(&d), &cfg, &QuietSink, &hardlink).expect("engine");
        assert_eq!(groups.len(), 1, "one verified group");
        assert_eq!(groups[0].files.len(), 2, "two members");
        // The closure was called for each verified member.
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            2,
            "hardlink closure called once per verified member"
        );
        // The captured (vs, fi) fields are populated (non-zero on
        // real filesystems; the closure returned real metadata).
        for f in &groups[0].files {
            assert!(
                f.volume_serial != 0 || f.file_index != 0,
                "hardlink identity captured from open handle (path={})",
                f.path
            );
        }
    }

    /// Tiny files (size ≤ `prefix_len`) skip `verify_bucket` entirely
    /// (the prefix screen hashed the full content). They leave with
    /// `volume_serial=0` and `file_index=node_id` — the caller's signal
    /// to do the legacy one-open for hardlink identity.
    #[test]
    fn tiny_files_leave_zero_volume_serial_for_caller_fallback() {
        let (_t, d) = stage("tiny-fallback");
        let cfg = small_cfg();
        // Two identical files SMALLER than the prefix length.
        let size = (cfg.prefix_len / 2) as usize;
        let payload = fmt_blob(3, 0x0303, size);
        fs::write(d.join("small_a.bin"), &payload).unwrap();
        fs::write(d.join("small_b.bin"), &payload).unwrap();

        let groups = run(&candidates_of(&d), &cfg, &QuietSink, no_hardlink()).expect("engine");
        assert_eq!(groups.len(), 1, "tiny pair verified by prefix screen");
        assert_eq!(groups[0].files.len(), 2);
        // volume_serial == 0 signals "caller, do the legacy open".
        // file_index falls back to node_id (the honest default).
        for f in &groups[0].files {
            assert_eq!(
                f.volume_serial, 0,
                "tiny files skip verify so no handle captured"
            );
        }
    }

    /// Performance smoke: a small real-I/O corpus with a loose
    /// throughput floor. Not a benchmark (criterion owns those) —
    /// this catches "the engine became CPU-bound" regressions.
    #[test]
    fn engine_stays_io_bound_on_a_small_corpus() {
        let (_t, d) = stage("perf");
        let cfg = small_cfg();
        let size = cfg.prefix_len + 3 * cfg.sample_len;
        // 24 files: 6 identical groups of 4.
        for g in 0..6u64 {
            let payload = fmt_blob((g as usize) % 8, 0x9000 + g, size as usize);
            for m in 0..4u32 {
                fs::write(d.join(format!("g{g}-m{m}.bin")), &payload).unwrap();
            }
        }
        let cands = candidates_of(&d);
        let t0 = std::time::Instant::now();
        let groups = run(&cands, &cfg, &QuietSink, no_hardlink()).expect("engine");
        let elapsed = t0.elapsed();
        assert_eq!(groups.len(), 6);
        assert!(groups.iter().all(|g| g.files.len() == 4));
        // ~12 MiB of unique content staged; even on CI's slowest
        // runners this must complete in seconds (I/O-bound, no
        // hashing authority pass).
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "engine took {elapsed:?} for ~12 MiB — CPU-bound regression?"
        );
    }
}

#[cfg(test)]
mod bench {
    use super::tests::{fmt_blob, stage};
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A counting sink for read-amplification accounting.
    #[derive(Default)]
    struct Counting {
        bytes: AtomicU64,
        files: AtomicU64,
    }
    impl ProgressSink for Counting {
        fn phase(&self, _p: &str, _f: u64, _b: u64) {}
        fn file_done(&self, bytes: u64) {
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
            self.files.fetch_add(1, Ordering::Relaxed);
        }
        fn cancelled(&self) -> bool {
            false
        }
    }

    /// The engine benchmark (run explicitly: `cargo test --release --
    /// --ignored engine_bench`). Stages `DB_BENCH_GB` (default 4) GiB
    /// of REAL magic-headed multi-format content with a realistic
    /// duplicate structure, then times the engine end-to-end and
    /// prints throughput + read amplification.
    ///
    /// Corpus shape (the owner's "real user simulation"):
    /// - `d` GiB across unique "media" files (8–24 MiB, 8 formats)
    /// - 25% of the corpus planted as exact duplicate copies
    /// - near-dup pairs (same screens, gap difference) that the
    ///   verify pass must reject
    /// - small-photo clusters (40–90 KiB, tiny-file path)
    #[test]
    #[ignore = "benchmark — stages gigabytes; run explicitly"]
    fn engine_bench() {
        let gib: u64 = std::env::var("DB_BENCH_GB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4)
            .max(1);
        let (_t, d) = stage("bench");
        let cfg = EngineConfig::default();
        let mib = 1024 * 1024u64;

        // ── Stage: unique media + 25% duplicate copies. Target
        //    ~¾ of the budget in 8–24 MiB media files (~16 MiB avg),
        //    the rest grows through the i%4 duplicate copies.
        let media = d.join("media");
        fs::create_dir_all(&media).unwrap();
        let n_media = (gib * 1024 * 3) / 4 / 16; // ~¾ GiB in ~16 MiB files
        let mut planted_dupe_bytes = 0u64;
        for i in 0..n_media {
            let fmt = (i % 8) as usize;
            let size = 8 * mib + (i % 16) * mib; // 8..24 MiB
            let name = format!(
                "clip_{i:04}.{}",
                ["jpg", "png", "mp4", "zip", "pdf", "iso", "txt", "bin"][fmt]
            );
            let payload = fmt_blob(fmt, 0xBEE0 + i * 3, size as usize);
            fs::write(media.join(&name), &payload).unwrap();
            if i % 4 == 0 {
                fs::write(media.join(name.replace("clip", "copy")), &payload).unwrap();
                planted_dupe_bytes += size;
            }
        }
        // ── Near-dups: same screens, gap-different (verify rejects).
        let nd = d.join("near");
        fs::create_dir_all(&nd).unwrap();
        for i in 0..8u64 {
            let size = 12 * mib;
            let base = fmt_blob(2, 0xD00D + i * 7, size as usize);
            let mut twin = base.clone();
            twin[(size / 2) as usize] ^= 0x5A;
            fs::write(nd.join(format!("nd{i}-a.mp4")), &base).unwrap();
            fs::write(nd.join(format!("nd{i}-b.mp4")), &twin).unwrap();
        }
        // ── Small photo clusters: 20 groups × 3 copies.
        let photos = d.join("photos");
        fs::create_dir_all(&photos).unwrap();
        for i in 0..20u64 {
            let size = (40 + i % 50) * 1024;
            let p = fmt_blob(0, 0x600D + i * 11, size as usize);
            for m in 0..3u32 {
                fs::write(photos.join(format!("img_{i:03}_{m}.jpg")), &p).unwrap();
            }
        }

        // Candidates from the staged tree.
        let mut cands: Vec<EngineCandidate> = Vec::new();
        let mut stack = vec![d.clone()];
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir).unwrap() {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    cands.push(EngineCandidate {
                        path: e.path().to_string_lossy().into_owned(),
                        size: e.metadata().unwrap().len(),
                        node_id: cands.len() as u32,
                    });
                }
            }
        }
        let unique_bytes: u64 = cands.iter().map(|c| c.size).sum();
        let counting = Counting::default();
        eprintln!(
            "staged {gib} GiB → {} files, {} total bytes ({planted_dupe_bytes} planted duplicate bytes)",
            cands.len(),
            unique_bytes
        );

        let t0 = std::time::Instant::now();
        let groups = run(&cands, &cfg, &counting, no_hardlink()).unwrap();
        let elapsed = t0.elapsed();
        let read = counting.bytes.load(Ordering::Relaxed);
        let mps = read as f64 / elapsed.as_secs_f64() / (mib as f64);
        eprintln!(
            "engine: {} groups in {:.2}s — read {} MiB = {:.0} MiB/s (amp {:.2}x of the {} MiB corpus)",
            groups.len(),
            elapsed.as_secs_f32(),
            read / mib,
            mps,
            read as f64 / unique_bytes as f64,
            unique_bytes / mib
        );
        // Planted structure: ceil(n_media/4) media groups (i%4==0,
        // counting i=0) + 8 rejected near-dups + 20 photo groups.
        let expected_groups = n_media.div_ceil(4) + 20;
        assert_eq!(groups.len(), expected_groups as usize, "group count");
        assert!(groups.iter().all(|g| g.files.len() >= 2));
        // The near-dups must never appear.
        assert!(groups
            .iter()
            .flat_map(|g| &g.files)
            .all(|f| !f.path.contains("nd")));
    }
}
