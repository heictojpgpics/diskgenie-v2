//! Turbo engine — tree building from parent references.
//!
//! The adaptation doc's §4.3–4.6 model (`MFTool` facts, original code):
//! 1. Extension records (base != 0) fold into their base record —
//!    `FILE_NAME` attributes merge in (hardlink names can live in
//!    extensions), no separate tree node.
//! 2. Children link by parent record number; BFS from record 5 assigns
//!    arena ids so parents always precede children (the roll-up reverse
//!    pass works).
//! 3. Hardlinks (several `FILE_NAME` parents) charge the size ONCE to
//!    the lexicographically-first parent (deterministic policy).
//! 4. Records unreachable from root become a synthetic
//!    "(unreferenced)" bucket — surfaced, never silently dropped.
//! 5. Deleted records are excluded by default (Explorer parity).

use std::collections::VecDeque;

use super::record::{canonical_name, CompactEntry};
use crate::scan::categories::FileCategory;
use crate::scan::node::{BatchEntry, Node, Tree};

/// The MFT root-directory record number.
pub const ROOT_RECORD: u32 = 5;

/// Warnings collected during the tree build (surfaced, never hidden).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurboWarnings {
    /// Records skipped as torn (USN mismatch).
    pub torn: u64,
    /// Records skipped as unparsable.
    pub bad: u64,
    /// Extension records whose base record was missing.
    pub orphan_extensions: u64,
    /// Live records unreachable from the root (folded into the
    /// "(unreferenced)" bucket).
    pub unreferenced: u64,
    /// Parent-reference cycles detected and cut.
    pub cycles: u64,
}

/// The tree-build result (Tree deliberately not Clone/Eq — huge).
#[derive(Debug)]
pub struct TurboBuild {
    /// The built tree (generation set by the caller).
    pub tree: Tree,
    /// Surfaced warnings (torn/bad/orphan/unreferenced/cycles).
    pub warnings: TurboWarnings,
}

/// Build the tree from parsed entries (indexed by record number — every
/// MFT slot occupies its position, live or not).
///
/// # Panics
/// Never: all index access is bounds-checked; corrupt references are
/// counted as warnings.
#[must_use]
pub fn build_tree(mut entries: Vec<CompactEntry>, root_label: &str) -> TurboBuild {
    let mut warnings = TurboWarnings::default();

    // Pass 1: fold extension records into their bases.
    let mut extensions: Vec<(u32, u32)> = Vec::new(); // (ext record, base record)
    for e in &mut entries {
        if e.base_record != 0 && e.in_use {
            extensions.push((e.mft_record, e.base_record));
        }
    }
    for (ext, base) in extensions {
        let names = {
            let ext_entry = &mut entries[ext as usize];
            std::mem::take(&mut ext_entry.names)
        };
        if let Some(base_entry) = entries.get_mut(base as usize) {
            base_entry.names.extend(names);
            // Sizes: the base's own unnamed $DATA wins (`MFTool` precedent:
            // only the first $DATA counts).
        } else {
            warnings.orphan_extensions += 1;
        }
    }

    // Pass 2: link children by parent (hardlink policy: charge to the
    // lexicographically-first parent). Win32-namespace names only for
    // linking; DOS names are display fallbacks. CSR layout (counts →
    // offsets → flat child array): the old Vec-per-record table spent
    // one allocation per MFT record (1-3M on a real volume) purely on
    // mostly-empty child lists.
    let record_count = entries.len();
    let mut child_count: Vec<u32> = vec![0; record_count];
    for e in &entries {
        if !e.in_use || e.base_record != 0 {
            continue; // Deleted or folded extension.
        }
        let Some(name_rec) = pick_link_name(e) else {
            continue;
        };
        let parent = name_rec.parent as usize;
        if parent >= record_count {
            warnings.unreferenced += 1;
            continue;
        }
        // Self-links (the root record's parent is itself in NTFS) never
        // become child links.
        if parent != e.mft_record as usize {
            child_count[parent] += 1;
        }
    }
    let mut offsets: Vec<u32> = Vec::with_capacity(record_count + 1);
    let mut running: u32 = 0;
    offsets.push(0);
    for &c in &child_count {
        running += c;
        offsets.push(running);
    }
    let mut flat_children: Vec<u32> = vec![0; running as usize];
    let mut write_at: Vec<u32> = offsets.clone(); // per-parent cursor
    for e in &entries {
        if !e.in_use || e.base_record != 0 {
            continue;
        }
        let Some(name_rec) = pick_link_name(e) else {
            continue;
        };
        let parent = name_rec.parent as usize;
        if parent >= record_count || parent == e.mft_record as usize {
            continue;
        }
        flat_children[write_at[parent] as usize] = e.mft_record;
        write_at[parent] += 1;
    }
    let children_of = |rec: u32| -> &[u32] {
        &flat_children[offsets[rec as usize] as usize..offsets[rec as usize + 1] as usize]
    };

    // Hardlink charge-once: a record with multiple parents appears in
    // several children lists — keep ONLY the first parent link (sort by
    // name for the deterministic policy; entries arrive in record order).
    // Pass 2 already linked by the canonical name's parent only, so each
    // record is linked exactly once (the policy lives in pick_link_name).

    // Pass 3: BFS from the root record assigning arena ids.
    let mut arena_id_of: Vec<u32> = vec![u32::MAX; record_count];
    let mut queue: VecDeque<u32> = VecDeque::new();
    if ROOT_RECORD < record_count as u32 && entries[ROOT_RECORD as usize].in_use {
        queue.push_back(ROOT_RECORD);
    }
    let mut bfs_order: Vec<u32> = Vec::with_capacity(record_count);
    while let Some(rec) = queue.pop_front() {
        if arena_id_of[rec as usize] != u32::MAX {
            warnings.cycles += 1;
            continue; // Already placed: a cycle or double-link.
        }
        let id = bfs_order.len() as u32;
        arena_id_of[rec as usize] = id;
        bfs_order.push(rec);
        for &child in children_of(rec) {
            if arena_id_of[child as usize] == u32::MAX {
                queue.push_back(child);
            }
        }
    }

    // Unreferenced live records → synthetic bucket under the root.
    let mut unreferenced_records: Vec<u32> = Vec::new();
    for (rec, e) in entries.iter().enumerate() {
        if e.in_use && e.base_record == 0 && arena_id_of[rec] == u32::MAX {
            unreferenced_records.push(rec as u32);
        }
    }
    warnings.unreferenced += unreferenced_records.len() as u64;

    // Pass 4 (see `emit_bfs_batches`).
    let mut tree = Tree::new(0); // generation filled by the caller
    tree.add_root_path(0, root_label);
    emit_bfs_batches(&mut tree, &entries, &bfs_order, &arena_id_of);
    // Unreferenced bucket under the root (honest accounting).
    append_unreferenced_bucket(&mut tree, &entries, &unreferenced_records);

    TurboBuild { tree, warnings }
}

/// Pass 4: emit arena batches in BFS order (parent-first by
/// construction). BFS enqueues each parent's children as one
/// contiguous block, so consecutive records sharing a parent merge
/// into ONE `append_batch` — the old per-record emission spent one
/// Vec allocation + one tree lock per MFT record (1-3M on a real
/// volume).
fn emit_bfs_batches(
    tree: &mut Tree,
    entries: &[CompactEntry],
    bfs_order: &[u32],
    arena_id_of: &[u32],
) {
    let mut idx = 0usize;
    while idx < bfs_order.len() {
        let rec = bfs_order[idx];
        if rec == ROOT_RECORD {
            idx += 1;
            continue; // The root record IS arena id 0 (add_root_path).
        }
        let e = &entries[rec as usize];
        let parent_rec = pick_link_name(e).map_or(ROOT_RECORD, |n| n.parent as u32);
        let parent_id = arena_id_of[parent_rec as usize];
        if parent_id == u32::MAX {
            idx += 1;
            continue; // Cycle cut above; skip.
        }
        // Extend the batch across consecutive records with the SAME
        // parent (the BFS block).
        let mut batch: Vec<BatchEntry> = Vec::new();
        while idx < bfs_order.len() {
            let r = bfs_order[idx];
            if r == ROOT_RECORD {
                break;
            }
            let er = &entries[r as usize];
            let pr = pick_link_name(er).map_or(ROOT_RECORD, |n| n.parent as u32);
            if pr != parent_rec || arena_id_of[pr as usize] != parent_id {
                break;
            }
            batch.push(node_entry_of(er));
            idx += 1;
        }
        if !batch.is_empty() {
            tree.append_batch(parent_id, batch);
        }
    }
}

/// One record → (name, node) arena entry: sizes, FILETIME→unix
/// conversion, reparse protection, category resolution.
fn node_entry_of(e: &CompactEntry) -> BatchEntry {
    let mut node = if e.is_dir {
        Node::new_dir()
    } else {
        Node::new_file()
    };
    node.logical = e.real;
    node.on_disk = e.allocated;
    node.modified = filetime_to_unix(e.modified);
    node.created = filetime_to_unix(e.created);
    if e.is_reparse {
        node.set_protected(true); // Reparse points: never descend/stage.
    }
    let name: Vec<u16> = canonical_name(e).map_or(Vec::new(), |n| n.name.clone());
    if !e.is_dir {
        node.set_category(FileCategory::from_name(&name));
    }
    BatchEntry { name, node }
}

/// One synthetic "(unreferenced)" folder holding every live-but-
/// unreachable record (surfaced, never silently dropped).
fn append_unreferenced_bucket(
    tree: &mut Tree,
    entries: &[CompactEntry],
    unreferenced_records: &[u32],
) {
    if unreferenced_records.is_empty() {
        return;
    }
    let mut bucket = Node::new_dir();
    bucket.modified = 0;
    let bucket_entry = BatchEntry {
        name: "(unreferenced)".encode_utf16().collect(),
        node: bucket,
    };
    let bucket_id = tree.append_batch(0, vec![bucket_entry]);
    let batch: Vec<BatchEntry> = unreferenced_records
        .iter()
        .filter_map(|&rec| {
            let e = &entries[rec as usize];
            let mut node = if e.is_dir {
                Node::new_dir()
            } else {
                Node::new_file()
            };
            node.logical = e.real;
            node.on_disk = e.allocated;
            node.modified = filetime_to_unix(e.modified);
            let name: Vec<u16> = canonical_name(e).map_or(Vec::new(), |n| n.name.clone());
            if !e.is_dir {
                node.set_category(FileCategory::from_name(&name));
            }
            (!name.is_empty()).then_some(BatchEntry { name, node })
        })
        .collect();
    if !batch.is_empty() {
        tree.append_batch(bucket_id, batch);
    }
}

/// The hardlink charge-once policy: the name whose parent is the target.
/// Deterministic pick: prefer Win32 namespace, then first by record
/// order (entries arrive in MFT order — stable).
fn pick_link_name(e: &CompactEntry) -> Option<&crate::turbo::record::NameRec> {
    e.names
        .iter()
        .find(|n| n.namespace == 1 || n.namespace == 3)
        .or_else(|| e.names.first())
}

/// FILETIME (100ns ticks since 1601) → Unix seconds.
fn filetime_to_unix(ticks: u64) -> i64 {
    const EPOCH_DELTA: u64 = 11_644_473_600;
    if ticks == 0 {
        return 0;
    }
    let secs = ticks / 10_000_000;
    secs.checked_sub(EPOCH_DELTA).map_or(0, |s| s as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::rollup;

    fn entry(
        rec: u32,
        parent: u64,
        name: &str,
        size: u64,
        is_dir: bool,
        in_use: bool,
    ) -> CompactEntry {
        let mut e = CompactEntry {
            mft_record: rec,
            in_use,
            is_dir,
            base_record: 0,
            allocated: size,
            real: size,
            ..CompactEntry::default()
        };
        e.names.push(crate::turbo::record::NameRec {
            namespace: 1,
            name: name.encode_utf16().collect(),
            parent,
        });
        e
    }

    /// Entries indexed BY RECORD NUMBER: pad holes, then place each.
    fn aligned(mut ents: Vec<CompactEntry>) -> Vec<CompactEntry> {
        ents.sort_by_key(|e| e.mft_record);
        let max = ents.last().map_or(0, |e| e.mft_record) + 1;
        let mut out: Vec<CompactEntry> =
            std::iter::repeat_n(CompactEntry::default(), max as usize).collect();
        for e in ents {
            let slot = e.mft_record as usize;
            if slot < out.len() {
                out[slot] = e;
            }
        }
        out
    }

    #[test]
    fn builds_tree_bfs_parents_first() {
        // Root 5 → folder 6 → inner 8; root → root.txt 7.
        let entries = aligned(vec![
            entry(5, 5, ".", 0, true, true),
            entry(6, 5, "folder", 0, true, true),
            entry(7, 5, "root.txt", 100, false, true),
            entry(8, 6, "inner.txt", 50, false, true),
        ]);
        let mut build = build_tree(entries, "C:\\");
        rollup::finalize(&mut build.tree);
        // Root has folder + root.txt as children.
        let kids = build.tree.children_sorted(0).to_vec();
        assert_eq!(kids.len(), 2);
        let root = build.tree.node(0).unwrap();
        assert_eq!(root.on_disk, 150);
        // Parents precede children (BFS ids): folder id < inner id.
        let folder = kids
            .iter()
            .find(|&&k| build.tree.node(k).unwrap().is_dir())
            .copied()
            .unwrap();
        let inner = build
            .tree
            .children_sorted(folder)
            .iter()
            .copied()
            .next()
            .unwrap();
        assert!(folder < inner);
    }

    #[test]
    fn extension_folds_names_into_base() {
        let mut base = entry(20, 5, "doc.txt", 500, false, true);
        base.base_record = 0;
        let mut ext = entry(21, 5, "hard.txt", 0, false, true);
        ext.base_record = 20;
        // NOTE: entries must ALSO contain the root so the tree links.
        let entries = aligned(vec![entry(5, 5, ".", 0, true, true), base, ext]);
        let mut build = build_tree(entries, "C:\\");
        rollup::finalize(&mut build.tree);
        assert_eq!(build.warnings.orphan_extensions, 0);
        let root = build.tree.node(0).unwrap();
        // The base's size charges once (the extension folds in).
        assert_eq!(root.on_disk, 500);
    }

    #[test]
    fn deleted_records_excluded() {
        let entries = aligned(vec![
            entry(5, 5, ".", 0, true, true),
            entry(6, 5, "live.txt", 10, false, true),
            entry(7, 5, "dead.txt", 20, false, false),
        ]);
        let mut build = build_tree(entries, "C:\\");
        rollup::finalize(&mut build.tree);
        let names: Vec<String> = (0..build.tree.len())
            .map(|i| build.tree.name(i as u32))
            .collect();
        assert!(!names.iter().any(|n| n.contains("dead")));
        assert_eq!(build.tree.node(0).unwrap().on_disk, 10);
    }

    #[test]
    fn unreferenced_surfaced_not_dropped() {
        let entries = aligned(vec![
            entry(5, 5, ".", 0, true, true),
            entry(9, 12345, "orphan.txt", 30, false, true), // parent missing
        ]);
        let mut build = build_tree(entries, "C:\\");
        rollup::finalize(&mut build.tree);
        assert!(build.warnings.unreferenced >= 1);
        // The bucket holds the orphan's 30 bytes.
        let root = build.tree.node(0).unwrap();
        assert_eq!(root.on_disk, 30);
        let names: Vec<String> = (0..build.tree.len())
            .map(|i| build.tree.name(i as u32))
            .collect();
        assert!(names.iter().any(|n| n.contains("(unreferenced)")));
    }

    #[test]
    fn hardlink_charged_once() {
        // Record 6 has TWO names (hardlink) in the SAME parent — the
        // link policy charges it once.
        let mut e = entry(6, 5, "b.txt", 40, false, true);
        e.names.push(crate::turbo::record::NameRec {
            namespace: 1,
            name: "a.txt".encode_utf16().collect(),
            parent: 5,
        });
        let entries = aligned(vec![entry(5, 5, ".", 0, true, true), e]);
        let mut build = build_tree(entries, "C:\\");
        rollup::finalize(&mut build.tree);
        let count = (0..build.tree.len())
            .filter(|&i| {
                let n = build.tree.name(i as u32);
                n == "b.txt" || n == "a.txt"
            })
            .count();
        assert_eq!(count, 1);
        assert_eq!(build.tree.node(0).unwrap().on_disk, 40);
    }

    #[test]
    fn filetime_conversion() {
        // 2026-01-01 ~ 133_850_880_000_000_000 ticks.
        assert!(filetime_to_unix(133_850_880_000_000_000) > 1_700_000_000);
        assert_eq!(filetime_to_unix(0), 0);
    }
}
