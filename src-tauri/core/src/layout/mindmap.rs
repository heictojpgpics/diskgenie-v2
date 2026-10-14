//! Mind Map (spec §7 mode 6): "Branches from the root, sized by weight".
//!
//! A radial tree with curved links (JS draws a quadratic curve from each
//! dot to its parent dot position, stored in the cell); dot area ∝ share
//! of the folder. Labels are placed JS-side, biggest-first, skipping
//! collisions.

use crate::error::CoreError;
use crate::layout::{
    check_geometry, depth_below, node_color, pack_rgba, Cell, ColorMode, LayoutBuffer, LayoutMeta,
};
use crate::scan::node::Tree;

/// Minimum dot radius.
pub(crate) const MIN_R: f32 = 1.5;
/// Base dot radius at the root's children (scales with viewport).
pub(crate) const DOT_BASE: f32 = 26.0;
/// Root hub dot radius — sized for the JS label (root name + total
/// bytes rendered inside the disc, sunburst-center style).
pub(crate) const ROOT_DOT_R: f32 = 30.0;
/// A child this dominant (≥ 72% of its level) continues the SPINE:
/// the hub's story unrolls through it ("This PC" → C: → …) and its
/// children share the parent's ring — the map only gives sectors to
/// levels that actually BRANCH. Below the threshold the child takes a
/// normal sector like its siblings.
const SPINE_SHARE: f32 = 0.72;
/// Alpha for top-level dots — the spine chain plus the first
/// branching level: solid, matching the reference's bold branch dots.
const ALPHA_TOP: u32 = 0xFF;
/// Alpha for nested child dots (slight translucency so the hierarchy
/// reads and links/labels stay legible).
const ALPHA_NESTED: u32 = 0xCC;

/// Layout the subtree under `node` as a radial mind map.
///
/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
/// - [`CoreError::NodeNotFound`] when `node` is not in the arena.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)] // hub + spine bookkeeping + scale + re-seat passes read best together
pub fn mindmap(
    tree: &Tree,
    node: u32,
    width: f32,
    height: f32,
    depth: u32,
    color: ColorMode,
    now: i64,
) -> Result<LayoutBuffer, CoreError> {
    check_geometry(width, height)?;
    let n = tree.node(node).ok_or(CoreError::NodeNotFound(node))?;
    let total = n.on_disk;
    let cx = width / 2.0;
    let cy = height / 2.0;
    // ADAPTIVE ring budget: `rings` = the levels that actually SPREAD
    // (a level with ≥ 2 sizeable children consumes one ring; a
    // single-child chain level consumes a LEVEL but no ring — its dot
    // collapses onto the parent's position). A depth-7 request over a
    // 3-level tree used to divide r_max by 7 and fill only the inner
    // ~40% of the canvas (the "tiny, off-center mind map" finding);
    // counting CHAIN levels as ring consumers had the same effect.
    // The depth setting stays the hard descent ceiling.
    let rings = if depth == 0 {
        0
    } else {
        spread_rings(tree, node, depth)
    };
    // Reserve the largest possible dot + air so dots and their labels
    // never clip the canvas edge (the deepest ring sits AT r_max; with
    // only a 6 px margin, 20-26 px dots at the 12-o'clock start angle
    // rendered half-off-canvas — VLM audit: "labels clipped by the
    // container"). Floor keeps tiny windows usable.
    let r_max = (width.min(height) / 2.0 - DOT_BASE - 8.0).max(48.0);
    let mut cells: Vec<Cell> = Vec::with_capacity(512);
    let mut truncated = false;
    // Indices of spine cells (re-seated after the scale pass — see below).
    let mut spine_cells: Vec<usize> = Vec::new();
    // By-folder families attach at the effective branch root (descend
    // single-sizeable-child chains like "This PC" → "C:"); dots at or
    // above that level form the solid "top-level" alpha tier.
    // The mindmap's family root is the SPINE HEAD: descend
    // single-child chains AND dominant children (≥ [`SPINE_SHARE`]). The
    // shared `effective_branch_root` (used by the four area modes)
    // only collapses single-child chains — with a dominant drive
    // ("This PC" → C: 95%) it hands ring 1 = [C:, D:], every C:
    // descendant one color family, and 95% of the map renders blue.
    // The spine unrolls the hub's story instead: ring 1 becomes the
    // first genuinely BRANCHING level, each branch its own family.
    let branch_root = spine_head(tree, node, depth.max(1));
    let branch_level = depth_below(tree, branch_root, node) + 1;
    // Root hub: brand coral, sized for the in-disc label (the JS draws
    // the root name + total bytes inside it, matching the sunburst
    // center treatment) — the map reads as anchored on "This PC",
    // not an anonymous gray blob.
    cells.push(Cell::dot(
        node,
        0,
        pack_rgba(0xFF6B4A),
        cx,
        cy,
        ROOT_DOT_R,
        cx,
        cy,
    ));
    if depth > 0 && total > 0 {
        layout_branches(
            tree,
            node,
            cx,
            cy,
            r_max,
            1,
            depth,
            rings,
            total as f32,
            -std::f32::consts::FRAC_PI_2,
            -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU,
            color,
            now,
            &mut cells,
            &mut truncated,
            0,
            branch_root,
            branch_level,
            &mut spine_cells,
        );
    }
    // Scale-to-fit (session-4 fill fix): the ring budget can under-
    // fill the canvas when the deepest spreading levels hold micro-dots
    // that cull away — the VISIBLE mass then hugs the center (the
    // "tiny, off-center mind map" finding). Measure the emitted dots'
    // extent from the root and uniformly scale POSITIONS (dot radii
    // stay size-proportional; parent links ride the same transform)
    // so the visible extent exactly reaches r_max. Clamped: a 0.5
    // floor keeps odd geometries from collapsing onto the hub, a 3.0
    // ceiling keeps a few specks from exploding across the canvas.
    if cells.len() > 1 {
        let reach = cells
            .iter()
            .filter(|c| c.id != node && (c.flags & 0b111) == crate::layout::cell_kind::DOT)
            .map(|c| ((c.g[0] - cx).powi(2) + (c.g[1] - cy).powi(2)).sqrt() + c.g[2])
            .fold(0.0_f32, f32::max);
        if reach > f32::EPSILON {
            let scale = (r_max / reach).clamp(0.5, 3.0);
            for c in &mut cells {
                if (c.flags & 0b111) == crate::layout::cell_kind::DOT {
                    if c.id != node {
                        c.g[0] = cx + (c.g[0] - cx) * scale;
                        c.g[1] = cy + (c.g[1] - cy) * scale;
                    }
                    // The root's own link is the center — scale-
                    // invariant; every other link is a position.
                    c.g[3] = cx + (c.g[3] - cx) * scale;
                    c.g[4] = cy + (c.g[4] - cy) * scale;
                }
            }
        }
        // Re-seat the SPINE cells clear of their parents: the uniform
        // scale shrinks offsets (a 0.8 scale pulls a 52 px spine offset
        // to 42 px) while radii stay size-proportional — the spine dot
        // could then overlap its parent (the hub or a nested parent
        // dot). Spine dots sit deep inside the canvas, so nudging them
        // back out never clips.
        let dot_r: std::collections::HashMap<u32, f32> =
            cells.iter().map(|c| (c.id, c.g[2])).collect();
        for &idx in &spine_cells {
            let Some(c) = cells.get(idx) else {
                continue;
            };
            let (dx, dy) = (c.g[0] - c.g[3], c.g[1] - c.g[4]);
            let d = (dx * dx + dy * dy).sqrt();
            if d < f32::EPSILON {
                continue;
            }
            let parent_id = tree.node(c.id).map_or(u32::MAX, |n| n.parent);
            let parent_r = dot_r.get(&parent_id).copied().unwrap_or(ROOT_DOT_R);
            let need = parent_r + c.g[2] + 6.0;
            if d < need {
                let (ux, uy) = (dx / d, dy / d);
                let cell = &mut cells[idx];
                cell.g[0] = cell.g[3] + ux * need;
                cell.g[1] = cell.g[4] + uy * need;
            }
        }
    }
    Ok(LayoutBuffer {
        cells,
        meta: LayoutMeta {
            mode: "mindmap".into(),
            generation: tree.generation,
            node,
            width,
            height,
            depth,
            color_mode: color,
            cell_count: 0,
            truncated,
            center: Some((cx, cy)),
            groups: Vec::new(),
            total_bytes: total,
        },
    })
}

/// Rings the map will actually SPREAD over below `node`, with
/// `limit` levels available: a spine level (single sizeable child OR
/// a dominant one, ≥ [`SPINE_SHARE`]) consumes a level but NO ring (its
/// children share the parent's ring — see `layout_branches`); a level
/// with ≥ 2 non-spine children consumes a level AND a ring. Descent
/// mirrors the emission's guards (dirs with children only), so the
/// budget the top call divides by matches the rings actually drawn.
fn spread_rings(tree: &Tree, node: u32, limit: u32) -> u32 {
    spread_rings_at(tree, node, limit, true)
}

/// [`spread_rings`] with the root-level spine flag threaded.
fn spread_rings_at(tree: &Tree, node: u32, limit: u32, at_root: bool) -> u32 {
    if limit == 0 {
        return 0;
    }
    let sizeable: Vec<u32> = tree
        .children_sorted(node)
        .iter()
        .copied()
        .filter(|&id| tree.node(id).is_some_and(|c| c.on_disk > 0))
        .collect();
    if sizeable.is_empty() {
        return 0;
    }
    let spine = spine_child(tree, &sizeable, at_root);
    // This level's ring hosts: the non-spine children's dots AND the
    // spine child's children's dots (the spine itself floats near the
    // parent). Count what extends BELOW that ring:
    //  * a normal (descendable) child starts its children one ring OUT
    //    (its own spread_rings already counts from there);
    //  * the spine child's children ride OUR ring, so its contribution
    //    below it is spread_rings(spine) − 1 (≥ 0: the spine is
    //    descendable by construction).
    let mut below = 0u32;
    for &id in &sizeable {
        let descendable = tree
            .node(id)
            .is_some_and(|c| c.is_dir() && c.child_count > 0);
        if !descendable {
            continue;
        }
        if Some(id) == spine {
            below = below.max(spread_rings_at(tree, id, limit - 1, false).saturating_sub(1));
        } else {
            below = below.max(spread_rings_at(tree, id, limit - 1, false));
        }
    }
    1 + below
}

/// The spine child of a level: a single-child chain continues at ANY
/// depth (the folder is visually just its content); a DOMINANT child
/// (≥ [`SPINE_SHARE`]) continues only at the layout root's own level —
/// one contextual unroll ("This PC" → C:), never an endless chain. A
/// deeper dominant child takes a normal sector (proportional to its
/// weight) so the map always branches; `None` when the level branches.
fn spine_child(tree: &Tree, sizeable: &[u32], at_root: bool) -> Option<u32> {
    let (&first, rest) = sizeable.split_first()?;
    let total: u64 = sizeable
        .iter()
        .map(|&id| tree.node(id).map_or(0, |c| c.on_disk))
        .sum();
    if total == 0 {
        return None;
    }
    let eligible = |id: u32| {
        tree.node(id)
            .is_some_and(|c| c.is_dir() && c.child_count > 0)
    };
    if rest.is_empty() {
        // Single sizeable child (share = 1.0 by definition) — a chain
        // at any depth.
        return eligible(first).then_some(first);
    }
    if at_root {
        let share = tree.node(first).map_or(0, |c| c.on_disk) as f32 / total as f32;
        if share >= SPINE_SHARE && eligible(first) {
            return Some(first);
        }
    }
    None
}

/// The spine HEAD: walk spine children from `node` (a dominant step
/// first — the root's context — then single-child chains) down to the
/// first level that actually branches. Families attach at ITS children
/// (ring 1); the hub keeps the chain's story.
fn spine_head(tree: &Tree, node: u32, limit: u32) -> u32 {
    spine_head_at(tree, node, limit, true)
}

/// [`spine_head`] with the root-level flag threaded (the dominant rule
/// applies only on the first step).
fn spine_head_at(tree: &Tree, node: u32, limit: u32, at_root: bool) -> u32 {
    if limit == 0 {
        return node;
    }
    let sizeable: Vec<u32> = tree
        .children_sorted(node)
        .iter()
        .copied()
        .filter(|&id| tree.node(id).is_some_and(|c| c.on_disk > 0))
        .collect();
    match spine_child(tree, &sizeable, at_root) {
        Some(next) => spine_head_at(tree, next, limit - 1, false),
        None => node,
    }
}

/// Place the children of `node` on the ring at radius `ring_r`, within
/// the inherited angular sector `[a0, a1)` — spans ∝ weights, and every
/// descendant stays inside its ancestor's wedge (children used to start
/// at 12 o'clock regardless of the parent's direction, letting deep
/// dots cross back over the root hub). Dot radii ∝ sqrt(share of the
/// ROOT total) — share-of-parent let a 99%-of-parent child of a small
/// branch render 4× its parent's size, floating over the root hub
/// (dwarfed hierarchy inversions).
///
/// Family rule (v2): a child gets a FRESH family when its parent is
/// the spine head (`node == branch_root` — ring 1, the first genuinely
/// branching level) OR when the parent is the layout root itself
/// (`depth_here == 1` — the root's minor branches like a second drive
/// get their own color instead of inheriting ring 1's first family).
/// Everyone deeper inherits.
///
/// SPINE rule (v2): a dominant child (≥ [`SPINE_SHARE`] of the level, a
/// dir, with children) does NOT take a sector on this ring — its dot
/// renders just OUTSIDE the parent along its angular mid (the spine
/// reads as the hub's continuation), and its CHILDREN share this ring
/// inside its sector (recursing with the ring budget unchanged). This
/// is what makes "This PC → C: 95% → [Users, Windows, …]" render as a
/// colorful ring 1 instead of one blue mega-sector.
///
/// Ring accounting (the session-4 fill fix): `rings_left` counts the
/// SPREADING levels this subtree still owns; `depth_left` is the hard
/// level ceiling. A branched level consumes one ring (its children
/// sit at `level_r = ring_r - step_r × (rings_left - 1)`, reserving
/// outer rings for descendants); a spine level passes the budget
/// through. Invariant: a call whose children branch always holds
/// `rings_left ≥ 1`, so `level_r` is well-defined there.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)] // the ring/spine/family rules form one readable algorithm
fn layout_branches(
    tree: &Tree,
    node: u32,
    cx: f32,
    cy: f32,
    ring_r: f32,
    depth_here: u32,
    depth_left: u32,
    rings_left: u32,
    root_total: f32,
    a0: f32,
    a1: f32,
    color: ColorMode,
    now: i64,
    cells: &mut Vec<Cell>,
    truncated: &mut bool,
    top_index: usize,
    branch_root: u32,
    branch_level: u32,
    spine_cells: &mut Vec<usize>,
) {
    if depth_left == 0 {
        return;
    }
    let children = tree.children_sorted(node);
    let total: u64 = children
        .iter()
        .map(|&id| tree.node(id).map_or(0, |c| c.on_disk))
        .sum();
    if total == 0 {
        return;
    }
    // The spine child (dominant or only): renders just outside the
    // parent along its angular mid; its children then share THIS ring
    // (the recursion passes the budget through). A level with no spine
    // child gives every child a normal sector on the ring.
    let sizeable_ids: Vec<u32> = children
        .iter()
        .copied()
        .filter(|&id| tree.node(id).is_some_and(|c| c.on_disk > 0))
        .collect();
    // The dominant continuation fires only at the layout root's own
    // level (one contextual unroll); single-child chains continue at
    // any depth.
    let spine = spine_child(tree, &sizeable_ids, depth_here == 1);
    let rings = rings_left.max(1);
    let step_r = ring_r / rings as f32; // per-spreading-level radius step
    let level_r = ring_r - step_r * (rings as f32 - 1.0);
    let mut cursor = a0; // start at the sector's leading edge
    for (i, &id) in children.iter().enumerate() {
        if crate::layout::over_budget(cells, truncated) {
            return;
        }
        let c = tree.node(id).expect("child id");
        if c.is_removed() || c.on_disk == 0 {
            continue;
        }
        let span = c.on_disk as f32 / total as f32 * (a1 - a0);
        let mid = cursor + span / 2.0;
        let is_spine = spine == Some(id);
        // Position: normal children sit at the ring radius along their
        // angular mid; the SPINE child sits just outside the parent
        // near its sector's LEADING edge — a dominant child owns most
        // of the circle, and anchoring the spine at the sector MID
        // threw the whole chain across the map (the "everything
        // cascades to the bottom-right" defect); the leading edge
        // reads as "the first branch off the hub" while its children
        // fill the sector from the same edge.
        let spine_ang = cursor + (span * 0.5).min(15.0_f32.to_radians());
        let x = if is_spine {
            cx + (ROOT_DOT_R + 22.0) * spine_ang.cos()
        } else {
            cx + level_r * mid.cos()
        };
        let y = if is_spine {
            cy + (ROOT_DOT_R + 22.0) * spine_ang.sin()
        } else {
            cy + level_r * mid.sin()
        };
        // Dot radius: share of the ROOT keeps every dot's area
        // comparable across the map and monotone down every chain. The
        // cap scales with the ring step (≈ r_max/rings) so small
        // canvases don't blob adjacent levels together; ring-1 dots
        // also clear the root hub (largest child vs hub overlap).
        let mut cap = (step_r * 0.8).clamp(10.0, DOT_BASE);
        if depth_here == 1 {
            cap = cap.min((level_r - ROOT_DOT_R - 2.0).max(6.0));
        }
        if is_spine {
            // The spine dot is the hub's continuation — compact, not a
            // full ring dot (it must never crowd ring 1).
            cap = cap.min(ROOT_DOT_R * 0.62);
        }
        // Radius ∝ sqrt(share of the ROOT) — area comparable across the
        // whole map and monotone along every chain (child ≤ parent).
        let r = (cap * (c.on_disk as f32 / root_total).sqrt()).max(MIN_R);
        // Visibility floor: sub-2.5 px dots are invisible specks that
        // only add overplotting noise (VLM: "too many micro-dots").
        // Culling sets `truncated` so the UI's "showing top N" hint
        // stays honest.
        if r < 2.5 {
            *truncated = true;
            cursor += span;
            continue;
        }
        // Family rule (v2): fresh at the spine head's children (ring 1)
        // AND at the layout root's own children (a second drive gets
        // its own family, not ring 1's first); inherited below.
        // Family color is now name-based, resolved inside `node_color` from
        // `branch_root` + `id`; the legacy `fam` index is no longer used
        // for coloring (kept for inheritance threading).
        let fam = if node == branch_root || depth_here == 1 { i } else { top_index };
        let rgb = match color {
            ColorMode::ByFolder => node_color(tree, id, color, now, branch_root, depth_here as u16, i),
            ColorMode::ByType => c.category().color(),
            ColorMode::ByAge => node_color(tree, id, color, now, branch_root, 0, i),
        };
        // The spine chain + ring 1 stay solid; deeper child dots get
        // the slightly translucent tier.
        let alpha = if depth_here <= branch_level || is_spine {
            ALPHA_TOP
        } else {
            ALPHA_NESTED
        };
        let rgba = (rgb << 8) | alpha;
        let cell_idx = cells.len();
        cells.push(Cell::dot(id, depth_here as u16, rgba, x, y, r, cx, cy));
        if is_spine {
            spine_cells.push(cell_idx);
        }
        if c.is_dir() && c.child_count > 0 && depth_left > 1 {
            layout_branches(
                tree,
                id,
                // The spine child's children orbit THIS node's center
                // (the hub/branch anchor), NOT the spine dot's offset
                // position — orbiting the offset clustered the whole
                // map around a point off-center (the "center-right
                // bunch" defect); the spine dot decorates the path.
                if is_spine { cx } else { x },
                if is_spine { cy } else { y },
                // The child's annulus is the OUTER remainder of ours —
                // a normal child consumed `step_r`; the SPINE child's
                // children share OUR ring, so it passes the annulus
                // through unchanged. (Passing `step_r` for spine nodes —
                // the pre-v2 chain bug — shrank each level
                // geometrically, collapsing the map into a concentric
                // blob around the root.)
                if is_spine { ring_r } else { ring_r - step_r },
                depth_here + 1,
                depth_left - 1,
                // Rings decrement ONLY when this level actually spread;
                // the spine passes the budget through (its children own
                // the same ring).
                if is_spine {
                    rings_left
                } else {
                    rings_left.saturating_sub(1)
                },
                root_total,
                cursor,
                cursor + span,
                color,
                now,
                cells,
                truncated,
                fam,
                branch_root,
                branch_level,
                spine_cells,
            );
        }
        cursor += span;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::node::{BatchEntry, Node, Tree};
    use crate::scan::rollup;

    fn build() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\M");
        t.append_batch(0, vec![dir("a"), dir("b")]);
        t.append_batch(1, vec![file("a1", 75, 75, 1), file("a2", 25, 25, 1)]);
        t.append_batch(2, vec![file("b1", 30, 30, 1)]);
        rollup::finalize(&mut t);
        t
    }

    fn dir(name: &str) -> BatchEntry {
        let mut node = Node::new_dir();
        node.modified = 1;
        BatchEntry {
            name: name.encode_utf16().collect(),
            node,
        }
    }

    fn file(name: &str, logical: u64, on_disk: u64, modified: i64) -> BatchEntry {
        let mut node = Node::new_file();
        node.logical = logical;
        node.on_disk = on_disk;
        node.modified = modified;
        BatchEntry {
            name: name.encode_utf16().collect(),
            node,
        }
    }

    #[test]
    fn dots_ring_around_root_with_weight_spans() {
        let t = build();
        let buf = mindmap(&t, 0, 900.0, 700.0, 3, ColorMode::ByType, 1).unwrap();
        let l1: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 1).collect();
        assert_eq!(l1.len(), 2);
        // a=100 (77% of the level — DOMINANT, ≥ [`SPINE_SHARE`]) renders as
        // the spine continuation near the hub; b=30 takes a ring sector.
        let spine_dot = l1.iter().find(|c| c.id == 1).expect("a (id 1)");
        let ring_dot = l1.iter().find(|c| c.id == 2).expect("b (id 2)");
        // Positions radiate from center; parent link points at the center.
        for c in &l1 {
            let d = ((c.g[0] - 450.0).powi(2) + (c.g[1] - 350.0).powi(2)).sqrt();
            assert!(d > 40.0, "level-1 dot should sit off-center");
            assert!(
                (c.g[3] - 450.0).abs() < 0.01 && (c.g[4] - 350.0).abs() < 0.01,
                "dot parent link should point at the root center"
            );
        }
        // The spine dot is COMPACT (the hub's continuation, capped at
        // 0.62 × ROOT_DOT_R — it must never crowd ring 1).
        assert!(
            spine_dot.g[2] <= ROOT_DOT_R * 0.62 + 0.01,
            "spine dot must be compact ({} > {})",
            spine_dot.g[2],
            ROOT_DOT_R * 0.62
        );
        // Ring dots keep the sqrt(share-of-root) law: b share = 30/130.
        let rb = ring_dot.g[2] / DOT_BASE;
        assert!((rb * rb - 30.0 / 130.0).abs() < 0.01);
        // And the spine dot sits clear of the (30 px) hub.
        let d_spine = ((spine_dot.g[0] - 450.0).powi(2) + (spine_dot.g[1] - 350.0).powi(2)).sqrt();
        assert!(
            d_spine >= ROOT_DOT_R + spine_dot.g[2] - 1.0,
            "spine dot must clear the hub ({d_spine} vs {}+{})",
            ROOT_DOT_R,
            spine_dot.g[2]
        );
    }

    #[test]
    fn invalid_geometry_rejected() {
        let t = build();
        assert!(mindmap(&t, 0, 10.0, 0.0, 2, ColorMode::ByAge, 1).is_err());
    }

    #[test]
    fn dots_and_labels_never_clip_the_canvas_bounds() {
        // Margin regression: the deepest ring sits AT r_max, so the
        // canvas edge must reserve the largest possible dot radius —
        // every emitted dot (x, y ± r) must stay inside the canvas.
        let t = build_single_drive();
        let w = 900.0f32;
        let h = 700.0f32;
        let buf = mindmap(&t, 0, w, h, 3, ColorMode::ByFolder, 1).unwrap();
        assert!(buf.cells.len() > 4, "tree must emit dots");
        for c in &buf.cells {
            if (c.flags & 0b111) != crate::layout::cell_kind::DOT {
                continue;
            }
            let (dot_x, dot_y, dot_r) = (c.g[0], c.g[1], c.g[2]);
            assert!(
                dot_x - dot_r >= -0.5
                    && dot_y - dot_r >= -0.5
                    && dot_x + dot_r <= w + 0.5
                    && dot_y + dot_r <= h + 0.5,
                "dot clips the canvas: ({dot_x},{dot_y}) r={dot_r} in {w}x{h}"
            );
        }
    }

    /// "This PC" → single "C:" drive → 6 folders with distinct sizes
    /// (each holding one file) — the shape that collapsed the whole
    /// mind map into one pastel family.
    fn build_single_drive() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "This PC");
        t.append_batch(0, vec![dir("C:")]); // id 1
        t.append_batch(
            1,
            vec![
                dir("Users"),    // 2
                dir("Windows"),  // 3
                dir("Programs"), // 4
                dir("Data"),     // 5
                dir("Temp"),     // 6
                dir("Logs"),     // 7
            ],
        );
        for (id, size) in [
            (2u32, 600u64),
            (3, 500),
            (4, 400),
            (5, 300),
            (6, 200),
            (7, 100),
        ] {
            t.append_batch(id, vec![file("f.bin", size, size, 1)]);
        }
        rollup::finalize(&mut t);
        t
    }

    #[test]
    fn single_child_root_assigns_branch_families_and_nested_alpha() {
        let t = build_single_drive();
        let buf = mindmap(&t, 0, 900.0, 700.0, 3, ColorMode::ByFolder, 1).unwrap();
        // Depth-2 dots = C:'s children (the effective top-level branches):
        // distinct pastel families, solid top-level alpha.
        let branch: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 2).collect();
        assert!(branch.len() >= 3, "all branch dots must emit");
        let mut rgb: Vec<u32> = branch.iter().map(|c| c.rgba >> 8).collect();
        rgb.sort_unstable();
        rgb.dedup();
        assert!(rgb.len() >= 3, "branch dots must span >= 3 pastel families");
        assert!(
            branch.iter().all(|c| c.rgba & 0xFF == ALPHA_TOP),
            "top-level dots stay solid"
        );
        // Nested child dots (files inside the branches) inherit their
        // branch family and use the translucent nested tier.
        let nested: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 3).collect();
        assert!(nested.len() >= 3, "nested dots must emit");
        let mut rgb: Vec<u32> = nested.iter().map(|c| c.rgba >> 8).collect();
        rgb.sort_unstable();
        rgb.dedup();
        assert!(
            rgb.len() >= 3,
            "nested dots must inherit distinct branch families"
        );
        assert!(nested.iter().all(|c| c.rgba & 0xFF == ALPHA_NESTED));
    }

    /// "This PC" → "C:" → three folders, one holding a nested home
    /// dir with files — the shape that the pre-fix ring decay collapsed.
    fn build_deep_chain() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "This PC");
        t.append_batch(0, vec![dir("C:")]); // 1
        t.append_batch(1, vec![dir("Users"), dir("Win"), dir("Tools")]); // 2, 3, 4
        t.append_batch(2, vec![dir("me")]); // 5
        t.append_batch(
            5,
            vec![
                file("a.bin", 100, 100, 1),
                file("b.bin", 50, 50, 1),
                file("c.bin", 25, 25, 1),
            ], // 6..8
        );
        t.append_batch(3, vec![file("w.bin", 200, 200, 1)]); // 9
                                                             // Tools: a SMALL branch (20/395) whose single child holds ~all
                                                             // of it — the share-of-parent radius dwarfed the parent (child
                                                             // rendered 4× the parent's dot, floating over the root hub).
        t.append_batch(4, vec![dir("kit")]); // 10
        t.append_batch(10, vec![file("k.bin", 19, 19, 1)]); // 11
        rollup::finalize(&mut t);
        t
    }

    #[test]
    fn deep_levels_render_across_the_full_radius_at_default_depth() {
        // Regression: the recursion used to pass `step_r` as the child
        // ring radius, shrinking each level geometrically (r_max/depth,
        // then /(depth-1), …) — at the app's default request depth 7 the
        // whole map collapsed into a ~60 px concentric blob around the
        // root and every level past 3 vanished via the `ring_r <= 4`
        // guard.
        let t = build_deep_chain();
        let w = 900.0f32;
        let h = 700.0f32;
        let req_depth = 7u32;
        let buf = mindmap(&t, 0, w, h, req_depth, ColorMode::ByFolder, 1).unwrap();
        // Level 4+ must actually emit (pre-fix: nothing past depth 3 —
        // the ring decay hit the `ring_r <= 4` guard by level 4).
        assert!(
            buf.cells.iter().any(|c| c.depth >= 4),
            "deep levels must render at default depth 7"
        );
        let r_max = (w.min(h) / 2.0 - DOT_BASE - 8.0).max(48.0);
        let (cx, cy) = (w / 2.0, h / 2.0);
        // Every dot sits ONE ring step out from its parent (level_r ==
        // step_r algebraically); the pre-fix decay put level-2 dots
        // 7.6 px and level-3 dots 1.5 px from their parents — concentric.
        // Collapsed single-child chain dots sit AT their parent by
        // design (d ≈ 0) and are skipped.
        let min_step = r_max / req_depth as f32 * 0.75;
        for c in &buf.cells {
            if c.depth == 0 || (c.flags & 0b111) != crate::layout::cell_kind::DOT {
                continue;
            }
            let d = ((c.g[0] - c.g[3]).powi(2) + (c.g[1] - c.g[4]).powi(2)).sqrt();
            if d < 1.0 {
                continue; // collapsed chain dot
            }
            assert!(
                d >= min_step,
                "dot at depth {} sits {d:.1}px from its parent (min ring step {min_step:.1}px) — rings must advance outward one step per level",
                c.depth
            );
        }
        // And the map must span the canvas, not hug the root: the
        // farthest dot+radius reaches ≥ 40% of r_max (pre-fix ~19%).
        let reach = buf
            .cells
            .iter()
            .filter(|c| (c.flags & 0b111) == crate::layout::cell_kind::DOT)
            .map(|c| ((c.g[0] - cx).powi(2) + (c.g[1] - cy).powi(2)).sqrt() + c.g[2])
            .fold(0.0_f32, f32::max);
        assert!(
            reach >= r_max * 0.4,
            "map must span the canvas (reach={reach}, r_max={r_max})"
        );
        // Share-of-ROOT radius: no child dot may exceed its parent's
        // dot — the pre-fix share-of-parent let Tools' dominant child
        // "kit" (95% of Tools, but Tools is 5% of the disk) render at
        // ~26 px against Tools' ~6 px, floating over the root hub.
        let by_id: std::collections::HashMap<u32, f32> =
            buf.cells.iter().map(|c| (c.id, c.g[2])).collect();
        let tools_r = by_id[&4];
        let kit_r = by_id[&10];
        assert!(
            kit_r <= tools_r + 0.01,
            "child dot (kit {kit_r}px) must not exceed its parent (Tools {tools_r}px)"
        );
        let users_r = by_id[&2];
        let me_r = by_id[&5];
        assert!(
            me_r <= users_r + 0.01,
            "collapsed chain dot (me {me_r}px) must not exceed its parent (Users {users_r}px)"
        );
        // Hub clearance: with sector-inheriting recursion, no branching
        // dot may cross back over the root hub (pre-fix: deep dots
        // landed 7 px from center, on top of the hub, because children
        // always started at 12 o'clock instead of inside the parent's
        // wedge). Collapsed chain dots sit ON their parent by design.
        for c in &buf.cells {
            if c.depth == 0 || (c.flags & 0b111) != crate::layout::cell_kind::DOT {
                continue;
            }
            let from_parent = ((c.g[0] - c.g[3]).powi(2) + (c.g[1] - c.g[4]).powi(2)).sqrt();
            if from_parent < 1.0 {
                continue; // collapsed chain dot
            }
            let from_center = ((c.g[0] - cx).powi(2) + (c.g[1] - cy).powi(2)).sqrt();
            assert!(
                from_center >= ROOT_DOT_R + c.g[2] - 1.0,
                "dot at depth {} crosses the root hub (center-dist {:.1}, r {:.1})",
                c.depth,
                from_center,
                c.g[2]
            );
        }
    }

    #[test]
    fn shallow_tree_fills_the_full_radius_at_default_depth() {
        // Regression (the "tiny, off-center mind map" finding): the
        // ring budget used to divide r_max by the REQUESTED depth (7),
        // so a 3-level tree filled only the inner ~40% of the canvas.
        // Now rings = reachable levels: the deepest dots must reach
        // ≥ 80% of r_max, and no dot may clip the bounds.
        let t = build_single_drive();
        let w = 900.0f32;
        let h = 700.0f32;
        let req_depth = 7u32;
        let buf = mindmap(&t, 0, w, h, req_depth, ColorMode::ByFolder, 1).unwrap();
        let r_max = (w.min(h) / 2.0 - DOT_BASE - 8.0).max(48.0);
        let (cx, cy) = (w / 2.0, h / 2.0);
        let dots: Vec<&Cell> = buf
            .cells
            .iter()
            .filter(|c| (c.flags & 0b111) == crate::layout::cell_kind::DOT)
            .collect();
        assert!(dots.len() > 4, "tree must emit dots");
        // Deepest level emitted = 3 (This PC → C: → branches → files).
        let deepest = dots.iter().map(|c| c.depth).max().unwrap();
        assert_eq!(deepest, 3, "single-drive tree is 3 levels deep");
        let reach = dots
            .iter()
            .map(|c| ((c.g[0] - cx).powi(2) + (c.g[1] - cy).powi(2)).sqrt() + c.g[2])
            .fold(0.0_f32, f32::max);
        assert!(
            reach >= r_max * 0.8,
            "shallow map must span the canvas (reach={reach:.1}, r_max={r_max:.1})"
        );
        // And still inside the bounds (the edge reserve holds).
        for c in &dots {
            assert!(
                c.g[0] - c.g[2] >= -0.5 && c.g[1] - c.g[2] >= -0.5,
                "dot clips top-left: ({:.1},{:.1}) r={:.1}",
                c.g[0],
                c.g[1],
                c.g[2]
            );
            assert!(
                c.g[0] + c.g[2] <= w + 0.5 && c.g[1] + c.g[2] <= h + 0.5,
                "dot clips bottom-right: ({:.1},{:.1}) r={:.1}",
                c.g[0],
                c.g[1],
                c.g[2]
            );
        }
    }
}
