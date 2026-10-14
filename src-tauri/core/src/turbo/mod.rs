//! Turbo engine (spec §5; doc 02 §4; doc 03 M7): whole-drive NTFS scans
//! via direct Master File Table parsing.
//!
//! Layering: THIS module is pure, host-testable byte parsing
//! (`runs`, `record`, `tree`) — the Windows volume/FSCTL/privilege side
//! lives in the app's `platform/win.rs` seam (doc 02 §2) and feeds
//! bytes + geometry in. The one allowed user-visible fallback
//! (R2): turbo → standard engine, with a stated reason.

pub mod record;
pub mod runs;
pub mod tree;

/// Volume geometry (from `FSCTL_GET_NTFS_VOLUME_DATA` or the boot
/// sector fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Bytes per disk sector (512 typically).
    pub bytes_per_sector: u32,
    /// Bytes per cluster (the allocation unit).
    pub bytes_per_cluster: u32,
    /// Bytes per MFT record (1 KiB typically).
    pub bytes_per_record: u32,
    /// Valid data length of the $MFT stream (bounds the record count).
    pub mft_valid_data_length: u64,
}

/// Per-record parse outcome (folded into the warning counts).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ok,
    /// Hole/BAAD/zeroed slot: index-aligned default, no warning.
    Hole,
    Torn,
    Bad,
}

/// Parse the WHOLE staged MFT buffer (every slot, index-aligned) into
/// entries + warnings. Torn/bad records are skipped and counted —
/// never fatal (a single corrupt record must not kill a scan).
///
/// PARALLEL (`par_chunks_mut`, in-place fixups): records are disjoint
/// slices and every fixup patches only its own record's sector tails,
/// so per-record parsing is race-free. This replaces the previous
/// single-threaded loop that ALSO allocated a fresh 1 KiB copy per
/// record (1-3M serial allocations + memcpys on a real volume: the
/// turbo path's biggest CPU cost). Order is preserved (rayon `map`
/// is order-stable), so index alignment holds exactly as before.
#[must_use]
pub fn parse_all(
    mft: &mut [u8],
    geometry: &Geometry,
) -> (Vec<record::CompactEntry>, tree::TurboWarnings) {
    use rayon::prelude::*;
    let sector = geometry.bytes_per_sector as usize;
    let rec_size = geometry.bytes_per_record as usize;
    let per_record: Vec<(record::CompactEntry, Outcome)> = mft
        .par_chunks_mut(rec_size)
        .enumerate()
        .map(|(i, slot)| {
            if slot.len() < 0x30 {
                return (record::CompactEntry::default(), Outcome::Bad);
            }
            match record::parse_record(slot, i as u32, sector) {
                Ok(e) => (e, Outcome::Ok),
                Err(record::RecordError::Torn) => (record::CompactEntry::default(), Outcome::Torn),
                Err(record::RecordError::NotFile) => {
                    (record::CompactEntry::default(), Outcome::Hole)
                }
                Err(_) => (record::CompactEntry::default(), Outcome::Bad),
            }
        })
        .collect();
    let mut warnings = tree::TurboWarnings::default();
    let entries: Vec<record::CompactEntry> = per_record
        .into_iter()
        .map(|(e, outcome)| {
            match outcome {
                Outcome::Torn => warnings.torn += 1,
                Outcome::Bad => warnings.bad += 1,
                Outcome::Ok | Outcome::Hole => {}
            }
            e
        })
        .collect();
    (entries, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::rollup;

    const GEOM: Geometry = Geometry {
        bytes_per_sector: 512,
        bytes_per_cluster: 4096,
        bytes_per_record: 1024,
        mft_valid_data_length: 4096,
    };

    /// Synthetic full pipeline: MFT bytes → entries → tree → totals.
    #[test]
    fn end_to_end_synthetic_mft() {
        use super::record::CompactEntry;
        use super::record::NameRec;
        // Record 5 = root dir; 6 = folder; 7,8 = files; 9 = deleted.
        let mk = |rec: u32, parent: u64, name: &str, size: u64, dir: bool, in_use: bool| {
            let mut e = CompactEntry {
                mft_record: rec,
                in_use,
                is_dir: dir,
                allocated: size,
                real: size,
                ..CompactEntry::default()
            };
            e.names.push(NameRec {
                namespace: 1,
                name: name.encode_utf16().collect(),
                parent,
            });
            e
        };
        let entries = vec![
            CompactEntry::default(), // 0 hole
            CompactEntry::default(), // 1
            CompactEntry::default(), // 2
            CompactEntry::default(), // 3
            CompactEntry::default(), // 4
            mk(5, 5, ".", 0, true, true),
            mk(6, 5, "folder", 0, true, true),
            mk(7, 5, "a.txt", 100, false, true),
            mk(8, 6, "b.bin", 200, false, true),
            mk(9, 5, "gone.txt", 999, false, false),
        ];
        let mut build = tree::build_tree(entries, "C:\\");
        rollup::finalize(&mut build.tree);
        let root = build.tree.node(0).unwrap();
        assert_eq!(root.on_disk, 300); // 100 + 200; deleted excluded
        assert_eq!(root.logical, 300);
        let root_extra = &build.tree.dir_extras[root.dir_index as usize];
        assert_eq!(root_extra.file_count, 2);
    }

    #[test]
    fn parse_all_skips_and_counts_torn() {
        // Two records: one valid-ish (zeroed signature = hole), one torn.
        let mut mft = vec![0u8; 2048];
        // First slot: zeros → NotFile → index-aligned hole, no warning.
        // Second slot: build a FILE record then corrupt a tail.
        let sector = 512usize;
        let rec2 = &mut mft[1024..];
        rec2[0..4].copy_from_slice(b"FILE");
        rec2[4..6].copy_from_slice(&0x28u16.to_le_bytes());
        rec2[6..8].copy_from_slice(&3u16.to_le_bytes());
        rec2[0x14..0x16].copy_from_slice(&0x30u16.to_le_bytes());
        rec2[0x16..0x18].copy_from_slice(&0x1u16.to_le_bytes());
        // USA [usn, tail0, tail1].
        let usn = 0xABCDu16;
        let tail0 = [rec2[sector - 2], rec2[sector - 1]];
        let tail1 = [rec2[2 * sector - 2], rec2[2 * sector - 1]];
        rec2[0x28..0x2A].copy_from_slice(&usn.to_le_bytes());
        rec2[0x2A..0x2C].copy_from_slice(&tail0);
        rec2[0x2C..0x2E].copy_from_slice(&tail1);
        rec2[sector - 2..sector].copy_from_slice(&usn.to_le_bytes());
        rec2[2 * sector - 2..2 * sector].copy_from_slice(&usn.to_le_bytes());
        // Corrupt the second sector tail → torn.
        mft[1024 + 2 * sector - 1] ^= 0xFF;
        let (entries, warnings) = parse_all(&mut mft, &GEOM);
        assert_eq!(entries.len(), 2);
        assert_eq!(warnings.torn, 1);
    }
}
