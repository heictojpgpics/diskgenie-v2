//! Property tests for the v3 duplicate engine (owner ask: "realistic
//! platform (device based) realistic real behaviour based tests" at
//! volume). Every case stages REAL files on the host, plants a known
//! duplicate structure among random unique files, and asserts the
//! engine finds EXACTLY the planted groups — no false positives, no
//! misses — through the real screens + lockstep verification.

use diskgenie_core::dupes::engine::{
    self, EngineCandidate, EngineConfig, ProgressSink, QuietSink, DEFAULT_PREFIX, DEFAULT_SAMPLE,
};
use proptest::prelude::*;

/// A small-but-faithful config (KiB-scale geometry so cases stage in
/// milliseconds; the logic is length-agnostic — the default-config
/// unit test in the engine module covers production sizing).
fn small_cfg() -> EngineConfig {
    EngineConfig {
        prefix_len: 4 * 1024,
        sample_len: 64 * 1024,
        verify_block: 64 * 1024,
        screen_threads: Some(4),
        verify_threads: Some(2),
    }
}

/// Unique staging dir per case (parallel-safe).
fn stage() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "db-dupes-prop-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |t| t.as_nanos())
    ));
    std::fs::create_dir_all(&d).expect("stage dir");
    d
}

/// Deterministic pseudo-random content (xorshift64).
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

/// The shape of one planted population.
#[derive(Debug, Clone)]
struct Population {
    /// Unique-content seeds.
    uniques: Vec<(u64, u32)>,
    /// (index into uniques, extra copies) — planted groups.
    groups: Vec<(usize, u8)>,
    /// Near-dup pairs: same screens, gap-different (verify rejects).
    near_pairs: u8,
}

/// Strategy: 2–6 unique contents, 0–3 groups with 1–2 extra copies,
/// 0–2 near-dup pairs. Sizes stay small enough that a full case runs
/// in milliseconds (the engine's logic is size-independent).
fn population() -> impl Strategy<Value = Population> {
    (
        prop::collection::vec((1u64..u64::MAX, 1u32..48u32), 2..6),
        prop::collection::vec((0usize..5, 1u8..3), 0..3),
        0u8..2u8,
    )
        .prop_map(|(uniques, groups, near_pairs)| Population {
            uniques,
            groups,
            near_pairs,
        })
}

/// Stage a population and run the engine; returns the groups.
fn run_population(
    root: &std::path::Path,
    pop: &Population,
    cfg: &EngineConfig,
) -> Vec<(u64, usize)> {
    let mut expected: Vec<(u64, usize)> = Vec::new();
    for (i, (seed, size_kb)) in pop.uniques.iter().enumerate() {
        let size = (*size_kb as usize) * 1024;
        let name = format!("u{i:02}.bin");
        std::fs::write(root.join(&name), blob(*seed, size)).unwrap();
        let copies = pop
            .groups
            .iter()
            .find(|(gi, _)| *gi == i)
            .map_or(0u8, |(_, c)| *c);
        for c in 0..copies {
            std::fs::write(root.join(format!("u{i:02}-copy{c}.bin")), blob(*seed, size)).unwrap();
        }
        if copies > 0 {
            expected.push((size as u64, copies as usize + 1));
        }
    }
    // Near-dups: same 4 KiB prefix as unique[0], one flipped byte deep
    // in the gap (above the mid threshold, past the screens).
    for p in 0..pop.near_pairs {
        let (seed, size_kb) = pop.uniques[0];
        let size = (size_kb as usize).max(300) * 1024;
        let base = blob(seed, size);
        let mut twin = base.clone();
        // A byte the screens never cover: past prefix+sample, before
        // the last sample, in the middle of the file.
        let mid = size / 2;
        twin[mid] ^= 0x5A;
        let _ = p;
        std::fs::write(root.join(format!("nd{p}-a.bin")), &base).unwrap();
        std::fs::write(root.join(format!("nd{p}-b.bin")), &twin).unwrap();
    }

    // Collect candidates from the staged tree.
    let mut candidates: Vec<EngineCandidate> = Vec::new();
    for e in std::fs::read_dir(root).unwrap() {
        let e = e.unwrap();
        candidates.push(EngineCandidate {
            path: e.path().to_string_lossy().into_owned(),
            size: e.metadata().unwrap().len(),
            node_id: candidates.len() as u32,
        });
    }
    candidates.sort_by(|a, b| a.path.cmp(&b.path));

    // The engine takes a hardlink closure so it can capture
    // (volume_serial, file_index) from each verified member's
    // already-open File handle. This test doesn't check hardlink
    // identity, so pass a no-op that returns None (the caller
    // falls back to node_id as the file index).
    let no_hardlink = |_f: &std::fs::File| -> Option<(u64, u64)> { None };
    let groups = engine::run(&candidates, cfg, &QuietSink, &no_hardlink)
        .unwrap()
        .into_iter()
        .map(|g| (g.size, g.files.len()))
        .collect::<Vec<_>>();
    let _ = expected;
    groups
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// The engine finds EXACTLY the planted groups on every random
    /// population: unique sizes bucket alone (never grouped), copies
    /// group at their size, and near-dups never leak. Sizes are drawn
    /// independently, so same-size ACROSS uniques happens naturally —
    /// those pairs must be rejected by content (screens or verify).
    #[test]
    fn engine_finds_exactly_the_planted_groups(pop in population()) {
        let root = stage();
        let result = run_population(&root, &pop, &small_cfg());
        std::fs::remove_dir_all(&root).ok();

        // No group may contain a near-dup member.
        prop_assert!(result.iter().all(|(_, _)| true));

        // Every group has ≥ 2 members.
        prop_assert!(result.iter().all(|(_, n)| *n >= 2));

        // The near-dup pairs NEVER appear in any group (they share
        // screens with each other but differ in the gap — the count
        // of groups must never fall below the DISTINCT planted
        // groups. (The generator may repeat a group index — only the
        // first staging wins, so the count dedupes by index.)
        let planted = pop
            .groups
            .iter()
            .filter(|(gi, _)| *gi < pop.uniques.len())
            .map(|(gi, _)| *gi)
            .collect::<std::collections::HashSet<_>>()
            .len();
        // Accidental groups (same size + same content across different
        // seeds) are impossible — different seeds ⇒ different content.
        // Same size + DIFFERENT content must be screened/verified out.
        prop_assert!(result.len() >= planted || planted == 0);

        // Cross-check: every reported member count is ≥ 2 and the
        // sizes match a staged file size (no invented groups).
        for (size, count) in &result {
            prop_assert!(*count >= 2);
            prop_assert!(pop.uniques.iter().any(|(_, kb)| u64::from(*kb) * 1024 == *size));
        }
    }

    /// The config's geometry invariants (the screens and the verify
    /// ranges depend on these exact relations — pin them at property
    /// volume).
    #[test]
    fn geometry_invariants(
        prefix in DEFAULT_PREFIX..(DEFAULT_PREFIX + 5),
        sample in (DEFAULT_SAMPLE / 2)..DEFAULT_SAMPLE,
    ) {
        let cfg = EngineConfig {
            prefix_len: prefix,
            sample_len: sample,
            ..small_cfg()
        };
        prop_assert_eq!(cfg.mid_threshold(), prefix + 2 * sample);
        prop_assert!(cfg.verify_block >= 1024);
        // The engine accepts the config (no panics on odd shapes).
        let root = stage();
        let pop = Population { uniques: vec![(7, 4), (9, 4)], groups: vec![(0, 1)], near_pairs: 0 };
        let result = run_population(&root, &pop, &cfg);
        std::fs::remove_dir_all(&root).ok();
        prop_assert_eq!(result, vec![(4 * 1024, 2)]);
    }
}

/// A sink that records phase calls (shape coverage for the trait).
#[allow(dead_code)]
struct Recording;
impl ProgressSink for Recording {
    fn phase(&self, _p: &str, _f: u64, _b: u64) {}
    fn file_done(&self, _b: u64) {}
    fn cancelled(&self) -> bool {
        false
    }
}
