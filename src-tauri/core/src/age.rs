//! Age analysis (`BuildPrompt` §7, Age Map + doc 02 §7).
//!
//! Age uses **last-modified time** (NTFS last-access updates are disabled
//! by default and unreliable — spec §7). All computation happens here,
//! off the UI thread; the command layer caches per (generation, node).

use serde::{Deserialize, Serialize};

use crate::scan::node::Tree;

/// The six age buckets (spec §7 Age Map), in order.
pub const BUCKETS: [&str; 6] = [
    "Last 7 days",
    "8–30 days",
    "1–3 months",
    "3–12 months",
    "1–2 years",
    "Over 2 years",
];

/// Bucket boundaries in seconds before `now`.
const BOUNDS: [i64; 5] = [
    7 * 86_400,
    30 * 86_400,
    91 * 86_400,
    365 * 86_400,
    730 * 86_400,
];

/// Bucket index of a Unix-seconds mtime (0..=5).
#[must_use]
pub fn bucket_of(modified: i64, now: i64) -> usize {
    if modified <= 0 {
        return 5; // Unknown counts as oldest — honest display via "—".
    }
    let age = now - modified;
    for (i, &b) in BOUNDS.iter().enumerate() {
        if age < b {
            return i;
        }
    }
    5
}

/// "How old are these bytes?" — on-disk bytes per bucket for a subtree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgeBuckets {
    /// Bytes per bucket (6 slots, on-disk sizes).
    pub bytes: [u64; 6],
    /// Total bytes considered (live files).
    pub total: u64,
}

/// Compute bucket totals for the subtree under `id`.
#[must_use]
pub fn bucket_totals(tree: &Tree, id: u32, now: i64) -> AgeBuckets {
    let mut bytes = [0u64; 6];
    tree.walk(id, |_, n| {
        if !n.is_dir() {
            bytes[bucket_of(n.modified, now)] =
                bytes[bucket_of(n.modified, now)].saturating_add(n.on_disk);
        }
    });
    let total = bytes.iter().sum();
    AgeBuckets { bytes, total }
}

/// "Bytes by last-modified month" — year × month heatmap (spec §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonthHeatmap {
    /// First year present (e.g. 2019).
    pub first_year: i32,
    /// Last year present (e.g. 2026).
    pub last_year: i32,
    /// `bytes[(year - first_year) * 12 + month]`, 12 rows per year.
    pub bytes: Vec<u64>,
    /// Max cell value (legend "more" end).
    pub max: u64,
    /// `(year, zero-based month)` of the busiest month.  This is an IPC-facing
    /// value: month indexes must agree with `bytes` and JavaScript's
    /// `Date#getUTCMonth` (January = 0), not civil-date month numbers.
    pub busiest: Option<(i32, usize)>,
}

/// Civil-from-days algorithm (Howard Hinnant) — no chrono dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Year/month of a Unix-seconds timestamp.
#[must_use]
pub fn year_month(unix: i64) -> Option<(i32, usize)> {
    if unix <= 0 {
        return None;
    }
    let days = unix.div_euclid(86_400);
    let (y, m, _) = civil_from_days(days);
    Some((y as i32, m as usize))
}

/// Compute the month heatmap for the subtree under `id`.
#[must_use]
pub fn month_heatmap(tree: &Tree, id: u32) -> MonthHeatmap {
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;
    let mut cells: std::collections::HashMap<(i32, usize), u64> = std::collections::HashMap::new();
    tree.walk(id, |_, n| {
        if n.is_dir() || n.modified <= 0 {
            return;
        }
        if let Some((y, m)) = year_month(n.modified) {
            min_y = min_y.min(y);
            max_y = max_y.max(y);
            // NOTE: the naive `= cells[&(y, m)] + …` RHS reads the map
            // before the entry exists (assignment evaluates the value
            // expression first) → "no entry found" panic on the FIRST
            // file of the first month. `+=` through the entry is the
            // correct accumulate-once form (CI caught this; the host
            // never ran month_heatmap before).
            *cells.entry((y, m)).or_insert(0) += n.on_disk;
        }
    });
    if min_y == i32::MAX {
        return MonthHeatmap {
            first_year: 1970,
            last_year: 1970,
            bytes: vec![0; 12],
            max: 0,
            busiest: None,
        };
    }
    let years = (max_y - min_y + 1) as usize;
    let mut bytes = vec![0u64; years * 12];
    let mut max = 0u64;
    let mut busiest: Option<(i32, usize)> = None;
    for ((y, m), v) in cells {
        let idx = (y - min_y) as usize * 12 + (m - 1);
        bytes[idx] = v;
        if v > max {
            max = v;
            // `year_month` deliberately returns a civil month (1..=12), but
            // `bytes` and the UI address columns with a zero-based index.
            // Keep the entire heatmap DTO on that one convention.
            busiest = Some((y, m - 1));
        }
    }
    MonthHeatmap {
        first_year: min_y,
        last_year: max_y,
        bytes,
        max,
        busiest,
    }
}

/// "Big & Untouched" — files ≥ 40 MB not modified in over a year
/// (spec §7), ranked largest-first, capped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BigFile {
    /// Node id.
    pub id: u32,
    /// Logical size in bytes.
    pub logical: u64,
    /// On-disk size in bytes.
    pub on_disk: u64,
    /// Last modified (Unix seconds).
    pub modified: i64,
}

/// Minimum size for the list (40 MB).
pub const BIG_FILE_MIN: u64 = 40 * 1024 * 1024;

/// Age threshold (365 days).
pub const UNTOUCHED_AFTER: i64 = 365 * 86_400;

/// Collect Big & Untouched files under `id`, largest-first.
#[must_use]
pub fn big_untouched(tree: &Tree, id: u32, now: i64, cap: usize) -> Vec<BigFile> {
    let mut out: Vec<BigFile> = Vec::new();
    tree.walk(id, |nid, n| {
        if !n.is_dir()
            && n.logical >= BIG_FILE_MIN
            && n.modified > 0
            && now - n.modified > UNTOUCHED_AFTER
            && !n.is_cloud_placeholder()
        {
            out.push(BigFile {
                id: nid,
                logical: n.logical,
                on_disk: n.on_disk,
                modified: n.modified,
            });
        }
    });
    out.sort_unstable_by_key(|f| std::cmp::Reverse(f.logical));
    out.truncate(cap);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::node::{BatchEntry, Node};

    fn file(name: &str, on_disk: u64, modified: i64) -> BatchEntry {
        let mut node = Node::new_file();
        node.logical = on_disk;
        node.on_disk = on_disk;
        node.modified = modified;
        BatchEntry {
            name: name.encode_utf16().collect(),
            node,
        }
    }

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn bucket_boundaries() {
        assert_eq!(bucket_of(NOW, NOW), 0);
        assert_eq!(bucket_of(NOW - 6 * 86_400, NOW), 0);
        assert_eq!(bucket_of(NOW - 8 * 86_400, NOW), 1);
        assert_eq!(bucket_of(NOW - 60 * 86_400, NOW), 2);
        assert_eq!(bucket_of(NOW - 200 * 86_400, NOW), 3);
        assert_eq!(bucket_of(NOW - 500 * 86_400, NOW), 4);
        assert_eq!(bucket_of(NOW - 3 * 365 * 86_400, NOW), 5);
        assert_eq!(bucket_of(0, NOW), 5); // unknown → oldest bucket
    }

    #[test]
    fn totals_walk_subtree() {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\");
        t.append_batch(
            0,
            vec![
                file("a", 100, NOW),
                file("b", 200, NOW - 10 * 86_400),
                file("c", 400, NOW - 800 * 86_400),
            ],
        );
        let b = bucket_totals(&t, 0, NOW);
        assert_eq!(b.bytes[0], 100);
        assert_eq!(b.bytes[1], 200);
        assert_eq!(b.bytes[5], 400);
        assert_eq!(b.total, 700);
    }

    #[test]
    fn year_month_math() {
        assert_eq!(year_month(0), None);
        // 2026-09-21 ~ 1,790,000,000 (within the fixture range).
        let (y, m) = year_month(1_790_000_000).expect("valid");
        assert_eq!((y, m), (2026, 9));
        let (y2, _m2) = year_month(0).unwrap_or((2026, 9));
        assert_eq!(y2, 2026); // unreachable path kept honest
    }

    #[test]
    fn big_untouched_filter() {
        let mut t = Tree::new(2);
        t.add_root_path(0, "C:\\");
        t.append_batch(
            0,
            vec![
                file("big_old", 100 * 1024 * 1024, NOW - 400 * 86_400),
                file("big_new", 100 * 1024 * 1024, NOW),
                file("small_old", 1000, NOW - 400 * 86_400),
                file("huge_old", 900 * 1024 * 1024, NOW - 500 * 86_400),
            ],
        );
        let list = big_untouched(&t, 0, NOW, 100);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, 4); // huge first
        assert_eq!(list[1].id, 1);
    }

    #[test]
    fn month_heatmap_first_file_does_not_panic() {
        // Regression: the first (y, m) key must insert, not index-read.
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\");
        t.append_batch(0, vec![file("only", 10, 1_790_000_000)]);
        let h = month_heatmap(&t, 0);
        assert_eq!(h.first_year, 2026);
        assert_eq!(h.last_year, 2026);
        assert_eq!(h.bytes.len(), 12);
        assert_eq!(h.bytes[8], 10); // September = index 8
        assert_eq!(h.max, 10);
        assert_eq!(h.busiest, Some((2026, 8))); // September, zero-based
    }

    #[test]
    fn month_heatmap_multi_year_and_accumulate() {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\");
        // Two files in the SAME month must ADD, not overwrite; a third
        // two years later spans the year axis.
        let m1 = 1_790_000_000; // 2026-09
        let m2 = 1_726_000_000; // 2024-09 (approx, verified below)
        let (y2, mo2) = year_month(m2).expect("valid");
        assert_eq!((y2, mo2), (2024, 9));
        t.append_batch(
            0,
            vec![
                file("a", 100, m1),
                file("b", 50, m1),
                file("c", 7, m2),
                dir("sub"),
            ],
        );
        let h = month_heatmap(&t, 0);
        assert_eq!(h.first_year, 2024);
        assert_eq!(h.last_year, 2026);
        assert_eq!(h.bytes.len(), 3 * 12);
        let idx_2024_09 = (2024 - h.first_year) as usize * 12 + 8;
        let idx_2026_09 = (2026 - h.first_year) as usize * 12 + 8;
        assert_eq!(h.bytes[idx_2024_09], 7);
        assert_eq!(h.bytes[idx_2026_09], 150); // 100 + 50 accumulated
        assert_eq!(h.max, 150);
        assert_eq!(h.busiest, Some((2026, 8))); // September, zero-based
    }

    #[test]
    fn month_heatmap_empty_and_unknown_times() {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\");
        t.append_batch(0, vec![file("unknown", 10, 0), dir("emptydir")]);
        let h = month_heatmap(&t, 0);
        assert_eq!(h.first_year, 1970);
        assert_eq!(h.bytes, vec![0; 12]);
        assert_eq!(h.max, 0);
        assert_eq!(h.busiest, None);
    }

    /// A directory entry helper (dirs are skipped by the heatmap walk).
    fn dir(name: &str) -> BatchEntry {
        let mut node = Node::new_dir();
        node.modified = 5;
        BatchEntry {
            name: name.encode_utf16().collect(),
            node,
        }
    }
}
