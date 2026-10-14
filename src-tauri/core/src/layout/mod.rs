//! Layout engines (`BuildPrompt` §7 + doc 02 §5).
//!
//! Layouts are computed IN RUST and returned as a flat binary frame:
//! `[u32 meta_len LE][meta JSON][cells × 32 B][sizes × 8 B]` — each cell
//! is a fixed-size `#[repr(C)]` 32-byte record, followed by a tail of one
//! little-endian `u64` on-disk size per cell (same order; consumed by the
//! JS twin for two-line "name / size" cell labels). Never more than
//! 20,000 cells per layout.
//!
//! Cell geometry is 5 × f32 whose meaning depends on the mode:
//! - **Treemap / Flame**: `[x, y, w, h, spare]`
//! - **Sunburst**: `[a0, a1, r0, r1, spare]` (center lives in metadata)
//! - **Bubbles**: `[cx, cy, r, spare, spare]`
//! - **Mind Map**: `[x, y, r, parent_x, parent_y]` (link = curve to parent)
//!
//! `rgba` is packed `0xRRGGBBAA` and serialized little-endian field-wise
//! (the JS twin decodes with `DataView` — see `src/viz/layoutIpc.ts`).

pub mod bubbles;
pub mod flame;
pub mod groups;
pub mod mindmap;
pub mod regroup;
pub mod sunburst;
pub mod treemap;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::scan::node::Tree;

/// Hard cell budget (spec §7: "Never return more than ~20,000 cells").
pub const MAX_CELLS: usize = 20_000;

/// A regroup/label-group tuple: `(synthetic id, name, size, color, members)`
/// where members are `(real node id, on-disk size)`.
pub type GroupTuple = (u32, String, u64, u32, Vec<(u32, u64)>);

/// Cell kind encoded in `Cell::flags` bits 0..=2.
pub mod cell_kind {
    /// Rectangle (treemap, flame).
    pub const RECT: u16 = 0;
    /// Annulus arc (sunburst).
    pub const ARC: u16 = 1;
    /// Circle (bubbles).
    pub const CIRCLE: u16 = 2;
    /// Mind-map dot (link drawn to the parent position).
    pub const DOT: u16 = 3;
    /// Label strip (treemap group header).
    pub const HEADER: u16 = 4;
    /// Extra flag (bit 3): the cell's node is a directory. Lets the JS
    /// hover chip and dblclick logic act without a round trip.
    pub const DIR_BIT: u16 = 1 << 3;
}

/// One layout cell — exactly 32 bytes (spec §7).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    /// Real node id or synthetic group id (`0xFFFF_0000 + n`).
    pub id: u32,
    /// Depth from the layout root.
    pub depth: u16,
    /// `cell_kind` | extra mode bits.
    pub flags: u16,
    /// `0xRRGGBBAA`.
    pub rgba: u32,
    /// Geometry (meaning per mode; see module docs).
    pub g: [f32; 5],
}

const _: () = assert!(size_of::<Cell>() == 32, "spec §7: cell must be 32 bytes");

impl Cell {
    /// Rectangle constructor.
    #[must_use]
    pub fn rect(id: u32, depth: u16, rgba: u32, x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            id,
            depth,
            flags: cell_kind::RECT,
            rgba,
            g: [x, y, w, h, 0.0],
        }
    }

    /// Sunburst arc constructor.
    #[must_use]
    pub fn arc(id: u32, depth: u16, rgba: u32, a0: f32, a1: f32, r0: f32, r1: f32) -> Self {
        Self {
            id,
            depth,
            flags: cell_kind::ARC,
            rgba,
            g: [a0, a1, r0, r1, 0.0],
        }
    }

    /// Circle constructor.
    #[must_use]
    pub fn circle(id: u32, depth: u16, rgba: u32, cx: f32, cy: f32, r: f32) -> Self {
        Self {
            id,
            depth,
            flags: cell_kind::CIRCLE,
            rgba,
            g: [cx, cy, r, 0.0, 0.0],
        }
    }

    /// Mind-map dot constructor (link target = parent position).
    #[must_use]
    #[allow(clippy::too_many_arguments)] // geometry constructor — pairs with the 5-f32 cell payload
    pub fn dot(id: u32, depth: u16, rgba: u32, x: f32, y: f32, r: f32, px: f32, py: f32) -> Self {
        Self {
            id,
            depth,
            flags: cell_kind::DOT,
            rgba,
            g: [x, y, r, px, py],
        }
    }

    /// Treemap header strip constructor.
    #[must_use]
    pub fn header(id: u32, depth: u16, rgba: u32, x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            id,
            depth,
            flags: cell_kind::HEADER,
            rgba,
            g: [x, y, w, h, 0.0],
        }
    }
}

/// How tree items are colored (spec §7 "Color modes").
///
/// Wire format: kebab-case ("by-folder" / "by-type" / "by-age") — the
/// JS contract (`state/vizUi` `ColorMode` + the mock backend). Without the
/// rename, serde demanded the Rust variant names (`"ByFolder"`…) and EVERY
/// `get_layout` invoke failed with "unknown variant 'by-folder'" — the
/// CI canvas frames were blank for exactly this reason (surfaced by the
/// inline viz error in FIX-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ColorMode {
    /// Pastel family per top-level branch.
    ByFolder,
    /// Dominant category color, view regrouped per category.
    ByType,
    /// Age-bucket colors, view regrouped per bucket.
    ByAge,
}

/// Layout result: cells + JSON metadata (group descriptions, totals).
#[derive(Debug, Clone)]
pub struct LayoutBuffer {
    /// The cells (≤ [`MAX_CELLS`]).
    pub cells: Vec<Cell>,
    /// JSON metadata payload (see `LayoutMeta`).
    pub meta: LayoutMeta,
}

impl LayoutBuffer {
    /// Serialize cells to the raw little-endian binary body (IPC contract).
    #[must_use]
    pub fn cells_to_bytes(&self) -> Vec<u8> {
        // Safe field-wise serialization (no unsafe transmute needed; the
        // buffer is small and produced once per layout, then cached).
        let mut out = Vec::with_capacity(self.cells.len() * 32);
        for c in &self.cells {
            out.extend_from_slice(&c.id.to_le_bytes());
            out.extend_from_slice(&c.depth.to_le_bytes());
            out.extend_from_slice(&c.flags.to_le_bytes());
            out.extend_from_slice(&c.rgba.to_le_bytes());
            for f in c.g {
                out.extend_from_slice(&f.to_le_bytes());
            }
        }
        out
    }

    /// Serialize the per-cell on-disk sizes — one little-endian `u64` per
    /// cell, in the same order as [`LayoutBuffer::cells_to_bytes`]. The
    /// app's frame appends this tail after the cells block and the JS twin
    /// (`decodeLayout`) reads it for the two-line "name / size" cell labels
    /// (no extra IPC round trip for label sizes).
    ///
    /// Real node ids resolve through the tree's `on_disk`; synthetic group
    /// ids (`SYNTH_BASE + n`, emitted by the regroup engines and the group
    /// header strips) resolve through `meta.groups`.
    #[must_use]
    pub fn sizes_to_bytes(&self, tree: &Tree) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.cells.len() * 8);
        for c in &self.cells {
            let size = if c.id >= regroup::SYNTH_BASE {
                self.meta
                    .groups
                    .iter()
                    .find(|g| g.id == c.id)
                    .map_or(0, |g| g.size)
            } else {
                tree.node(c.id).map_or(0, |n| n.on_disk)
            };
            out.extend_from_slice(&size.to_le_bytes());
        }
        out
    }
}

/// Layout metadata returned as JSON alongside the binary cells.
///
/// Wire format: camelCase keys — the JS `LayoutMeta` contract
/// (`layoutIpc.ts`) and the mock backend's encoder. The core tests assert
/// the exact JSON keys so a future rename cannot silently drift the
/// binary IPC contract again.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutMeta {
    /// Layout mode that produced this buffer.
    pub mode: String,
    /// Scan generation (the UI drops stale results).
    pub generation: u64,
    /// Folder node the layout was computed for.
    pub node: u32,
    /// Viewport width in CSS px.
    pub width: f32,
    /// Viewport height in CSS px.
    pub height: f32,
    /// Depth setting used.
    pub depth: u32,
    /// Color mode used.
    pub color_mode: ColorMode,
    /// Number of cells emitted.
    pub cell_count: u32,
    /// True when the cell budget forced skipping smaller items.
    pub truncated: bool,
    /// Sunburst center `(cx, cy)` (other modes ignore it).
    pub center: Option<(f32, f32)>,
    /// Synthetic group legend (regroup modes; empty otherwise).
    pub groups: Vec<GroupDesc>,
    /// Total on-disk bytes of the laid-out subtree (100% reference).
    pub total_bytes: u64,
}

/// A synthetic group description (regroup modes, spec §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDesc {
    /// Synthetic id `0xFFFF_0000 + n`.
    pub id: u32,
    /// Display name ("Video", "Last 7 days", …).
    pub name: String,
    /// `0xRRGGBB`.
    pub color: u32,
    /// Group size on disk.
    pub size: u64,
}

/// Pack a color as `0xRRGGBBAA` with full alpha.
#[must_use]
pub fn pack_rgba(rgb: u32) -> u32 {
    (rgb << 8) | 0xFF
}

/// The neutral anchor/gray tone for root cells, hub dots and group
/// headers (spelled as one constant — it was a magic literal at six
/// sites across four engines).
pub(crate) const ANCHOR_GRAY: u32 = 0x8E8E93;

/// Cell-budget check shared by EVERY emit loop (hoisted from groups.rs
/// where it lived private next to five hand-copied inline twins).
#[must_use]
pub(crate) fn over_budget(cells: &[Cell], truncated: &mut bool) -> bool {
    if cells.len() >= MAX_CELLS {
        *truncated = true;
        return true;
    }
    false
}

/// By-folder family assignment (the effective-branch-root rule shared
/// by all five engines): a node AT the branch root takes its sibling
/// index as a new family; every deeper node inherits its parent's.
#[must_use]
pub(crate) fn family_of(
    parent: u32,
    branch_root: u32,
    sibling_index: usize,
    inherited: usize,
) -> usize {
    if parent == branch_root {
        sibling_index
    } else {
        inherited
    }
}

/// Live, sizeable children of `parent` (removed and zero-`on_disk`
/// filtered) plus their total — the enumeration every engine starts
/// its layout pass with (was five hand-rolled variants; treemap's also
/// allocated three intermediate vecs per directory).
#[must_use]
pub(crate) fn sizeable_children(tree: &Tree, parent: u32) -> (Vec<u32>, u64) {
    let children = tree.children_sorted(parent);
    let mut ids = Vec::with_capacity(children.len());
    let mut total = 0u64;
    for &id in children {
        if let Some(n) = tree.node(id) {
            if !n.is_removed() && n.on_disk > 0 {
                ids.push(id);
                total = total.saturating_add(n.on_disk);
            }
        }
    }
    (ids, total)
}

/// Age bucket colors (spec §7 By age), bucket index 0..=5.
#[must_use]
pub const fn age_bucket_color(bucket: usize) -> u32 {
    const COLORS: [u32; 6] = [0x34D399, 0x60A5FA, 0x818CF8, 0xA78BFA, 0xF472B6, 0xF87171];
    // `Ord::min` is not const-callable; an if/else is (and clamps > 5 to 5).
    COLORS[if bucket > 5 { 5 } else { bucket }]
}

/// Legend chips for by-folder views: the effective branch root's
/// sizeable children (the pastel family level), size-desc — the JS
/// renders `meta.groups` as the bottom-of-canvas legend. The engines
/// only fill groups for the regroup (by-type/by-age) modes, so
/// by-folder legends existed in the dev mock but never in production.
#[must_use]
pub fn folder_legend(tree: &Tree, node: u32) -> Vec<GroupDesc> {
    let branch_root = effective_branch_root(tree, node);
    let mut out: Vec<GroupDesc> = Vec::new();
    for &id in tree.children_sorted(branch_root) {
        if out.len() >= 8 {
            break;
        }
        let Some(n) = tree.node(id) else { continue };
        if n.on_disk == 0 || n.is_removed() {
            continue;
        }
        let fam = out.len();
        out.push(GroupDesc {
            id: crate::layout::regroup::SYNTH_BASE + fam as u32,
            name: tree.name(id),
            color: folder_family_color_by_name(&tree.name(id), 0, 0),
            size: n.on_disk,
        });
    }
    out
}

/// Pastel families for By-folder coloring (spec §7): blue, teal, violet,
/// amber, rose, green, sky, slate — shades vary by depth+index.
///
/// Kept for back-compat with any external caller still passing an index;
/// new code should call [`folder_family_color_by_name`] which is stable
/// across machines and matches the reference design where Users=blue,
/// Applications=mint, Library=yellow on EVERY scan, not just the dev mock.
#[must_use]
pub fn folder_family_color(top_index: usize, depth: u16, index: usize) -> u32 {
    const FAMILIES: [(u8, u8, u8); 8] = [
        (147, 197, 253), // blue
        (153, 246, 228), // teal
        (196, 181, 253), // violet
        (253, 230, 138), // amber
        (253, 205, 211), // rose
        (187, 247, 208), // green
        (186, 230, 253), // sky
        (203, 213, 225), // slate
    ];
    let (r, g, b) = FAMILIES[top_index % FAMILIES.len()];
    // Shade: darken slightly with depth and vary by sibling index so
    // neighbors separate (stays inside the pastel family).
    let shade =
        1.0 - f32::from((depth.min(4)) as u8) * 0.06 - if index % 2 == 1 { 0.05 } else { 0.0 };
    let r = (f32::from(r) * shade).clamp(0.0, 255.0) as u32;
    let g = (f32::from(g) * shade).clamp(0.0, 255.0) as u32;
    let b = (f32::from(b) * shade).clamp(0.0, 255.0) as u32;
    (r << 16) | (g << 8) | b
}

/// Name-based pastel family color (the reference design: stable
/// hue-per-directory across machines and scans, not by sibling index).
///
/// The reference analysis pinned these mappings from the DaisyDisk-class
/// target:
/// - `Users` → soft blue (`#D6E4FF`)
/// - `Applications` / `Program Files` / `Program Files (x86)` → mint (`#CCFBF1`)
/// - `Library` / `ProgramData` → soft yellow (`#FEF9C3`)
/// - `private` / `var` → soft purple (`#E9D5FF`)
/// - `usr` / `bin` / `sbin` / `etc` → soft pink (`#FBCFE8`)
/// - `Windows` / `System Volume Information` → cool slate (`#E2E8F0`)
/// - `Downloads` → soft amber (`#FEF3C7`)
/// - `Documents` → soft sky (`#DBEAFE`-tier)
/// - `Desktop` → soft green (`#D1FAE5`)
///
/// Unknown names fall back to a stable FNV-1a hash of the lowercased
/// name bytes, mod the 8-color pastel palette — so a given directory
/// name always maps to the SAME color on every machine, but two
/// different unknown directories get different colors. The shade
/// varies by depth and sibling index so neighbors separate (same
/// behavior as the legacy index-based `folder_family_color`).
#[must_use]
pub fn folder_family_color_by_name(name: &str, depth: u16, index: usize) -> u32 {
    // Curated palette mirroring the reference (slightly more saturated
    // than the legacy families so the gradient stops in JS read cleanly
    // against the dark theme too).
    const PALETTE: [(u8, u8, u8); 8] = [
        (214, 228, 255), // blue   (Users)
        (204, 251, 241), // mint   (Applications)
        (254, 249, 195), // yellow (Library)
        (233, 213, 255), // purple (private)
        (251, 207, 232), // pink   (usr)
        (226, 232, 240), // slate  (Windows)
        (254, 243, 199), // amber  (Downloads)
        (209, 250, 229), // green  (Desktop)
    ];
    let key = name.trim_end_matches(['/', '\\']).to_ascii_lowercase();
    let idx = match key.as_str() {
        // macOS / Linux homes + system roots.
        "users" | "home" | "users/admin" | "users/shared" => 0,
        "applications" | "program files" | "program files (x86)" => 1,
        "library" | "programdata" => 2,
        "private" | "private/var" | "var" | "var/db" | "var/log" => 3,
        "usr" | "usr/local" | "bin" | "sbin" | "etc" | "opt" => 4,
        "windows" | "system volume information" | "system32" | "syswow64" | "winsxs" => 5,
        "downloads" | "documents" | "documents and settings" => 6,
        "desktop" | "music" | "itunes" => 7,
        "pictures" | "photos" | "images" => 4, // pink
        "videos" | "movies" => 5,              // slate
        "$recycle.bin" | "trash" | ".trash" => 4,
        // Stable hash for unknown names.
        _ => stable_name_hash(&key) as usize % PALETTE.len(),
    };
    let (r, g, b) = PALETTE[idx];
    let shade =
        1.0 - f32::from(depth.min(4) as u8) * 0.06 - if index % 2 == 1 { 0.05 } else { 0.0 };
    let r = (f32::from(r) * shade).clamp(0.0, 255.0) as u32;
    let g = (f32::from(g) * shade).clamp(0.0, 255.0) as u32;
    let b = (f32::from(b) * shade).clamp(0.0, 255.0) as u32;
    (r << 16) | (g << 8) | b
}

/// FNV-1a 32-bit hash of a name's lowercased bytes — stable across
/// machines, scans, and runs. Used by [`folder_family_color_by_name`]
/// to pick a pastel family for unknown directory names so the same
/// directory on two machines lands on the same color.
#[must_use]
pub fn stable_name_hash(name: &str) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for b in name.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Walk up from `id` to find its top-level branch — the ancestor
/// whose parent is `branch_root`. Returns `id` unchanged when `id`
/// is itself at depth 1 below `branch_root` (or `branch_root`
/// itself). The walk terminates because parent ids always precede
/// child ids in the arena.
#[must_use]
pub(crate) fn top_level_branch_id(tree: &Tree, id: u32, branch_root: u32) -> u32 {
    if id == branch_root {
        return id;
    }
    let mut cur = id;
    for _ in 0..64 {
        let parent = tree.node(cur).map_or(u32::MAX, |n| n.parent);
        if parent == u32::MAX || parent == branch_root {
            return cur;
        }
        cur = parent;
    }
    cur
}

/// Resolve the cell color for a real node under the given color mode.
///
/// `top_index` is the legacy by-folder family index — still threaded
/// by the layout engines for back-compat, but ignored in `ByFolder`
/// mode (the family color is now name-based and resolved from
/// `branch_root` + `id` via [`top_level_branch_id`]).
#[must_use]
pub fn node_color(
    tree: &Tree,
    id: u32,
    mode: ColorMode,
    now: i64,
    branch_root: u32,
    depth: u16,
    index: usize,
) -> u32 {
    match mode {
        ColorMode::ByFolder => {
            let family_id = top_level_branch_id(tree, id, branch_root);
            let fam_name = tree.name(family_id);
            folder_family_color_by_name(&fam_name, depth, index)
        }
        ColorMode::ByType => {
            let cat = tree.dominant_category(id);
            cat.color()
        }
        ColorMode::ByAge => age_bucket_color(crate::age::bucket_of(
            tree.node(id).map_or(0, |n| n.modified),
            now,
        )),
    }
}

/// Descend from `root` through single-sizeable-child chains to the first
/// node whose sizeable children actually branch (≥ 2). Its children form
/// the effective top-level branches for by-folder family coloring (e.g.
/// "This PC" → "C:" → {Users, Windows, …} assigns families at C:'s children).
///
/// Returns `root` unchanged when it already branches (or cannot descend
/// further), so a normal multi-folder scan keeps assigning families at the
/// layout root's own children. The iteration cap only guards pathological
/// arenas — parent ids always precede child ids, so chains cannot cycle.
#[must_use]
pub fn effective_branch_root(tree: &Tree, root: u32) -> u32 {
    let mut cur = root;
    for _ in 0..64 {
        // Count sizeable (on-disk > 0) children; remember the only one.
        let mut sizeable = 0usize;
        let mut only = cur;
        for &id in tree.children_sorted(cur) {
            if tree.node(id).map_or(0, |n| n.on_disk) > 0 {
                sizeable += 1;
                only = id;
                if sizeable > 1 {
                    break; // already branching — no descent needed
                }
            }
        }
        if sizeable != 1 {
            return cur;
        }
        let descend = tree
            .node(only)
            .is_some_and(|n| n.is_dir() && n.child_count > 0);
        if !descend {
            return cur;
        }
        cur = only;
    }
    cur
}

/// Depth of `node` below `root` (0 when equal). Walks parents; arena ids
/// always shrink toward the root, so the walk terminates. Used to locate
/// the effective branch level for the tiered bubble/mind-map alpha.
pub(crate) fn depth_below(tree: &Tree, node: u32, root: u32) -> u32 {
    let mut d = 0u32;
    let mut cur = node;
    while cur != root {
        let p = tree.node(cur).map_or(u32::MAX, |n| n.parent);
        if p == u32::MAX {
            break;
        }
        cur = p;
        d += 1;
    }
    d
}

/// Levels the layout will actually draw below `node`, bounded by
/// `limit`: one level per sizeable-child level (dirs AND files — file
/// cells occupy a level too), recursing only through dirs. Layouts that
/// budget geometry per level (flame rows, mind-map rings) divide by the
/// REACHABLE depth, not the requested one — a depth-7 request over a
/// 3-level tree otherwise reserves 7 thin rings and the drawing fills
/// only the inner ~40% of the canvas.
pub(crate) fn reachable_levels(tree: &Tree, node: u32, limit: u32) -> u32 {
    if limit == 0 {
        return 0;
    }
    let children = tree.children_sorted(node);
    let any_sizeable = children
        .iter()
        .any(|&id| tree.node(id).is_some_and(|c| c.on_disk > 0));
    if !any_sizeable {
        return 0;
    }
    let mut best: u32 = 0;
    for &id in children {
        if let Some(c) = tree.node(id) {
            if c.is_dir() && c.on_disk > 0 {
                best = best.max(reachable_levels(tree, id, limit - 1));
            }
        }
    }
    1 + best
}

/// Validate geometry before layout math (doc 04 §4 "assert bounds early").
pub(crate) fn check_geometry(width: f32, height: f32) -> Result<(), CoreError> {
    if width <= 0.0 || height <= 0.0 || !width.is_finite() || !height.is_finite() {
        return Err(CoreError::InvalidGeometry {
            width: width.max(0.0) as u32,
            height: height.max(0.0) as u32,
        });
    }
    Ok(())
}

/// The 6px² skip threshold for treemap cells (spec §7).
const MIN_CELL_AREA: f32 = 6.0;

/// Whether a rect is large enough to emit (spec §7 treemap rule).
#[must_use]
pub(crate) fn rect_visible(w: f32, h: f32) -> bool {
    w > 0.0 && h > 0.0 && w * h >= MIN_CELL_AREA
}

#[cfg(test)]
mod tests {
    use super::*;

    // folder_legend: the by-folder legend mirrors the branch-root family
    // level (the pastel chips the JS renders under the canvas).
    #[test]
    fn folder_legend_lists_branch_root_children_with_family_colors() {
        use crate::scan::node::{BatchEntry, Node, Tree};
        use crate::scan::rollup;
        fn dir(name: &str) -> BatchEntry {
            let mut node = Node::new_dir();
            node.modified = 1;
            BatchEntry {
                name: name.encode_utf16().collect(),
                node,
            }
        }
        fn file(name: &str, on_disk: u64) -> BatchEntry {
            let mut node = Node::new_file();
            node.logical = on_disk;
            node.on_disk = on_disk;
            node.modified = 1;
            BatchEntry {
                name: name.encode_utf16().collect(),
                node,
            }
        }
        // This PC -> C: -> three sized folders (the branch level).
        let mut t = Tree::new(1);
        t.add_root_path(0, "This PC");
        t.append_batch(0, vec![dir("C:")]); // 1
        t.append_batch(1, vec![dir("a"), dir("b"), dir("c")]); // 2, 3, 4
        t.append_batch(2, vec![file("f1", 300)]);
        t.append_batch(3, vec![file("f2", 200)]);
        t.append_batch(4, vec![file("f3", 100)]);
        rollup::finalize(&mut t);

        let legend = folder_legend(&t, 0);
        assert_eq!(legend.len(), 3, "branch-root children a,b,c");
        // Size-desc order with distinct pastel family base colors.
        assert_eq!(legend[0].name, "a");
        assert_eq!(legend[1].name, "b");
        assert_eq!(legend[2].name, "c");
        assert_eq!(legend[0].size, 300);
        assert!(legend[0].color != legend[1].color && legend[1].color != legend[2].color);
        // Synthetic ids above SYNTH_BASE (describe buckets, not nodes).
        assert!(legend
            .iter()
            .all(|g| g.id >= crate::layout::regroup::SYNTH_BASE));
    }

    /// Field-wise little-endian bytes of a cell (test twin of
    /// `LayoutBuffer::cells_to_bytes`).
    fn cell_to_le_bytes(c: &Cell) -> [u8; 32] {
        let mut out = [0u8; 32];
        let mut i = 0;
        out[i..i + 4].copy_from_slice(&c.id.to_le_bytes());
        i += 4;
        out[i..i + 2].copy_from_slice(&c.depth.to_le_bytes());
        i += 2;
        out[i..i + 2].copy_from_slice(&c.flags.to_le_bytes());
        i += 2;
        out[i..i + 4].copy_from_slice(&c.rgba.to_le_bytes());
        i += 4;
        for f in c.g {
            out[i..i + 4].copy_from_slice(&f.to_le_bytes());
            i += 4;
        }
        out
    }

    #[test]
    fn wire_json_keys_match_the_js_contract() {
        // The binary IPC contract: `LayoutMeta` serializes CAMELCASE and
        // `ColorMode` KEBAB — the JS `LayoutMeta`/`ColorMode` types
        // (layoutIpc.ts) + the mock's encoder. These exact keys once
        // drifted (`cell_count`, `"ByFolder"`) and every canvas layout
        // failed silently (CI blank treemap) — the keys are now pinned.
        let meta = LayoutMeta {
            mode: "treemap".into(),
            generation: 7,
            node: 3,
            width: 800.0,
            height: 600.0,
            depth: 4,
            color_mode: ColorMode::ByFolder,
            cell_count: 12,
            truncated: false,
            center: None,
            groups: vec![GroupDesc {
                id: 1,
                name: "g".into(),
                color: 0x112233,
                size: 5,
            }],
            total_bytes: 4096,
        };
        let json = serde_json::to_value(&meta).expect("meta serializes");
        for key in [
            "mode",
            "generation",
            "node",
            "width",
            "height",
            "depth",
            "colorMode",
            "cellCount",
            "truncated",
            "center",
            "groups",
            "totalBytes",
        ] {
            assert!(json.get(key).is_some(), "meta JSON must carry `{key}`");
        }
        assert_eq!(json["colorMode"], serde_json::json!("by-folder"));
        // The request side: kebab variants must round-trip.
        for (wire, variant) in [
            ("\"by-folder\"", ColorMode::ByFolder),
            ("\"by-type\"", ColorMode::ByType),
            ("\"by-age\"", ColorMode::ByAge),
        ] {
            let parsed: ColorMode = serde_json::from_str(wire).expect("kebab parses");
            assert_eq!(parsed, variant);
        }
        assert!(serde_json::from_str::<ColorMode>("\"ByFolder\"").is_err());
    }

    #[test]
    fn cell_is_32_bytes() {
        assert_eq!(size_of::<Cell>(), 32);
        let c = Cell::rect(1, 2, 0xFF0000FF, 1.0, 2.0, 3.0, 4.0);
        // Bit patterns: constructor values round-trip exactly.
        assert_eq!(c.g[0].to_bits(), 1.0f32.to_bits());
        assert_eq!(c.g[3].to_bits(), 4.0f32.to_bits());
    }

    #[test]
    fn cell_serialization_roundtrip_field_wise() {
        let c = Cell::arc(0x1234, 3, 0xAABBCCDD, 0.1, 1.2, 5.0, 6.0);
        let bytes = cell_to_le_bytes(&c);
        let buf = LayoutBuffer {
            cells: vec![c],
            meta: LayoutMeta {
                mode: "sunburst".into(),
                generation: 1,
                node: 0,
                width: 100.0,
                height: 100.0,
                depth: 5,
                color_mode: ColorMode::ByType,
                cell_count: 1,
                truncated: false,
                center: Some((50.0, 50.0)),
                groups: vec![],
                total_bytes: 42,
            },
        };
        assert_eq!(buf.cells_to_bytes().len(), 32);
        assert_eq!(bytes[0], 0x34);
        assert_eq!(bytes[1], 0x12);
    }

    #[test]
    fn colors_pack() {
        assert_eq!(pack_rgba(0xFF6B4A), 0xFF6B4AFF);
        assert_eq!(age_bucket_color(0), 0x34D399);
        assert_eq!(age_bucket_color(5), 0xF87171);
        assert_eq!(age_bucket_color(99), 0xF87171); // clamped
    }

    #[test]
    fn sizes_tail_resolves_real_and_synthetic_ids() {
        // Wire contract for the two-line cell labels: one LE u64 per cell
        // in cell order — real ids from the tree's on_disk, synthetic
        // group ids (headers/regroup cells) from meta.groups, unknowns 0.
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\T");
        let file = |name: &str, on_disk: u64| crate::scan::node::BatchEntry {
            name: name.encode_utf16().collect(),
            node: {
                let mut n = crate::scan::node::Node::new_file();
                n.logical = on_disk;
                n.on_disk = on_disk;
                n
            },
        };
        t.append_batch(0, vec![file("z1.bin", 100), file("z2.bin", 50)]);
        // ids: root 0, z1 1, z2 2.

        let synth = regroup::SYNTH_BASE + 1;
        let buf = LayoutBuffer {
            cells: vec![
                Cell::rect(1, 1, 0xFF, 0.0, 0.0, 10.0, 10.0), // real file → 100
                Cell::header(synth, 0, 0xFF, 0.0, 0.0, 10.0, 4.0), // group → 777
                Cell::rect(9, 1, 0xFF, 0.0, 0.0, 10.0, 10.0), // unknown → 0
            ],
            meta: LayoutMeta {
                mode: "treemap-groups".into(),
                generation: 1,
                node: 0,
                width: 100.0,
                height: 100.0,
                depth: 5,
                color_mode: ColorMode::ByType,
                cell_count: 3,
                truncated: false,
                center: None,
                groups: vec![GroupDesc {
                    id: synth,
                    name: "Video".into(),
                    color: 0x93C5FD,
                    size: 777,
                }],
                total_bytes: 150,
            },
        };
        let sizes = buf.sizes_to_bytes(&t);
        assert_eq!(sizes.len(), 24, "one u64 per cell");
        let read = |i: usize| u64::from_le_bytes(sizes[i * 8..(i + 1) * 8].try_into().unwrap());
        assert_eq!(read(0), 100, "real node id resolves to on_disk");
        assert_eq!(read(1), 777, "synthetic group id resolves via meta.groups");
        assert_eq!(read(2), 0, "unknown id resolves to 0 (never panics)");
    }

    // ── Name-based folder family color tests ──────────────────────────
    // The reference design (upload/extracted/refrence/ANALYSIS.md) pins
    // stable hue-per-directory: Users=blue, Applications=mint,
    // Library=yellow on EVERY scan. The legacy index-based
    // folder_family_color broke this when sibling order shifted on
    // real disks. folder_family_color_by_name is the fix.

    #[test]
    fn folder_family_color_by_name_well_known_mappings() {
        // Users → blue (index 0 in the PALETTE).
        let users = folder_family_color_by_name("Users", 0, 0);
        let home = folder_family_color_by_name("home", 0, 0);
        assert_eq!(
            users, home,
            "Users and home both map to blue (the reference Users=blue)"
        );

        // Applications → mint (index 1).
        let apps = folder_family_color_by_name("Applications", 0, 0);
        let pf = folder_family_color_by_name("Program Files", 0, 0);
        let pfx86 = folder_family_color_by_name("Program Files (x86)", 0, 0);
        assert_eq!(apps, pf, "Program Files maps to mint like Applications");
        assert_eq!(apps, pfx86, "Program Files (x86) maps to mint too");

        // Library → yellow (index 2).
        let lib = folder_family_color_by_name("Library", 0, 0);
        let progdata = folder_family_color_by_name("ProgramData", 0, 0);
        assert_eq!(lib, progdata, "ProgramData maps to yellow like Library");

        // Windows → slate (index 5).
        let win = folder_family_color_by_name("Windows", 0, 0);
        let svi = folder_family_color_by_name("System Volume Information", 0, 0);
        assert_eq!(win, svi, "System Volume Information maps to slate like Windows");

        // Downloads → amber (index 6).
        let dl = folder_family_color_by_name("Downloads", 0, 0);
        let docs = folder_family_color_by_name("Documents", 0, 0);
        assert_eq!(dl, docs, "Documents maps to amber like Downloads");

        // Desktop → green (index 7).
        let desk = folder_family_color_by_name("Desktop", 0, 0);
        let _music = folder_family_color_by_name("Music", 0, 0);
        assert_eq!(
            desk,
            folder_family_color_by_name("Music", 0, 0),
            "Music maps to green like Desktop"
        );
    }

    #[test]
    fn folder_family_color_by_name_case_insensitive_and_trimmed() {
        // Case-insensitive: "USERS" == "users" == "Users".
        let a = folder_family_color_by_name("Users", 0, 0);
        let b = folder_family_color_by_name("USERS", 0, 0);
        let c = folder_family_color_by_name("users", 0, 0);
        assert_eq!(a, b, "USERS and Users map to the same color");
        assert_eq!(a, c, "users and Users map to the same color");

        // Trailing path separators stripped.
        let with_sep = folder_family_color_by_name("Users/", 0, 0);
        let with_bsep = folder_family_color_by_name("Users\\", 0, 0);
        assert_eq!(a, with_sep, "Users/ == Users");
        assert_eq!(a, with_bsep, "Users\\ == Users");
    }

    #[test]
    fn folder_family_color_by_name_stable_across_calls() {
        // The SAME name MUST return the SAME color every call (the
        // legacy index-based version returned DIFFERENT colors when
        // the sibling order shifted between scans).
        let name = "Users";
        let c1 = folder_family_color_by_name(name, 0, 0);
        let c2 = folder_family_color_by_name(name, 0, 0);
        let c3 = folder_family_color_by_name(name, 3, 7);
        // Same name + same depth + same index → same color.
        assert_eq!(c1, c2, "deterministic per (name, depth, index)");
        // Different depth/index → different SHADE but same FAMILY.
        // The shade formula darkens by depth*0.06 + index_odd*0.05.
        // So c3 (depth 3, index 7) should be darker than c1 (depth 0, index 0).
        // Both should be in the blue family though.
        let r1 = ((c1 >> 16) & 0xFF) as u32;
        let r3 = ((c3 >> 16) & 0xFF) as u32;
        assert!(r3 <= r1, "deeper depth darkens the shade");
    }

    #[test]
    fn folder_family_color_by_name_unknown_uses_stable_hash() {
        // Unknown names fall back to a stable FNV-1a hash mod 8 —
        // the SAME name always maps to the SAME family on every
        // machine, but two DIFFERENT unknown names get different
        // families (with high probability).
        let custom1 = folder_family_color_by_name("MyProjectX", 0, 0);
        let custom2 = folder_family_color_by_name("MyProjectX", 0, 0);
        assert_eq!(custom1, custom2, "same unknown name → same color");

        let other = folder_family_color_by_name("DifferentName", 0, 0);
        // Two different unknown names should (very likely) map to
        // different families. We can't assert inequality for ALL
        // pairs (collision possible), but MyProjectX vs DifferentName
        // hash to different mod-8 buckets.
        if stable_name_hash("myprojectx") % 8 != stable_name_hash("differentname") % 8 {
            assert_ne!(custom1, other, "different unknown names → different colors");
        }
    }

    #[test]
    fn stable_name_hash_is_deterministic_and_fnv1a() {
        // FNV-1a 32-bit: 0x811C9DC5 offset, 0x0100_0193 prime.
        // Empty string → offset basis.
        assert_eq!(stable_name_hash(""), 0x811C_9DC5);
        // "a" → (offset ^ 'a') * prime = (0x811C9DC5 ^ 0x61) * 0x01000193
        let expected = 0x811C_9DC5 ^ u32::from(b'a');
        let expected = expected.wrapping_mul(0x0100_0193);
        assert_eq!(stable_name_hash("a"), expected);
        // Deterministic.
        assert_eq!(stable_name_hash("Users"), stable_name_hash("Users"));
    }

    #[test]
    fn top_level_branch_id_walks_up_to_branch_root_child() {
        use crate::scan::node::{BatchEntry, Node, Tree};
        use crate::scan::rollup;
        fn dir(name: &str) -> BatchEntry {
            let mut node = Node::new_dir();
            node.modified = 1;
            BatchEntry {
                name: name.encode_utf16().collect(),
                node,
            }
        }
        fn file(name: &str, on_disk: u64) -> BatchEntry {
            let mut node = Node::new_file();
            node.logical = on_disk;
            node.on_disk = on_disk;
            node.modified = 1;
            BatchEntry {
                name: name.encode_utf16().collect(),
                node,
            }
        }
        // root(0) → C:(1) → Users(2) → admin(3) → Documents(4) → file(5)
        let mut t = Tree::new(1);
        t.add_root_path(0, "This PC");
        t.append_batch(0, vec![dir("C:")]); // 1
        t.append_batch(1, vec![dir("Users")]); // 2
        t.append_batch(2, vec![dir("admin")]); // 3
        t.append_batch(3, vec![dir("Documents")]); // 4
        t.append_batch(4, vec![file("resume.pdf", 500)]); // 5
        rollup::finalize(&mut t);

        // branch_root = 1 (C: is the effective branch root — single
        // sizeable child of "This PC"). The top-level branch of node 5
        // (resume.pdf) is node 2 (Users), because Users's parent is C:
        // (the branch_root).
        assert_eq!(top_level_branch_id(&t, 5, 1), 2, "walks up to Users");
        assert_eq!(top_level_branch_id(&t, 4, 1), 2, "Documents → Users");
        assert_eq!(top_level_branch_id(&t, 3, 1), 2, "admin → Users");
        assert_eq!(top_level_branch_id(&t, 2, 1), 2, "Users is itself the branch");
        assert_eq!(top_level_branch_id(&t, 1, 1), 1, "C: == branch_root → itself");
        assert_eq!(top_level_branch_id(&t, 0, 0), 0, "root with branch_root=0 → itself");
    }

    #[test]
    fn node_color_by_folder_resolves_family_name() {
        use crate::scan::node::{BatchEntry, Node, Tree};
        use crate::scan::rollup;
        fn dir(name: &str) -> BatchEntry {
            let mut node = Node::new_dir();
            node.modified = 1;
            BatchEntry {
                name: name.encode_utf16().collect(),
                node,
            }
        }
        fn file(name: &str, on_disk: u64) -> BatchEntry {
            let mut node = Node::new_file();
            node.logical = on_disk;
            node.on_disk = on_disk;
            node.modified = 1;
            BatchEntry {
                name: name.encode_utf16().collect(),
                node,
            }
        }
        // root(0) → C:(1) → Users(2) → admin(3) → resume.pdf(4)
        let mut t = Tree::new(1);
        t.add_root_path(0, "This PC");
        t.append_batch(0, vec![dir("C:")]);
        t.append_batch(1, vec![dir("Users")]);
        t.append_batch(2, vec![dir("admin")]);
        t.append_batch(3, vec![file("resume.pdf", 500)]);
        rollup::finalize(&mut t);

        let now = 1_800_000_000;
        let branch_root = 1; // C:
        // The resume.pdf file's family is "Users" (its top-level
        // branch). node_color should resolve it via name and return
        // the BLUE family color — the SAME color as a direct call
        // to folder_family_color_by_name("Users", ...).
        let color_deep = node_color(&t, 4, ColorMode::ByFolder, now, branch_root, 3, 0);
        let color_users_direct = folder_family_color_by_name("Users", 3, 0);
        assert_eq!(
            color_deep, color_users_direct,
            "deep descendant of Users gets Users' family color (name-based, not index-based)"
        );
    }
}
