//! Layout commands (spec §7; doc 03 M4.1): `get_layout` — raw binary IPC
//! with a JSON meta header — plus the Rust-side layout cache keyed
//! `(generation, node, mode, w, h, depth, color)` (doc 03 M4.1), and
//! `get_names` for batched name fetch.
//!
//! Framing (one round trip, no base64): the response body is
//! `[u32 meta_len LE][meta JSON bytes][cells binary 32 B each]`.
//! Cells: `id u32 | depth u16 | flags u16 | rgba u32 | 5×f32 geometry`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use diskgenie_core::layout::regroup::{self, Regrouped};
use diskgenie_core::layout::{
    bubbles, flame, folder_legend, groups, mindmap, sunburst, treemap, ColorMode, LayoutBuffer,
    MAX_CELLS,
};
use diskgenie_core::scan::node::Tree;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::ipc::Response;
use tauri::State;

use crate::state::AppState;

/// The layout request (doc 03 M4.1; every field keys the cache).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutRequest {
    /// Scan generation (stale requests error out).
    pub generation: u64,
    /// Folder node id to lay out.
    pub node: u32,
    /// Visualization mode.
    pub mode: LayoutMode,
    /// Viewport width (CSS px).
    pub width: u32,
    /// Viewport height (CSS px).
    pub height: u32,
    /// Depth setting (treemap/sunburst/flame slider, 2..=10).
    pub depth: u32,
    /// Color mode.
    pub color: ColorMode,
}

/// The canvas visualization modes (spec §7; the DOM modes — Folders,
/// Top Sizes, Age Map, List — read the tree through their own commands).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutMode {
    /// Squarified treemap.
    Treemap,
    /// Sunburst rings.
    Sunburst,
    /// Flame/icicle rows.
    Flame,
    /// Nested bubble packing.
    Bubbles,
    /// Radial mind map.
    MindMap,
}

/// Cache key = the full request (doc 03 M4.1).
pub type Cache = Mutex<LruCache<LayoutRequest, Arc<Vec<u8>>>>;

/// Layout-cache entry budget. Keys are EXACT `width×height`: a resize drag
/// produces one cold key per pixel step, so the bound exists to keep a
/// drag's memory flat (insertion-order eviction is enough — stale sizes
/// are one-shot, never re-requested).
const LAYOUT_CACHE_CAP: usize = 64;

/// Regrouped-dataset cache key: (generation, node, color mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegroupKey {
    /// Scan generation.
    pub generation: u64,
    /// Folder node.
    pub node: u32,
    /// Color mode (By-type / By-age produce different groups).
    pub color: ColorMode,
}

/// Regroup-cache entry budget: one entry per visited folder × color mode
/// (the walk is the expensive part; resizes re-layout the same groups).
const REGROUP_CACHE_CAP: usize = 32;

/// Cache of regrouped wrapper datasets.
pub type RegroupCache = Mutex<LruCache<RegroupKey, Arc<Regrouped>>>;

/// Insertion-order LRU with a hard cap. `get` touches recency; inserts
/// past the cap evict the least-recent entry. The previous unbounded
/// HashMaps only emptied on a scan swap, which let per-resize keys
/// accumulate for the lifetime of a session.
pub struct LruCache<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
    cap: usize,
}

impl<K: Eq + std::hash::Hash + Clone, V> LruCache<K, V> {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Touch + fetch.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        if let Some(v) = self.map.get(key) {
            // Move the key to the back (most recent) when it is not
            // already there (cheap common case: back of the queue).
            if self.order.back().is_some_and(|k| k != key) {
                self.order.retain(|k| k != key);
                self.order.push_back(key.clone());
            }
            return Some(v);
        }
        None
    }

    /// Insert (evicting the least-recent entry past the cap).
    pub fn insert(&mut self, key: K, value: V) {
        if self.map.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        } else {
            // Existing key refreshed: re-seat recency.
            self.order.retain(|k| *k != key);
            self.order.push_back(key);
        }
        while self.map.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            } else {
                break;
            }
        }
    }

    /// Drop everything (scan swap path).
    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    /// Current entry count (tests + telemetry).
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Empty (tests).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Managed state constructor (registered in `lib.rs`).
#[must_use]
pub fn regroup_cache() -> RegroupCache {
    Mutex::new(LruCache::new(REGROUP_CACHE_CAP))
}

/// Managed state constructor (registered in `lib.rs`).
#[must_use]
pub fn layout_cache() -> Cache {
    Mutex::new(LruCache::new(LAYOUT_CACHE_CAP))
}

/// Compute (or fetch from cache) the layout for a request. The response
/// body is the framed `[meta len][meta JSON][cells]` buffer.
///
/// `(async)`: the command runs on Tauri's async runtime instead of the
/// main thread. The cold path (an uncached by-type/by-age request first
/// walks the entire subtree, and every resize pixel is a cold
/// width×height key) is a multi-hundred-ms main-thread stall on
/// million-node trees — the exact "app freezes mid-use" the owner
/// reported. The frontend already drops superseded responses (the
/// CanvasViz effect's `disposed` guard), so off-thread completion
/// ordering is safe.
///
/// # Errors
/// Returns a plain string message (mapped to a JS rejection) when the
/// generation is stale, the node is missing, or geometry is invalid.
#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn get_layout(
    req: LayoutRequest,
    state: State<'_, AppState>,
    cache: State<'_, Cache>,
    regroup_cache: State<'_, RegroupCache>,
) -> Result<Response, String> {
    let tree = state.tree.read();
    let Some(tree) = tree.as_ref() else {
        return Err("no scan yet".into());
    };
    if tree.generation != req.generation {
        return Err(format!(
            "stale generation {} (current {})",
            req.generation, tree.generation
        ));
    }

    // Cache hit?
    if let Some(hit) = cache.lock().get(&req) {
        return Ok(Response::new(hit.as_ref().clone()));
    }

    let buffer = compute_layout(tree, &req, &regroup_cache)?;
    let framed = frame(&buffer, tree);
    cache.lock().insert(req, Arc::new(framed.clone()));
    Ok(Response::new(framed))
}

/// Batched node-name fetch (spec §7: `get_names(ids)` contract — the UI
/// batches label lookups; the JS side keeps an LRU). `(async)` for the
/// same off-main-thread reason as `get_layout` (label bursts land right
/// after a paint, when the main thread is busiest).
///
/// # Errors
/// String message when the generation is stale.
#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn get_names(
    generation: u64,
    ids: Vec<u32>,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    let tree = state.tree.read();
    let Some(tree) = tree.as_ref() else {
        return Err("no scan yet".into());
    };
    if tree.generation != generation {
        return Err(format!(
            "stale generation {} (current {})",
            generation, tree.generation
        ));
    }
    Ok(tree.names_batch(&ids))
}

/// Dispatch to the core layout engines (spec §7 modes). By-type and
/// By-age color modes regroup first (spec §7 color modes): the groups
/// become depth-1 cells, member files depth-2 (synthetic ids
/// `0xFFFF_0000 + n`, described in the metadata — never a tree mutation).
fn compute_layout(
    tree: &Tree,
    req: &LayoutRequest,
    regroup_cache: &RegroupCache,
) -> Result<LayoutBuffer, String> {
    let w = f32::from(u16::try_from(req.width).map_err(|_| "width out of range")?);
    let h = f32::from(u16::try_from(req.height).map_err(|_| "height out of range")?);
    let now = now_unix();
    let out = match req.color {
        ColorMode::ByFolder => match req.mode {
            LayoutMode::Treemap => {
                treemap::treemap(tree, req.node, w, h, req.depth, req.color, now)
            }
            LayoutMode::Sunburst => {
                sunburst::sunburst(tree, req.node, w, h, req.depth, req.color, now)
            }
            LayoutMode::Flame => flame::flame(tree, req.node, w, h, req.depth, req.color, now),
            LayoutMode::Bubbles => {
                bubbles::bubbles(tree, req.node, w, h, req.depth, req.color, now)
            }
            LayoutMode::MindMap => {
                mindmap::mindmap(tree, req.node, w, h, req.depth, req.color, now)
            }
        },
        ColorMode::ByType | ColorMode::ByAge => {
            let regrouped = regrouped_for(tree, req, regroup_cache);
            match req.mode {
                LayoutMode::Treemap => treemap::treemap_groups(
                    &regrouped.groups,
                    w,
                    h,
                    tree.generation,
                    req.node,
                    req.color,
                ),
                LayoutMode::Sunburst => groups::sunburst_groups(
                    &regrouped.groups,
                    w,
                    h,
                    tree.generation,
                    req.node,
                    req.color,
                ),
                LayoutMode::Flame => groups::flame_groups(
                    &regrouped.groups,
                    w,
                    h,
                    tree.generation,
                    req.node,
                    req.color,
                ),
                LayoutMode::Bubbles => groups::bubbles_groups(
                    &regrouped.groups,
                    w,
                    h,
                    tree.generation,
                    req.node,
                    req.color,
                ),
                LayoutMode::MindMap => groups::mindmap_groups(
                    &regrouped.groups,
                    w,
                    h,
                    tree.generation,
                    req.node,
                    req.color,
                ),
            }
        }
    };
    let mut out = out.map_err(|e| e.to_string())?;
    // By-folder legend chips: the branch-root family level. The engines
    // only fill meta.groups for the regroup (by-type/by-age) modes, so
    // by-folder legends existed in the dev mock but never in production.
    if matches!(req.color, ColorMode::ByFolder) {
        out.meta.groups = folder_legend(tree, req.node);
    }
    // Post-pass: mark directory cells (DIR_BIT) so the JS hover chip and
    // dblclick-open logic never need a round trip. Synthetic group ids
    // live above SYNTH_BASE and describe buckets, not nodes.
    for c in &mut out.cells {
        if c.id < regroup::SYNTH_BASE {
            if let Some(n) = tree.node(c.id) {
                if n.is_dir() {
                    c.flags |= diskgenie_core::layout::cell_kind::DIR_BIT;
                }
            }
        }
    }
    Ok(out)
}

/// Resolve (or compute + cache) the regrouped dataset for a request.
/// `member_cap` keeps the walk bounded; engines enforce MAX_CELLS
/// regardless (the `truncated` flag stays honest).
fn regrouped_for(tree: &Tree, req: &LayoutRequest, cache: &RegroupCache) -> Arc<Regrouped> {
    let key = RegroupKey {
        generation: tree.generation,
        node: req.node,
        color: req.color,
    };
    if let Some(hit) = cache.lock().get(&key) {
        return Arc::clone(hit);
    }
    let now = now_unix();
    // Per-group member budget: 9 categories / 6 buckets max, MAX_CELLS
    // total headroom before engine truncation.
    let member_cap = MAX_CELLS / 9;
    let data = match req.color {
        ColorMode::ByType => regroup::by_type(tree, req.node, member_cap),
        ColorMode::ByAge => regroup::by_age(tree, req.node, now, member_cap),
        ColorMode::ByFolder => unreachable!("guarded by the caller"),
    };
    let arc = Arc::new(data);
    cache.lock().insert(key, Arc::clone(&arc));
    arc
}

/// Frame the response: `[u32 meta_len LE][meta JSON][cells binary]`.
fn frame(buffer: &LayoutBuffer, tree: &Tree) -> Vec<u8> {
    let mut meta = buffer.meta.clone();
    // Cell count is bounded by MAX_CELLS (20 000) at the engine level.
    meta.cell_count = u32::try_from(buffer.cells.len()).unwrap_or(u32::MAX);
    let meta_json = serde_json::to_vec(&meta).unwrap_or_else(|_| b"{}".to_vec());
    let cells = buffer.cells_to_bytes();
    let sizes = buffer.sizes_to_bytes(tree);
    let mut out = Vec::with_capacity(4 + meta_json.len() + cells.len() + sizes.len());
    // Meta JSON is small (a few KB — far below u32).
    out.extend_from_slice(
        &u32::try_from(meta_json.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(&meta_json);
    out.extend_from_slice(&cells);
    // Sizes tail: one LE u64 per cell (same order) — decoded by the JS
    // twin for the two-line "name / size" cell labels. The decoder trusts
    // `meta.cellCount`, so the tail length never confuses the cell count.
    out.extend_from_slice(&sizes);
    out
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Drop cached layouts when the tree (generation) is replaced — called
/// from the scan swap path.
pub fn clear_cache(cache: &Cache) {
    cache.lock().clear();
}

/// Drop cached regrouped datasets on tree swap (generation change).
pub fn clear_regroup_cache(cache: &RegroupCache) {
    cache.lock().clear();
}

const _: () = assert!(std::mem::size_of::<usize>() >= std::mem::size_of::<u32>());
const _: () = {
    // Keep the cell budget contract visible at the command boundary.
    assert!(MAX_CELLS == 20_000);
};
