/**
 * Mock layout engine (DEV/TEST ONLY): JS implementations of the 5 canvas
 * layouts emitting the SAME framed binary buffer as Rust `get_layout`
 * with the EXACT cell geometry contract (core/src/layout/mod.rs):
 *   RECT   g=[x, y, w, h, 0]
 *   ARC    g=[a0, a1, r0, r1, 0]  (center from meta.center)
 *   CIRCLE g=[cx, cy, r, 0, 0]
 *   DOT    g=[x, y, r, px, py]    (px/py = parent link position)
 *   HEADER g=[x, y, w, h, 0]
 */
import type { MockNode, MockTree } from "./tree";
import { CATEGORY_COLORS } from "./tree";

export interface MockCell {
  id: number;
  depth: number;
  flags: number;
  rgba: number;
  g: [number, number, number, number, number];
}

/** Pastel families (parity with core `folder_family_color`): blue, teal,
 *  violet, amber, rose, green, sky, slate. */
const TONE_BASE = [0x93c5fd, 0x99f6e4, 0xc4b5fd, 0xfde69a, 0xfdcdd3, 0xbbf7d0, 0xbae6fd, 0xcbd5e1];
const AGE_COLORS = [0x34d399, 0x60a5fa, 0x818cf8, 0xa78bfa, 0xf472b6, 0xf87171];

const DIR_BIT = 1 << 3;
const KIND_RECT = 0;
const KIND_HEADER = 4;
const KIND_ARC = 1;
const KIND_CIRCLE = 2;
const KIND_DOT = 3;
const MAX_CELLS = 20000;

function shade(hex: number, f: number): number {
  const r = Math.round(((hex >>> 16) & 0xff) * f);
  const g = Math.round(((hex >>> 8) & 0xff) * f);
  const b = Math.round((hex & 0xff) * f);
  return (Math.min(255, r) << 16) | (Math.min(255, g) << 8) | Math.min(255, b);
}

function ageBucket(modified: number, now: number): number {
  // `<` boundaries mirror core age::bucket_of exactly (a file exactly
  // 7 days old lands in bucket 1, not 0).
  const days = (now - modified) / 86400;
  if (days < 7) return 0;
  if (days < 30) return 1;
  if (days < 91) return 2;
  if (days < 365) return 3;
  if (days < 730) return 4;
  return 5;
}

function colorFor(
  node: MockNode,
  family: number,
  depth: number,
  mode: string,
  now: number,
  sibling = 0,
): number {
  if (mode === "by-type") {
    return shade(CATEGORY_COLORS[node.isDir ? 8 : node.category], 1);
  }
  if (mode === "by-age") {
    return AGE_COLORS[ageBucket(node.modified, now)];
  }
  const base = TONE_BASE[family % TONE_BASE.length];
  // Shade varies by depth + sibling parity (mirrors core
  // `folder_family_color`: neighbors separate, staying pastel).
  const f = Math.max(0.7, 1 - Math.min(depth, 4) * 0.06 - (sibling % 2 === 1 ? 0.05 : 0));
  return shade(base, f);
}

interface Item {
  node: number;
  v: number;
}

function childrenSorted(tree: MockTree, id: number): Item[] {
  return tree.nodes[id].children
    .map((c) => ({ node: c, v: tree.nodes[c].onDisk || tree.nodes[c].logical || 1 }))
    .filter((k) => k.v > 0)
    .sort((a, b) => b.v - a.v);
}

/** Strictly sizeable children (onDisk||logical > 0), size-desc. */
function sizeableSorted(tree: MockTree, id: number): Item[] {
  return tree.nodes[id].children
    .map((c) => ({ node: c, v: tree.nodes[c].onDisk || tree.nodes[c].logical }))
    .filter((k) => k.v > 0)
    .sort((a, b) => b.v - a.v);
}

/**
 * The node where by-folder families assign (mirrors the Rust engines'
 * effective branch root): descend while the node has exactly ONE
 * sizeable child that is a dir with children (This PC → C:), so families
 * start at the first real branching level.
 */
function effectiveBranchRoot(tree: MockTree, rootId: number): number {
  let cur = rootId;
  for (let guard = 0; guard < 64; guard++) {
    const n = tree.nodes[cur];
    if (!n || !n.isDir) return cur;
    const sizeable = n.children.filter((c) => {
      const k = tree.nodes[c];
      return k !== undefined && (k.onDisk > 0 || k.logical > 0);
    });
    if (sizeable.length !== 1) return cur;
    const only = sizeable[0];
    const k = tree.nodes[only];
    if (!k || !k.isDir || k.children.length === 0) return cur;
    cur = only;
  }
  return cur;
}

/**
 * Rings the map will actually SPREAD over below `node`, bounded by
 * `limit` levels — a level with exactly one sizeable child consumes a
 * LEVEL but no ring (the chain collapses onto the parent position); a
 * level with ≥ 2 sizeable children consumes a level AND a ring
 * (mirrors the Rust `mindmap::spread_rings`). Mind-map ring budgeting
 * divides by the SPREADING levels, not the requested depth — counting
 * chain levels as ring consumers left shallow maps at ~40-50% of the
 * canvas (the "tiny, off-center mind map" bug).
 */
/** A child this dominant continues the SPINE (Rust parity: ≥ 0.72 of
 * the level, a dir, with children) — its children share the parent's
 * ring; the level only gives sectors when it actually branches. */
const SPINE_SHARE = 0.72;

function spineChild(tree: MockTree, kids: Item[], atRoot: boolean): number | null {
  if (kids.length === 0) return null;
  const total = kids.reduce((a, b) => a + b.v, 0);
  if (total <= 0) return null;
  const first = tree.nodes[kids[0].node];
  const eligible = !!first && first.isDir && first.children.length > 0;
  // Single-child chains continue at ANY depth; a dominant child
  // continues only at the layout root's own level (one contextual
  // unroll — Rust parity).
  if (kids.length === 1 && eligible) return kids[0].node;
  if (atRoot && kids[0].v / total >= SPINE_SHARE && eligible) return kids[0].node;
  return null;
}

/** The spine HEAD: walk dominant children to the first branching
 * level — families attach at its children (ring 1). */
function spineHeadAt(tree: MockTree, node: number, limit: number, atRoot: boolean): number {
  if (limit <= 0) return node;
  const kids = childrenSorted(tree, node).filter((k) => k.v > 0);
  const spine = spineChild(tree, kids, atRoot);
  return spine === null ? node : spineHeadAt(tree, spine, limit - 1, false);
}

function spineHead(tree: MockTree, node: number, limit: number): number {
  return spineHeadAt(tree, node, limit, true);
}

function spreadRingsAt(tree: MockTree, node: number, limit: number, atRoot: boolean): number {
  if (limit <= 0) return 0;
  const kids = childrenSorted(tree, node).filter((k) => k.v > 0);
  if (kids.length === 0) return 0;
  const spine = spineChild(tree, kids, atRoot);
  // This level's ring hosts the non-spine children's dots AND the
  // spine child's children's dots; count what extends below it.
  let below = 0;
  for (const k of kids) {
    const kn = tree.nodes[k.node];
    if (!kn || !kn.isDir || kn.children.length === 0) continue;
    const sub = spreadRingsAt(tree, k.node, limit - 1, false);
    if (k.node === spine) {
      below = Math.max(below, Math.max(0, sub - 1));
    } else {
      below = Math.max(below, sub);
    }
  }
  return 1 + below;
}

function spreadRings(tree: MockTree, node: number, limit: number): number {
  return spreadRingsAt(tree, node, limit, true);
}

/**
 * node id → by-folder family index: families assign at the sizeable
 * children of the effective branch root (index among them, size desc);
 * every descendant inherits its branch's family. The single-child chain
 * above the branch root carries the dominant family so container strips
 * blend with their content.
 */
function buildFamilies(tree: MockTree, rootId: number, branchRoot: number): Map<number, number> {
  const fam = new Map<number, number>();
  const kids = sizeableSorted(tree, branchRoot);
  kids.forEach((k, i) => {
    const f = i % TONE_BASE.length;
    const stack = [k.node];
    while (stack.length > 0) {
      const id = stack.pop() as number;
      fam.set(id, f);
      const ch = tree.nodes[id].children;
      for (let j = 0; j < ch.length; j++) stack.push(ch[j]);
    }
  });
  const leadFam = kids.length > 0 ? (fam.get(kids[0].node) ?? 0) : 0;
  let cur = rootId;
  while (cur >= 0 && cur !== branchRoot) {
    fam.set(cur, leadFam);
    const p = tree.nodes[cur].parent;
    if (p === undefined) break;
    cur = p;
  }
  fam.set(branchRoot, leadFam);
  return fam;
}

/** Squarified treemap (Bruls et al.) — recursive on folders. */
function squarify(
  values: Item[],
  rect: [number, number, number, number],
  out: MockCell[],
  tree: MockTree,
  depth: number,
  maxDepth: number,
  fam: Map<number, number>,
  colorMode: string,
  now: number,
): void {
  const [x, y, w, h] = rect;
  const total = values.reduce((a, b) => a + b.v, 0);
  if (total <= 0 || w <= 1 || h <= 1) return;
  const scale = (w * h) / total;
  let cx = x;
  let cy = y;
  let items = values.slice();
  while (items.length > 0 && out.length < MAX_CELLS - 6) {
    const horizontal = w - (cx - x) >= h - (cy - y);
    const side = horizontal ? h - (cy - y) : w - (cx - x);
    if (side <= 0.5) break;
    // grow a row while the worst aspect ratio improves
    let row: Item[] = [items[0]];
    let rowSum = items[0].v;
    let best = Infinity;
    for (let i = 1; i < items.length; i++) {
      const candSum = rowSum + items[i].v;
      const candLen = (candSum * scale) / side;
      let worst = 0;
      for (const c of [...row, items[i]]) {
        const cw = horizontal ? (c.v * scale) / candLen : candLen;
        const chh = horizontal ? candLen : (c.v * scale) / candLen;
        worst = Math.max(worst, Math.max(cw / chh, chh / cw));
      }
      if (worst > best) break;
      best = worst;
      row = [...row, items[i]];
      rowSum = candSum;
    }
    items = items.slice(row.length);
    const rowLen = Math.max(0.5, (rowSum * scale) / side);
    let off = 0;
    for (let ri = 0; ri < row.length; ri++) {
      const c = row[ri];
      const n = tree.nodes[c.node];
      const cLen = (c.v * scale) / rowLen;
      const r: [number, number, number, number] = horizontal
        ? [cx, cy + off, rowLen, cLen]
        : [cx + off, cy, cLen, rowLen];
      // By-folder family: assigned at the effective branch root's
      // children, inherited by descendants; shade varies by depth +
      // sibling index (spec: one pastel family per top-level branch).
      const cellRgba =
        (colorFor(n, fam.get(c.node) ?? 0, depth, colorMode, now, values.indexOf(c)) << 8) | 0xff;
      if (n.isDir) {
        // PARITY with Rust treemap.rs: dirs that can afford a title band
        // emit a HEADER strip (kind 4) + children shifted below — NOT a
        // full-rect. The old mock drew the folder as a plain RECT, so
        // dev/CI screenshots never exercised the production HEADER
        // render path (the blank-band bug hid behind exactly this
        // divergence). Header constants mirror treemap.rs: 14px band,
        // min 42×26.
        const hasHdr = r[2] >= 42 && r[3] >= 26;
        const hdr = hasHdr ? 14 : 0;
        if (hasHdr) {
          out.push({
            id: c.node,
            depth,
            flags: DIR_BIT | KIND_HEADER,
            rgba: cellRgba,
            g: [r[0], r[1], r[2], 14, 0],
          });
        } else if (r[2] >= 6 && r[3] >= 6) {
          out.push({ id: c.node, depth, flags: DIR_BIT | KIND_RECT, rgba: cellRgba, g: [r[0], r[1], r[2], r[3], 0] });
        }
        const kids = childrenSorted(tree, c.node);
        const recurse = depth < maxDepth && kids.length > 0;
        if (recurse) {
          squarify(
            kids,
            [r[0] + 1, r[1] + hdr, Math.max(0, r[2] - 2), Math.max(0, r[3] - hdr - 1)],
            out,
            tree,
            depth + 1,
            maxDepth,
            fam,
            colorMode,
            now,
          );
        } else if (hasHdr && r[2] >= 6 && r[3] - hdr >= 6) {
          // Rust parity: no recursion → the body renders as a plain rect.
          out.push({ id: c.node, depth, flags: DIR_BIT | KIND_RECT, rgba: cellRgba, g: [r[0], r[1] + hdr, r[2], r[3] - hdr, 0] });
        }
      } else if (r[2] >= 6 && r[3] >= 6) {
        out.push({
          id: c.node,
          depth,
          flags: KIND_RECT,
          rgba: cellRgba,
          g: [r[0], r[1], r[2], r[3], 0],
        });
      }
      off += cLen;
    }
    if (horizontal) cx += rowLen;
    else cy += rowLen;
  }
}

/** Build cells for a canvas mode (Rust geometry contract). */
export function buildLayout(
  tree: MockTree,
  rootId: number,
  mode: string,
  width: number,
  height: number,
  depth: number,
  colorMode: string,
): { meta: Record<string, unknown>; cells: MockCell[] } {
  const now = Math.floor(Date.now() / 1000);
  const cells: MockCell[] = [];
  // Mind-map visibility culling flags truncation (Rust parity — the
  // footer's "(truncated)" hint must appear in dev exactly as in
  // production).
  let mindmapCulled = false;
  // Mind-map spine cell indices (re-seated after the scale pass —
  // Rust parity: the uniform scale can pull a spine dot into its
  // parent; nudging it back out never clips, spine dots sit deep).
  const spineCells: number[] = [];
  const root = tree.nodes[rootId];
  const total = root.onDisk || root.logical || 1;
  const kids = childrenSorted(tree, rootId);
  // By-folder families: one pastel family per branch under the effective
  // branch root (mirrors the Rust engines' effective_branch_root).
  const branchRoot = effectiveBranchRoot(tree, rootId);
  // Depth of the branch root below the layout root (+1 → tier index):
  // shared by bubbles (alpha tiers) and mind-map (alpha + collapse).
  let branchLevel = 1;
  for (let cur = branchRoot; cur !== rootId; ) {
    const p = tree.nodes[cur]?.parent;
    if (p === undefined || p < 0) break;
    cur = p;
    branchLevel += 1;
  }
  const fam = buildFamilies(tree, rootId, branchRoot);
  const famOf = (id: number): number => fam.get(id) ?? 0;

  if (mode === "treemap") {
    squarify(kids, [0, 0, width, height], cells, tree, 0, depth, fam, colorMode, now);
  } else if (mode === "sunburst") {
    const rMax = Math.min(width, height) / 2 - 6;
    const r0 = rMax * 0.16;
    const ringGap = 1.2;
    // Center disc (matches the Rust engine): coral; the JS layer draws
    // the root name + total size centered on it.
    cells.push({
      id: rootId,
      depth: 0,
      flags: KIND_CIRCLE,
      rgba: 0xff6b4abb,
      g: [width / 2, height / 2, r0, 0, 0],
    });
    const ring = (
      items: Item[],
      rIn: number,
      rOut: number,
      a0: number,
      a1: number,
      d: number,
    ): void => {
      const sum = items.reduce((a, b) => a + b.v, 0);
      if (sum <= 0 || rOut - rIn < 2) return;
      let a = a0;
      for (let i = 0; i < items.length; i++) {
        const it = items[i];
        const span = (it.v / sum) * (a1 - a0);
        if (span > 0.006 && cells.length < MAX_CELLS) {
          const n = tree.nodes[it.node];
          cells.push({
            id: it.node,
            depth: d,
            flags: (n.isDir ? DIR_BIT : 0) | KIND_ARC,
            rgba: (colorFor(n, famOf(it.node), d, colorMode, now, i) << 8) | 0xff,
            g: [a + 0.0012, a + span - 0.0012, rIn, rOut, 0],
          });
          if (n.isDir && d < depth - 1) {
            ring(
              childrenSorted(tree, it.node),
              rOut + ringGap,
              rOut + (rMax - rOut - ringGap) / Math.max(1, depth - 1 - d),
              a,
              a + span,
              d + 1,
            );
          }
        }
        a += span;
      }
    };
    ring(kids, r0 + ringGap, rMax * 0.5, -Math.PI / 2, -Math.PI / 2 + Math.PI * 2, 0);
  } else if (mode === "flame") {
    // Root title row (Rust parity): the CURRENT folder spans row 0 as
    // the anchor-gray band the renderer styles as a title bar; children
    // start at row 1. ADAPTIVE row count (Rust parity): rows = what the
    // data actually draws (bounded by the depth slider) — a shallow
    // subtree gets fat rows that still fill the height exactly instead
    // of slider-depth rows trailing into empty space.
    const levels = Math.max(3, Math.min(depth, 6));
    const rowsBelow = (items: Item[], d: number): number => {
      if (d > levels) return 0;
      const sum = items.reduce((a, b) => a + b.v, 0);
      if (sum <= 0) return 0;
      let best = 0;
      for (const it of items) {
        if (tree.nodes[it.node].isDir) {
          best = Math.max(best, rowsBelow(childrenSorted(tree, it.node), d + 1));
        }
      }
      return 1 + best;
    };
    const levelsUsed = Math.max(1, rowsBelow(kids, 1));
    // Conditional cap = the Rust engine's rule (parity): fat rows fill
    // exactly for 2+ levels; the 120px cap only guards the degenerate
    // 1-row almost-empty case.
    const rowH =
      levelsUsed + 1 >= 3
        ? (height - 6) / (levelsUsed + 1)
        : Math.min(120, (height - 6) / (levelsUsed + 1));
    cells.push({
      id: rootId,
      depth: 0,
      flags: KIND_RECT,
      rgba: 0x8e8e93ff,
      g: [0, 0, width, rowH, 0],
    });
    const rows: { item: Item; a0: number; a1: number; sib: number; d: number }[] = [];
    const layout = (items: Item[], a0: number, a1: number, d: number): void => {
      const sum = items.reduce((a, b) => a + b.v, 0);
      if (sum <= 0 || d > levels) return;
      // Two-pass, mirroring the Rust engine: KEPT children (span ≥ 1.5px)
      // are rescaled to fill the parent's span contiguously — skipped
      // sub-pixel children leave no background holes between kept blocks
      // (the old single-pass left a 1-2px hole per skipped sibling: the
      // "picket fence" the pixel audit found — 67 bg runs ≤2px).
      const kept = items.filter((it) => (it.v / sum) * (a1 - a0) * width > 1.5);
      if (!kept.length) return;
      const ksum = kept.reduce((a, b) => a + b.v, 0) || 1;
      let a = a0;
      for (let i = 0; i < kept.length; i++) {
        const it = kept[i];
        const span = (it.v / ksum) * (a1 - a0);
        rows.push({ item: it, a0: a, a1: a + span, sib: i, d });
        if (tree.nodes[it.node].isDir) {
          layout(childrenSorted(tree, it.node), a, a + span, d + 1);
        }
        a += span;
      }
    };
    layout(kids, 0, 1, 1);
    for (const r of rows.slice(0, MAX_CELLS)) {
      const n = tree.nodes[r.item.node];
      const x0 = r.a0 * width;
      const full = (r.a1 - r.a0) * width;
      // Wide blocks keep a 1px inset on each side (visual separation);
      // narrow blocks render FLUSH — the fixed inset striped the fine
      // "picket fence" of small files into near-invisible slivers
      // (mirrors the Rust engine's GAP_MIN_W rule).
      const wide = full >= 4;
      cells.push({
        id: r.item.node,
        depth: r.d,
        flags: (n.isDir ? DIR_BIT : 0) | KIND_RECT,
        rgba: (colorFor(n, famOf(r.item.node), r.d, colorMode, now, r.sib) << 8) | 0xff,
        g: [
          x0 + (wide ? 1 : 0),
          r.d * rowH + 1,
          wide ? Math.max(1, full - 2) : full,
          rowH - 2.5,
          0,
        ],
      });
    }
  } else if (mode === "bubbles") {
    // Faithful port of core/src/layout/bubbles.rs: FULL-DEPTH recursive
    // emission with alpha tiers + branch families (the old 2-level
    // render left every depth-3+ folder empty in dev while production
    // showed its children — Users rendered as a hollow circle).
    const cx = width / 2;
    const cy = height / 2;
    const rootR = Math.min(width, height) / 2 - 2; // core root_r
    const BUBBLE_PAD = 3; // core PAD: parent rim to content
    const BUBBLE_MIN_R = 1; // core MIN_R: sub-pixel bubbles skipped
    // Rust-engine parity: dense front-chain packing (tangent-pair
    // placement, origin-distance greedy) with a
    // bisection fill-fit. The old ring packing left 20-30% dead area per
    // parent in dev while production shows the same — both engines now
    // pack the advance-front way (core bubbles.rs `front_chain_pack`).
    // Children of a node with usable radius U get r = sqrt(share)·U,
    // are packed largest-first, then uniformly scaled by the largest
    // factor whose pack still fits U — sibling ratios stay exact and
    // the pack lands tangent to the rim.
    const FRONT_CHAIN_MAX = 32; // core parity: O(k³) guard for wide fan-outs
    const packFit = (
      sizes: number[],
      usable: number,
    ): { i: number; x: number; y: number; r: number }[] => {
      const total = sizes.reduce((a, b) => a + b, 0) || 1;
      const base = sizes.map((v) => Math.sqrt(v / total) * usable);
      const ks = base
        .map((r, i) => ({ i, x: 0, y: 0, r, base: r }))
        .filter((k) => k.r >= BUBBLE_MIN_R);
      if (!ks.length) return [];
      // The two centers at distance a.r+r from A and b.r+r from B
      // (externally tangent to both) — the two-circle intersection.
      const tangentCenters = (
        a: { x: number; y: number; r: number },
        b: { x: number; y: number; r: number },
        r: number,
      ): Array<[number, number]> => {
        const dx = b.x - a.x;
        const dy = b.y - a.y;
        const d2 = dx * dx + dy * dy;
        if (d2 <= Number.EPSILON) return [];
        const d = Math.sqrt(d2);
        const da = a.r + r;
        const db = b.r + r;
        if (d >= da + db || d <= Math.abs(da - db)) return [];
        const p = (da * da - db * db + d2) / (2 * d);
        const h = Math.sqrt(Math.max(0, da * da - p * p));
        const ux = dx / d;
        const uy = dy / d;
        const fx = a.x + p * ux;
        const fy = a.y + p * uy;
        return [
          [fx - h * uy, fy + h * ux],
          [fx + h * uy, fy - h * ux],
        ];
      };
      const extentOf = (arr: typeof ks, upto: number): number => {
        let m = 0;
        for (let q = 0; q < upto; q++) {
          m = Math.max(m, Math.hypot(arr[q].x, arr[q].y) + arr[q].r);
        }
        return m;
      };
      const EPS = 0.02; // core parity: tangency float-fuzz slack
      const frontChain = (arr: typeof ks): void => {
        arr.sort((a, b) => b.r - a.r);
        arr[0].x = 0;
        arr[0].y = 0;
        if (arr.length === 1) return;
        arr[1].x = arr[0].r + arr[1].r;
        arr[1].y = 0;
        for (let k = 2; k < arr.length; k++) {
          const r = arr[k].r;
          let best: [number, number, number] | null = null;
          for (let i = 0; i < k; i++) {
            for (let j = i + 1; j < k; j++) {
              for (const [cx, cy] of tangentCenters(arr[i], arr[j], r)) {
                let overlaps = false;
                for (let q = 0; q < k; q++) {
                  const min = arr[q].r + r;
                  const dx = cx - arr[q].x;
                  const dy = cy - arr[q].y;
                  if (dx * dx + dy * dy < (min - EPS) * (min - EPS)) {
                    overlaps = true;
                    break;
                  }
                }
                if (overlaps) continue;
                // Origin-distance greedy: interior gaps fill first, then
                // the tightest rim position (core parity).
                const score = Math.hypot(cx, cy) + r;
                if (best === null || score < best[2]) best = [cx, cy, score];
              }
            }
          }
          if (best) {
            arr[k].x = best[0];
            arr[k].y = best[1];
          } else {
            const ext = extentOf(arr, k);
            const ang = k * 2.399963;
            arr[k].x = (ext + r) * Math.cos(ang);
            arr[k].y = (ext + r) * Math.sin(ang);
          }
        }
      };
      const pack = (arr: typeof ks): number => {
        if (arr.length <= FRONT_CHAIN_MAX) {
          frontChain(arr);
        } else {
          arr.sort((a, b) => b.r - a.r);
          let placed = 0;
          let ringR = 0;
          const GAP = 2;
          while (placed < arr.length) {
            const r1 = arr[placed].r;
            const ringCenterR = ringR === 0 ? 0 : ringR + r1;
            if (ringCenterR <= 0) {
              arr[placed].x = 0;
              arr[placed].y = 0;
              ringR = r1;
              placed += 1;
              continue;
            }
            let count = 1;
            let angleUsed = 0;
            let j = placed + 1;
            while (j < arr.length) {
              const r2 = arr[j].r;
              const half = (r1 + r2 + GAP) / 2;
              const ratio = Math.min(1, half / ringCenterR);
              const theta = ratio >= 1 ? Math.PI : 2 * Math.asin(ratio);
              if (angleUsed + theta > Math.PI * 2) break;
              angleUsed += theta;
              count += 1;
              j += 1;
            }
            const step = (Math.PI * 2) / count;
            let angle = 0;
            for (const k of arr.slice(placed, placed + count)) {
              k.x = ringCenterR * Math.cos(angle);
              k.y = ringCenterR * Math.sin(angle);
              angle += step;
            }
            ringR = ringCenterR + arr[placed].r;
            placed += count;
          }
        }
        return arr.reduce((m, k) => Math.max(m, Math.hypot(k.x, k.y) + k.r), 0);
      };
      const needed = pack(ks);
      if (needed > usable && needed > 0) {
        let lo = 0;
        let hi = 1;
        for (let it = 0; it < 24; it++) {
          const mid = (lo + hi) / 2;
          for (const k of ks) k.r = mid * k.base;
          if (pack(ks) <= usable) lo = mid;
          else hi = mid;
        }
        for (const k of ks) k.r = lo * k.base;
        pack(ks);
      }
      return ks;
    };
    // Collect the bubble hierarchy to `depth` levels (core build_bubble):
    // geometry-free — emission works top-down from the collected sizes.
    interface Bubble {
      id: number;
      size: number;
      children: Bubble[];
    }
    const build = (nodeId: number, depthLeft: number): Bubble => {
      const n = tree.nodes[nodeId];
      const children: Bubble[] = [];
      if (depthLeft > 0 && n.isDir && n.children.length > 0) {
        for (const k of childrenSorted(tree, nodeId)) {
          children.push(build(k.node, depthLeft - 1));
        }
      }
      return { id: nodeId, size: n.onDisk || n.logical || 0, children };
    };
    // Core emit: circle at (x,y,drawnR), then allocate + ring-pack +
    // fill-fit children into usable = drawnR - PAD, recurse per child.
    // Alpha tiers at the branch level mirror ALPHA_PRIMARY/ALPHA_NESTED;
    // the by-folder family assigns at the branch root's children and is
    // inherited below (placement order, like the Rust `i`).
    const emitBubble = (
      b: Bubble,
      x: number,
      y: number,
      drawnR: number,
      depthHere: number,
      topIndex: number,
    ): void => {
      if (cells.length >= MAX_CELLS) return; // truncated computed post-hoc
      if (drawnR < BUBBLE_MIN_R) return;
      const n = tree.nodes[b.id];
      const rgb = colorFor(n, topIndex, depthHere, colorMode, now, topIndex);
      const alpha = depthHere <= branchLevel ? 0xb4 : 0xd9;
      cells.push({
        id: b.id,
        depth: depthHere,
        flags: (n.isDir ? DIR_BIT : 0) | KIND_CIRCLE,
        rgba: (rgb << 8) | alpha,
        g: [x, y, drawnR, 0, 0],
      });
      const usable = Math.max(drawnR - BUBBLE_PAD, 0);
      const sum = b.children.reduce((a, c) => a + c.size, 0);
      if (sum === 0 || usable < BUBBLE_MIN_R) return;
      const placed = packFit(
        b.children.map((c) => c.size),
        usable,
      );
      for (let pi = 0; pi < placed.length; pi++) {
        const p = placed[pi];
        emitBubble(
          b.children[p.i],
          x + p.x,
          y + p.y,
          p.r,
          depthHere + 1,
          b.id === branchRoot ? pi : topIndex,
        );
      }
    };
    emitBubble(build(rootId, depth), cx, cy, rootR, 0, 0);
  } else if (mode === "mind-map") {
    // Faithful port of core/src/layout/mindmap.rs (v2 spine design) —
    // the old heuristic (uniform angles + uncapped sqrt(share)*rMax
    // dots) degenerated at single-branch roots, and the v1 family rule
    // turned a dominant drive ("This PC" → C: 95%) into one blue
    // mega-sector. The v2 SPINE unrolls dominant chains: ring 1 is the
    // first genuinely branching level, each branch its own family.
    const cx = width / 2;
    const cy = height / 2;
    const DOT_BASE = 26; // core mindmap::DOT_BASE
    const MIN_R = 1.5; // core mindmap::MIN_R
    const ROOT_DOT_R = 30; // core mindmap::ROOT_DOT_R (label-sized hub)
    // The mind-map family root is the SPINE HEAD (dominant-chain
    // descent), not the shared effectiveBranchRoot (area-modes' rule).
    const mmBranchRoot = spineHead(tree, rootId, Math.max(depth, 1));
    let mmBranchLevel = 1;
    for (let cur = mmBranchRoot; cur !== rootId; ) {
      const p = tree.nodes[cur]?.parent;
      if (p === undefined || p < 0) break;
      if (p === rootId) break;
      mmBranchLevel += 1;
      cur = p;
    }
    // Reserve the largest possible dot + air so dots never clip the
    // edge (the deepest ring sits AT r_max); floor keeps tiny windows
    // usable. Mirrors core `r_max` exactly.
    const rMax = Math.max(Math.min(width, height) / 2 - DOT_BASE - 8, 48);
    // Depth of branchRoot below the layout root (+1 → tier index) —
    // computed once at buildLayout entry (shared with bubbles).
    // Root hub: brand coral, sized for the in-disc label (the
    // renderer draws the root name + total bytes inside it, matching
    // the sunburst center treatment).
    cells.push({
      id: rootId,
      depth: 0,
      flags: KIND_DOT,
      rgba: (0xff6b4a << 8) | 0xff,
      g: [cx, cy, ROOT_DOT_R, cx, cy],
    });
    if (depth > 0 && total > 0) {
      // ADAPTIVE ring budget (Rust parity): rings = SPREADING levels
      // (chains consume no ring), depth stays the hard ceiling — the
      // map fills the full radius on shallow trees instead of hugging
      // the center.
      const rings = spreadRings(tree, rootId, depth);
      const layoutBranches = (
        nodeId: number,
        px: number,
        py: number,
        ringR: number,
        depthHere: number,
        depthLeft: number,
        ringsLeft: number,
        rootTotal: number,
        a0: number,
        a1: number,
        topIndex: number,
      ): void => {
        if (depthLeft === 0) return;
        const children = childrenSorted(tree, nodeId);
        const sum = children.reduce((a, b) => a + b.v, 0);
        if (sum === 0) return;
        // The SPINE child: single-child chains at any depth, dominant
        // continuation only at the layout root's level (Rust parity).
        const spine = spineChild(tree, children, depthHere === 1);
        const rings = Math.max(ringsLeft, 1);
        const stepR = ringR / rings; // per-spreading-level radius step
        const levelR = ringR - stepR * (rings - 1);
        let cursor = a0; // start at the sector's leading edge
        for (let i = 0; i < children.length; i++) {
          if (cells.length >= MAX_CELLS) return;
          const k = children[i];
          const kn = tree.nodes[k.node];
          if (kn.onDisk === 0) continue;
          const span = (k.v / sum) * (a1 - a0);
          const mid = cursor + span / 2;
          const isSpine = spine === k.node;
          // Normal children sit at the ring radius along their angular
          // mid; the SPINE child sits just outside the parent near its
          // sector's LEADING edge (a dominant child owns most of the
          // circle — anchoring at the sector MID cascades the chain
          // across the map; the leading edge reads as "the first
          // branch off the hub"). Rust parity.
          const spineAng = cursor + Math.min(span * 0.5, (15 * Math.PI) / 180);
          const spineDist = ROOT_DOT_R + 22;
          const x = isSpine
            ? px + spineDist * Math.cos(spineAng)
            : px + levelR * Math.cos(mid);
          const y = isSpine
            ? py + spineDist * Math.sin(spineAng)
            : py + levelR * Math.sin(mid);
          // Dot radius ∝ sqrt(share of the ROOT) — share-of-parent let a
          // 99%-of-parent child of a small branch render 4× its parent
          // (dwarfed hierarchy inversions over the root hub). Share of
          // root keeps areas comparable and monotone down every chain.
          // Ring-1 dots also clear the root hub (mirrors Rust).
          let cap = Math.min(Math.max(stepR * 0.8, 10), DOT_BASE);
          if (depthHere === 1) {
            cap = Math.min(cap, Math.max(levelR - ROOT_DOT_R - 2, 6));
          }
          if (isSpine) {
            // The spine dot is the hub's continuation — compact, not a
            // full ring dot (it must never crowd ring 1).
            cap = Math.min(cap, ROOT_DOT_R * 0.62);
          }
          const r = Math.max(Math.sqrt(k.v / rootTotal) * cap, MIN_R);
          // Visibility floor: sub-2.5px dots are invisible noise. The
          // cull flags truncated like the Rust engine (the footer's
          // "(truncated)" hint stays honest in dev too).
          if (r < 2.5) {
            mindmapCulled = true;
            cursor += span;
            continue;
          }
          // Family rule (v2, Rust parity): fresh at the spine head's
          // children (ring 1) AND at the layout root's own children
          // (a second drive gets its own family); inherited below.
          const famIdx = nodeId === mmBranchRoot || depthHere === 1 ? i : topIndex;
          const rgb = colorFor(kn, famIdx, depthHere, colorMode, now, i);
          // The spine chain + ring 1 stay solid; deeper child dots get
          // the slightly translucent tier.
          const alpha = depthHere <= mmBranchLevel || isSpine ? 0xff : 0xcc;
          cells.push({
            id: k.node,
            depth: depthHere,
            flags: (kn.isDir ? DIR_BIT : 0) | KIND_DOT,
            rgba: (rgb << 8) | alpha,
            g: [x, y, r, px, py],
          });
          if (isSpine) spineCells.push(cells.length - 1);
          if (kn.isDir && kn.children.length > 0 && depthLeft > 1) {
            // Child's annulus = the OUTER remainder (this level consumed
            // stepR) — passing stepR decays geometrically and collapses
            // the map into a concentric blob (fixed both sides). A spine
            // node consumed no ring — budget passes through unchanged
            // (Rust parity: its children share OUR ring).
            layoutBranches(
              k.node,
              // The spine child's children orbit THIS node's center
              // (the hub/branch anchor), NOT the spine dot's offset
              // position — orbiting the offset clustered the whole map
              // around a point off-center (Rust parity).
              isSpine ? px : x,
              isSpine ? py : y,
              isSpine ? ringR : ringR - stepR,
              depthHere + 1,
              depthLeft - 1,
              // Rings decrement ONLY when this level actually spread;
              // the spine passes the budget through.
              isSpine ? ringsLeft : Math.max(0, ringsLeft - 1),
              rootTotal,
              cursor,
              cursor + span,
              famIdx,
            );
          }
          cursor += span;
        }
      };
      layoutBranches(rootId, cx, cy, rMax, 1, depth, rings, total, -Math.PI / 2, -Math.PI / 2 + Math.PI * 2, 0);
      // Scale-to-fit (Rust parity): the ring budget can under-fill the
      // canvas when the deepest spreading levels hold micro-dots that
      // cull away — the VISIBLE mass then hugs the center. Uniformly
      // scale positions (radii stay size-proportional; links ride the
      // same transform) so the visible extent exactly reaches rMax.
      const dots = cells.filter((c) => (c.flags & 0b111) === KIND_DOT);
      if (dots.length > 1) {
        let reach = 0;
        for (const c of dots) {
          if (c.id === rootId) continue;
          reach = Math.max(reach, Math.hypot(c.g[0] - cx, c.g[1] - cy) + c.g[2]);
        }
        if (reach > 1e-6) {
          const scale = Math.min(3, Math.max(0.5, rMax / reach));
          for (const c of dots) {
            if (c.id !== rootId) {
              c.g[0] = cx + (c.g[0] - cx) * scale;
              c.g[1] = cy + (c.g[1] - cy) * scale;
            }
            c.g[3] = cx + (c.g[3] - cx) * scale;
            c.g[4] = cy + (c.g[4] - cy) * scale;
          }
        }
        // Re-seat the spine cells clear of their parents (Rust parity).
        const dotR = new Map<number, number>();
        for (const c of dots) dotR.set(c.id, c.g[2]);
        for (const idx of spineCells) {
          const c = cells[idx];
          if (!c) continue;
          const dx = c.g[0] - c.g[3];
          const dy = c.g[1] - c.g[4];
          const d = Math.hypot(dx, dy);
          if (d < 1e-6) continue;
          const parentId = tree.nodes[c.id]?.parent ?? -1;
          const parentR = dotR.get(parentId) ?? ROOT_DOT_R;
          const need = parentR + c.g[2] + 6;
          if (d < need) {
            c.g[0] = c.g[3] + (dx / d) * need;
            c.g[1] = c.g[4] + (dy / d) * need;
          }
        }
      }
    }
  }

  const truncated = cells.length > MAX_CELLS || mindmapCulled;
  const finalCells = truncated ? cells.slice(0, MAX_CELLS) : cells;
  // Legend groups mirror the family level (children of the effective
  // branch root) so the chips match the colors on the canvas.
  const branchKids = sizeableSorted(tree, branchRoot);
  const groups = branchKids.slice(0, 8).map((k, i) => ({
    id: 0xffff0000 + i,
    name: tree.nodes[k.node].name,
    color: colorFor(tree.nodes[k.node], famOf(k.node), 0, colorMode, now, 0),
    size: k.v,
  }));

  return {
    meta: {
      mode,
      generation: tree.generation,
      node: rootId,
      width,
      height,
      depth,
      colorMode,
      cellCount: finalCells.length,
      truncated,
      center: [width / 2, height / 2] as [number, number],
      groups,
      totalBytes: total,
    },
    cells: finalCells,
  };
}

/** Encode cells into the framed binary layout buffer. */
export function encodeLayout(
  meta: Record<string, unknown>,
  cells: MockCell[],
  tree: MockTree,
): ArrayBuffer {
  const metaJson = JSON.stringify(meta);
  const metaBytes = new TextEncoder().encode(metaJson);
  // Frame: [u32 meta_len LE][meta JSON][32 B cells][8 B sizes per cell]
  // — the sizes tail mirrors the Rust core's `sizes_to_bytes` (real node
  // ids from the tree, synthetic group ids from meta.groups) so the JS
  // decoder's two-line "name / size" labels work identically in dev.
  const buf = new ArrayBuffer(4 + metaBytes.length + cells.length * 40);
  const view = new DataView(buf);
  view.setUint32(0, metaBytes.length, true);
  new Uint8Array(buf, 4, metaBytes.length).set(metaBytes);
  let o = 4 + metaBytes.length;
  const groupSizes = new Map<number, number>(
    ((meta.groups as { id: number; size: number }[] | undefined) ?? []).map((g) => [g.id, g.size]),
  );
  const sizeOf = (id: number): number => {
    if ((id >>> 0) >= 0xffff0000) return groupSizes.get(id) ?? 0;
    const n = tree.nodes[id];
    return n ? n.onDisk || n.logical || 0 : (groupSizes.get(id) ?? 0);
  };
  for (const c of cells) {
    view.setUint32(o, c.id >>> 0, true);
    view.setUint16(o + 4, c.depth, true);
    view.setUint16(o + 6, c.flags, true);
    view.setUint32(o + 8, c.rgba >>> 0, true);
    view.setFloat32(o + 12, c.g[0], true);
    view.setFloat32(o + 16, c.g[1], true);
    view.setFloat32(o + 20, c.g[2], true);
    view.setFloat32(o + 24, c.g[3], true);
    view.setFloat32(o + 28, c.g[4], true);
    o += 32;
  }
  for (const c of cells) {
    // u64 LE via two u32 halves (DataView has no setUint64).
    const s = sizeOf(c.id);
    view.setUint32(o, s % 4294967296, true);
    view.setUint32(o + 4, Math.floor(s / 4294967296), true);
    o += 8;
  }
  return buf;
}
