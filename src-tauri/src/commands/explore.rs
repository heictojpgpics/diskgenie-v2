//! Explore data commands (spec §7; doc 03 M4.3–M4.15): the DOM-mode
//! datasets — Folders grid, Top Sizes, Age Map, List — plus the §8
//! inspector `node_details`. All commands take the scan **generation**
//! and reject stale requests (spec §9 pitfall: never render a dead tree).
//!
//! Heavy "anywhere" rankings are computed on a background thread
//! (spawn_blocking) and cached per (generation, node, scope) exactly per
//! spec §7.7; Age Map caching follows the same shape (spec §7.8:
//! "Computed off the UI thread in Rust"). The pure computation lives in
//! `compute_*` functions over `&Tree` so the host can test the real
//! logic without a Tauri runtime.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Arc;

use diskgenie_core::age;
use diskgenie_core::scan::node::Tree;
use serde::Serialize;
use tauri::State;

use crate::state::AppState;

/// Files grid cap (spec §7.2: "Cap files at 600").
pub const FILES_CAP: usize = 600;
/// List mode cap per level (spec §7.9: "cap at 500 children per level").
pub const LIST_CAP: usize = 500;
/// Top Sizes ranking depth (spec §7.7: "top 200").
pub const TOP_CAP: usize = 200;
/// Big & Untouched row cap (spec §7.8 list; keep the UI bounded).
pub const BIG_CAP: usize = 500;
/// Largest Inside rows in the inspector (spec §8).
pub const LARGEST_CAP: usize = 10;

/// One pastel category dot on a folder card (spec §7.2: up to 3).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryDot {
    /// `0xRRGGBB`.
    pub color: u32,
    /// Category label ("Video", "Documents", …).
    pub label: String,
    /// Bytes in that category.
    pub size: u64,
}

/// One folder-shaped card (spec §7.2).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderCard {
    pub id: u32,
    pub name: String,
    /// On-disk size.
    pub size: u64,
    /// "N items" = descendant files + folders.
    pub item_count: u64,
    pub file_count: u64,
    pub folder_count: u64,
    /// Up to 3 category dots.
    pub categories: Vec<CategoryDot>,
    /// Most recent descendant modification (Unix seconds; 0 unknown).
    pub modified: i64,
    /// Protected items cannot be staged (tooltip in the UI).
    pub protected: bool,
}

/// One file tile (spec §7.2 Files grid).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTile {
    pub id: u32,
    pub name: String,
    pub size: u64,
    /// Category label ("Category · age" caption).
    pub category: String,
    /// `0xRRGGBB`.
    pub category_color: u32,
    /// Last modified (Unix seconds; 0 unknown).
    pub modified: i64,
    pub protected: bool,
    pub cloud: bool,
}

/// The Folders-mode dataset.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderView {
    pub generation: u64,
    pub node: u32,
    pub name: String,
    /// Folder on-disk size (header "big size").
    pub size: u64,
    /// "· N files · N folders" (descendant counts).
    pub file_count: u64,
    pub folder_count: u64,
    pub folders: Vec<FolderCard>,
    pub files: Vec<FileTile>,
    /// True when the 600-file cap truncated the grid.
    pub files_capped: bool,
}

/// Folders-mode computation (pure — the real logic, host-testable).
/// Children arrive largest-first from `children_sorted`; the name filter
/// is a case-insensitive substring (spec §7.15).
#[must_use]
pub fn compute_folder_view(tree: &Tree, node: u32, filter: &str) -> FolderView {
    let needle = filter.trim().to_ascii_lowercase();
    let mut folders = Vec::new();
    let mut files = Vec::new();
    let mut files_capped = false;
    for &child in tree.children_sorted(node) {
        let Some(n) = tree.node(child) else { continue };
        if n.is_removed() {
            continue;
        }
        let name = tree.name(child);
        if !needle.is_empty() && !name.to_ascii_lowercase().contains(&needle) {
            continue;
        }
        if n.is_dir() {
            let (fc, folders_count, max_mod) = tree
                .dir_extras
                .get(n.dir_index as usize)
                .map_or((0, 0, 0), |e| {
                    (e.file_count, e.folder_count, e.max_descendant_modified)
                });
            let categories: Vec<CategoryDot> = tree
                .top_categories(child, 3)
                .into_iter()
                .map(|(c, sz)| CategoryDot {
                    color: c.color(),
                    label: c.label().to_string(),
                    size: sz,
                })
                .collect();
            folders.push(FolderCard {
                id: child,
                name,
                size: n.on_disk,
                item_count: fc.saturating_add(folders_count),
                file_count: fc,
                folder_count: folders_count,
                categories,
                modified: max_mod,
                protected: n.is_protected(),
            });
        } else if files.len() < FILES_CAP {
            let cat = n.category();
            files.push(FileTile {
                id: child,
                name,
                size: n.on_disk,
                category: cat.label().to_string(),
                category_color: cat.color(),
                modified: n.modified,
                protected: n.is_protected(),
                cloud: n.is_cloud_placeholder(),
            });
        } else {
            files_capped = true;
        }
    }
    let n = tree.node(node).expect("validated by the command layer");
    let (file_count, folder_count) = tree
        .dir_extras
        .get(n.dir_index as usize)
        .map_or((0, 0), |e| (e.file_count, e.folder_count));
    FolderView {
        generation: tree.generation,
        node,
        name: tree.name(node),
        size: n.on_disk,
        file_count,
        folder_count,
        folders,
        files,
        files_capped,
    }
}

/// Fetch the Folders-mode dataset (command wrapper).
///
/// # Errors
/// String error when the generation is stale or the node is missing.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn get_folder_view(
    generation: u64,
    node: u32,
    filter: Option<String>,
    state: State<'_, AppState>,
) -> Result<FolderView, String> {
    let tree = resolve(&state, generation, node)?;
    Ok(compute_folder_view(
        &tree,
        node,
        filter.as_deref().unwrap_or(""),
    ))
}

/// One "Largest Inside" row (spec §8).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LargestItem {
    pub id: u32,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// The §8 inspector dataset.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDetails {
    pub id: u32,
    pub name: String,
    pub is_dir: bool,
    /// "Folder" or the category label.
    pub kind: String,
    /// `0xRRGGBB` category/dominant color.
    pub kind_color: u32,
    /// Full display path.
    pub path: String,
    /// Big size text value = size on disk.
    pub size: u64,
    /// "x.x% of scan".
    pub share_of_scan: f64,
    /// Logical size.
    pub logical: u64,
    /// on_disk − logical when positive (cluster overhead).
    pub overhead: u64,
    /// logical − on_disk when positive (compressed/sparse savings).
    pub savings: u64,
    /// Descendant counts (folders only; files use `kind`).
    pub files: u64,
    pub folders: u64,
    /// "Of parent %".
    pub of_parent: f64,
    /// Unix seconds (0 = unknown).
    pub modified: i64,
    pub created: i64,
    pub is_cloud: bool,
    pub is_protected: bool,
    /// Largest Inside ranked children.
    pub largest: Vec<LargestItem>,
}

/// Inspector computation (pure — the real logic, host-testable).
#[must_use]
pub fn compute_details(tree: &Tree, id: u32) -> NodeDetails {
    let n = tree.node(id).expect("validated by the command layer");
    let root_total = tree.node(tree.root).map_or(0, |r| r.on_disk);
    let (files, folders) = tree
        .dir_extras
        .get(n.dir_index as usize)
        .filter(|_| n.is_dir())
        .map_or((0, 0), |e| (e.file_count, e.folder_count));
    let cat = if n.is_dir() {
        tree.dominant_category(id)
    } else {
        n.category()
    };
    let largest: Vec<LargestItem> = tree
        .children_sorted(id)
        .iter()
        .filter_map(|&c| tree.node(c).map(|cn| (c, cn)))
        .filter(|(_, cn)| !cn.is_removed())
        .take(LARGEST_CAP)
        .map(|(c, cn)| LargestItem {
            id: c,
            name: tree.name(c),
            size: cn.on_disk,
            is_dir: cn.is_dir(),
        })
        .collect();
    let path = tree.node_path(id);
    // Drive roots read as "Disk" in the inspector (a path-shaped X:\
    // check — "This PC" and real folders stay "Folder").
    let is_drive = n.is_dir()
        && path.len() <= 3
        && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && path.as_bytes().get(1) == Some(&b':');
    NodeDetails {
        id,
        name: tree.name(id),
        is_dir: n.is_dir(),
        kind: if is_drive {
            "Disk".to_string()
        } else if n.is_dir() {
            "Folder".to_string()
        } else {
            cat.label().to_string()
        },
        kind_color: cat.color(),
        path,
        size: n.on_disk,
        share_of_scan: if root_total > 0 {
            n.on_disk as f64 / root_total as f64
        } else {
            0.0
        },
        logical: n.logical,
        overhead: tree.cluster_overhead(id),
        savings: tree.compression_savings(id),
        files,
        folders,
        of_parent: tree.share_of_parent(id),
        modified: n.modified,
        created: n.created,
        is_cloud: n.is_cloud_placeholder(),
        is_protected: n.is_protected(),
        largest,
    }
}

/// Inspector dataset command (spec §8, full field list).
///
/// # Errors
/// String error when the generation is stale or the node is missing.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn node_details(
    generation: u64,
    id: u32,
    state: State<'_, AppState>,
) -> Result<NodeDetails, String> {
    let tree = resolve(&state, generation, id)?;
    Ok(compute_details(&tree, id))
}

/// Top Sizes scope (spec §7.7 picker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TopScope {
    /// In this folder (direct children ranked).
    InFolder = 0,
    /// Biggest files anywhere in the subtree.
    FilesAnywhere = 1,
    /// Biggest folders anywhere in the subtree.
    FoldersAnywhere = 2,
}

impl TopScope {
    /// Parse the kebab-case scope id from JS.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "in-folder" => Some(Self::InFolder),
            "files-anywhere" => Some(Self::FilesAnywhere),
            "folders-anywhere" => Some(Self::FoldersAnywhere),
            _ => None,
        }
    }

    /// The kebab-case id echoed to JS.
    #[must_use]
    pub fn as_id(self) -> &'static str {
        match self {
            Self::InFolder => "in-folder",
            Self::FilesAnywhere => "files-anywhere",
            Self::FoldersAnywhere => "folders-anywhere",
        }
    }
}

/// One ranked row (spec §7.7).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopRow {
    pub rank: u32,
    pub id: u32,
    pub name: String,
    /// Parent display path ("anywhere" scopes; empty otherwise).
    pub parent_path: String,
    pub size: u64,
    /// "N files" for folders; category label for files.
    pub kind: String,
    pub is_dir: bool,
    /// % of the folder.
    pub share: f64,
    /// `0xRRGGBB`.
    pub color: u32,
}

/// The Top Sizes dataset.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopSizes {
    pub generation: u64,
    pub node: u32,
    /// Scope id echoed.
    pub scope: String,
    pub rows: Vec<TopRow>,
    /// Total on-disk bytes of the folder (100% reference).
    pub total: u64,
}

/// Top-200 computation (pure — spec §7.7 "top 200").
#[must_use]
pub fn compute_top(tree: &Tree, node: u32, scope: TopScope, filter: &str) -> TopSizes {
    let total = tree.node(node).map_or(0, |n| n.on_disk);
    let needle = filter.trim().to_ascii_lowercase();
    let mut rows: Vec<TopRow> = Vec::new();
    let push_row = |rows: &mut Vec<TopRow>, tree: &Tree, id: u32, rank: u32, parent: String| {
        let Some(n) = tree.node(id) else { return };
        if n.is_removed() || n.on_disk == 0 {
            return;
        }
        let name = tree.name(id);
        if !needle.is_empty() && !name.to_ascii_lowercase().contains(&needle) {
            return;
        }
        let files = tree
            .dir_extras
            .get(n.dir_index as usize)
            .filter(|_| n.is_dir())
            .map_or(0, |e| e.file_count);
        rows.push(TopRow {
            rank,
            id,
            name,
            parent_path: parent,
            size: n.on_disk,
            kind: if n.is_dir() {
                format!("{files} files")
            } else {
                n.category().label().to_string()
            },
            is_dir: n.is_dir(),
            share: if total > 0 {
                n.on_disk as f64 / total as f64
            } else {
                0.0
            },
            color: if n.is_dir() {
                0x93C5FD
            } else {
                n.category().color()
            },
        });
    };
    match scope {
        TopScope::InFolder => {
            for (i, &child) in tree.children_sorted(node).iter().enumerate() {
                if rows.len() >= TOP_CAP {
                    break;
                }
                push_row(&mut rows, tree, child, i as u32 + 1, String::new());
            }
        }
        TopScope::FilesAnywhere | TopScope::FoldersAnywhere => {
            let want_dirs = scope == TopScope::FoldersAnywhere;
            let mut candidates: Vec<(u64, u32)> = Vec::new();
            tree.walk(node, |id, n| {
                if n.is_dir() == want_dirs && !n.is_removed() && n.on_disk > 0 {
                    if want_dirs && id == node {
                        return; // the scope folder itself is the frame, not a row
                    }
                    candidates.push((n.on_disk, id));
                }
            });
            candidates.sort_unstable_by_key(|c| Reverse(c.0));
            let mut rank = 0u32;
            for (_, id) in candidates {
                if rank as usize >= TOP_CAP {
                    break;
                }
                let parent = tree
                    .node(id)
                    .and_then(|n| {
                        if n.parent == u32::MAX {
                            None
                        } else {
                            Some(tree.node_path(n.parent))
                        }
                    })
                    .unwrap_or_default();
                rank += 1;
                push_row(&mut rows, tree, id, rank, parent);
            }
            // Re-rank after filter drops.
            for (i, r) in rows.iter_mut().enumerate() {
                r.rank = i as u32 + 1;
            }
        }
    }
    TopSizes {
        generation: tree.generation,
        node,
        scope: scope.as_id().to_string(),
        rows,
        total,
    }
}

/// Cache key for the background rankings.
type TopKey = (u64, u32, u8);

/// Top-200 caches ("computed on a background thread in Rust and cached
/// per folder and scan generation" — spec §7.7).
#[derive(Default)]
pub struct TopCache {
    /// Finished rankings (filter-less full result; the live filter is a
    /// client-side re-rank over the cached copy).
    done: parking_lot::Mutex<HashMap<TopKey, Arc<TopSizes>>>,
    /// In-flight computation marks.
    inflight: parking_lot::Mutex<std::collections::HashSet<TopKey>>,
}

/// Managed state constructor.
#[must_use]
pub fn top_cache() -> TopCache {
    TopCache::default()
}

impl TopCache {
    /// Clear everything (scan swap path — generation changed).
    pub fn clear(&self) {
        self.done.lock().clear();
        self.inflight.lock().clear();
    }
}

/// Top Sizes command: serves the cache when warm; otherwise computes the
/// ranking on the blocking pool (spec §7.7 background thread) and caches
/// the unfiltered result per (generation, node, scope). The live name
/// filter re-ranks the cached copy (cheap, client-visible consistency).
///
/// # Errors
/// String error when the generation is stale, the scope id is unknown,
/// or the node is missing.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub async fn top_sizes(
    generation: u64,
    node: u32,
    scope: String,
    filter: Option<String>,
    state: State<'_, AppState>,
    cache: State<'_, TopCache>,
) -> Result<TopSizes, String> {
    let scope = TopScope::parse(&scope).ok_or_else(|| format!("unknown scope {scope}"))?;
    let arc = resolve(&state, generation, node)?;
    let key: TopKey = (generation, node, scope as u8);
    if let Some(hit) = cache.done.lock().get(&key).cloned() {
        return Ok(apply_filter(Arc::unwrap_or_clone(hit), filter.as_deref()));
    }
    // Compute on the blocking pool; one in-flight computation per key.
    let first = {
        let mut inflight = cache.inflight.lock();
        inflight.insert(key)
    };
    let result = if first {
        let computed =
            tauri::async_runtime::spawn_blocking(move || compute_top(&arc, node, scope, ""))
                .await
                .map_err(|e| format!("ranking thread failed: {e}"))?;
        cache.done.lock().insert(key, Arc::new(computed.clone()));
        cache.inflight.lock().remove(&key);
        computed
    } else {
        // A concurrent caller is computing; serve a direct computation so
        // this request is correct without inventing a wait state.
        compute_top(&arc, node, scope, "")
    };
    Ok(apply_filter(result, filter.as_deref()))
}

/// Re-rank a cached/unfiltered result under the live name filter.
fn apply_filter(mut tops: TopSizes, filter: Option<&str>) -> TopSizes {
    let Some(f) = filter else { return tops };
    let needle = f.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return tops;
    }
    tops.rows
        .retain(|r| r.name.to_ascii_lowercase().contains(&needle));
    for (i, r) in tops.rows.iter_mut().enumerate() {
        r.rank = i as u32 + 1;
    }
    tops
}

/// One Big & Untouched row (spec §7.8).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BigRow {
    pub id: u32,
    pub name: String,
    pub path: String,
    pub logical: u64,
    pub on_disk: u64,
    pub modified: i64,
    pub protected: bool,
}

/// The Age Map dataset.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgeMapData {
    pub generation: u64,
    pub node: u32,
    /// 6 bucket byte totals.
    pub buckets: [u64; 6],
    /// Bucket labels in order.
    pub bucket_labels: [String; 6],
    /// Total bytes considered.
    pub total: u64,
    /// Year × month heatmap.
    pub heatmap: age::MonthHeatmap,
    /// Big & Untouched rows, largest first.
    pub big: Vec<BigRow>,
}

/// Age Map computation (pure; now injected for tests).
#[must_use]
pub fn compute_age_map(tree: &Tree, node: u32, now: i64) -> AgeMapData {
    let buckets = age::bucket_totals(tree, node, now);
    let heatmap = age::month_heatmap(tree, node);
    let big: Vec<BigRow> = age::big_untouched(tree, node, now, BIG_CAP)
        .into_iter()
        .map(|f| BigRow {
            id: f.id,
            name: tree.name(f.id),
            path: tree.node_path(f.id),
            logical: f.logical,
            on_disk: f.on_disk,
            modified: f.modified,
            protected: tree
                .node(f.id)
                .is_some_and(diskgenie_core::scan::node::Node::is_protected),
        })
        .collect();
    AgeMapData {
        generation: tree.generation,
        node,
        buckets: buckets.bytes,
        bucket_labels: std::array::from_fn(|i| age::BUCKETS[i].to_string()),
        total: buckets.total,
        heatmap,
        big,
    }
}

/// Age Map command (spec §7.8) — computed on the blocking pool, cached
/// per (generation, node).
///
/// # Errors
/// String error when the generation is stale or the node is missing.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub async fn age_map(
    generation: u64,
    node: u32,
    state: State<'_, AppState>,
    cache: State<'_, AgeCache>,
) -> Result<AgeMapData, String> {
    let arc = resolve(&state, generation, node)?;
    let key = (generation, node);
    if let Some(hit) = cache.done.lock().get(&key).cloned() {
        return Ok(Arc::unwrap_or_clone(hit));
    }
    let result = tauri::async_runtime::spawn_blocking(move || {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        compute_age_map(&arc, node, now)
    })
    .await
    .map_err(|e| format!("age-map thread failed: {e}"))?;
    cache.done.lock().insert(key, Arc::new(result.clone()));
    Ok(result)
}

/// Age Map cache (per generation + node).
#[derive(Default)]
pub struct AgeCache {
    done: parking_lot::Mutex<HashMap<(u64, u32), Arc<AgeMapData>>>,
}

/// Managed state constructor.
#[must_use]
pub fn age_cache() -> AgeCache {
    AgeCache::default()
}

impl AgeCache {
    /// Clear everything (scan swap path).
    pub fn clear(&self) {
        self.done.lock().clear();
    }
}

/// One List-mode outline row (spec §7.9).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
// A row DTO mirrors the node flags; the bools are independent facets
// (kind/expandable/stageability/cloud), not a state machine.
#[allow(clippy::struct_excessive_bools)]
pub struct ListRow {
    pub id: u32,
    pub name: String,
    pub is_dir: bool,
    /// Only non-empty folders get disclosure triangles.
    pub has_children: bool,
    /// Share of parent (mini bar width).
    pub share: f64,
    pub size: u64,
    /// Category label (kind caption for files).
    pub category: String,
    /// Direct children count (folders only; files report 0). Powers
    /// the List-mode "Items" column — the UI header promised it, the
    /// rows never carried it (bytes rendered under "Items" and the
    /// Size column sat empty).
    pub items: u32,
    /// `0xRRGGBB`.
    pub color: u32,
    pub protected: bool,
    pub cloud: bool,
    pub modified: i64,
}

/// List-mode computation (pure — the real logic, host-testable).
#[must_use]
pub fn compute_list(tree: &Tree, node: u32, filter: &str) -> Vec<ListRow> {
    let needle = filter.trim().to_ascii_lowercase();
    let mut rows = Vec::new();
    for &child in tree.children_sorted(node) {
        if rows.len() >= LIST_CAP {
            break;
        }
        let Some(n) = tree.node(child) else { continue };
        if n.is_removed() {
            continue;
        }
        let name = tree.name(child);
        if !needle.is_empty() && !name.to_ascii_lowercase().contains(&needle) {
            continue;
        }
        let cat = if n.is_dir() {
            tree.dominant_category(child)
        } else {
            n.category()
        };
        rows.push(ListRow {
            id: child,
            name,
            is_dir: n.is_dir(),
            has_children: n.is_dir() && n.child_count > 0,
            share: tree.share_of_parent(child),
            size: n.on_disk,
            category: if n.is_dir() {
                "Folder".to_string()
            } else {
                cat.label().to_string()
            },
            items: n.child_count,
            color: cat.color(),
            protected: n.is_protected(),
            cloud: n.is_cloud_placeholder(),
            modified: n.modified,
        });
    }
    rows
}

/// List-mode children command (spec §7.9: virtualized outline, cap 500
/// children per level; name filter applies).
///
/// # Errors
/// String error when the generation is stale or the node is missing.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn list_children(
    generation: u64,
    node: u32,
    filter: Option<String>,
    state: State<'_, AppState>,
) -> Result<Vec<ListRow>, String> {
    let tree = resolve(&state, generation, node)?;
    Ok(compute_list(&tree, node, filter.as_deref().unwrap_or("")))
}

/// Breadcrumb entry (folder navigation, §3 top bar).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Crumb {
    pub id: u32,
    pub name: String,
    pub size: u64,
}

/// Breadcrumb chain from the scan root to `node` (pure).
#[must_use]
pub fn compute_breadcrumb(tree: &Tree, node: u32) -> Vec<Crumb> {
    let mut chain: Vec<u32> = Vec::new();
    let mut id = node;
    loop {
        chain.push(id);
        let Some(n) = tree.node(id) else { break };
        if n.parent == u32::MAX {
            break;
        }
        id = n.parent;
    }
    chain.reverse();
    chain
        .into_iter()
        .map(|id| Crumb {
            id,
            name: tree.name(id),
            size: tree.node(id).map_or(0, |n| n.on_disk),
        })
        .collect()
}

/// Breadcrumb command.
///
/// # Errors
/// String error when the generation is stale or the node is missing.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn get_breadcrumb(
    generation: u64,
    node: u32,
    state: State<'_, AppState>,
) -> Result<Vec<Crumb>, String> {
    let tree = resolve(&state, generation, node)?;
    Ok(compute_breadcrumb(&tree, node))
}

/// Shared stale-tree guard (moved to `commands::resolve_node` so the
/// other command modules share the exact wire strings; local alias keeps
/// the call sites unchanged).
use super::resolve_node as resolve;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{dir_entry, file_entry};

    fn build_tree() -> Tree {
        let mut t = Tree::new(7);
        t.add_root_path(0, "C:\\Base");
        // Mirrors production `build_root`: the root node carries a real
        // display name (Folders header / breadcrumb / inspector).
        t.set_name(0, "Base");
        t.append_batch(
            0,
            vec![
                dir_entry("alpha"),
                dir_entry("beta"),
                file_entry("root.bin", 10, 1),
            ],
        );
        t.append_batch(
            1,
            vec![file_entry("a1.mp4", 100, 1), file_entry("a2.txt", 20, 2)],
        );
        t.append_batch(2, vec![file_entry("b1.bin", 5, 3)]);
        diskgenie_core::scan::rollup::finalize(&mut t);
        t
    }

    #[test]
    fn folder_view_splits_folders_and_files() {
        let t = build_tree();
        let v = compute_folder_view(&t, 0, "");
        assert_eq!(v.folders.len(), 2);
        assert_eq!(v.files.len(), 1);
        assert_eq!(v.files[0].name, "root.bin");
        assert_eq!(v.folders[0].name, "alpha"); // largest first
        assert_eq!(v.folders[0].item_count, 2);
        assert_eq!(v.folders[0].categories.len(), 2); // Video + Document
        assert_eq!(v.name, "Base");
        assert_eq!(v.file_count, 4);
        assert_eq!(v.folder_count, 2);
        assert!(!v.files_capped);
    }

    #[test]
    fn folder_view_filter_case_insensitive() {
        let t = build_tree();
        let v = compute_folder_view(&t, 0, "ALPH");
        assert_eq!(v.folders.len(), 1);
        assert!(v.files.is_empty());
    }

    #[test]
    fn top_in_folder_ranks_children() {
        let t = build_tree();
        let tops = compute_top(&t, 0, TopScope::InFolder, "");
        assert_eq!(tops.rows.len(), 3);
        assert_eq!(tops.rows[0].id, 1); // alpha 120
        assert_eq!(tops.rows[0].rank, 1);
        assert!(tops.rows[0].share > 0.8);
        assert_eq!(tops.rows[0].kind, "2 files");
        assert_eq!(tops.rows[0].parent_path, "");
    }

    #[test]
    fn top_files_anywhere_walks_subtree_and_filters() {
        let t = build_tree();
        let tops = compute_top(&t, 0, TopScope::FilesAnywhere, "");
        assert_eq!(tops.rows.len(), 4);
        assert_eq!(tops.rows[0].name, "a1.mp4");
        assert_eq!(tops.rows[0].size, 100);
        assert!(tops.rows[0].parent_path.ends_with("alpha"));
        // "b1" — deliberately NOT "b": "root.bin" also contains a
        // bare "b", which the old assertion wrongly counted.
        let filtered = compute_top(&t, 0, TopScope::FilesAnywhere, "b1");
        assert_eq!(filtered.rows.len(), 1); // b1.bin only
        assert_eq!(filtered.rows[0].name, "b1.bin");
        assert_eq!(filtered.rows[0].rank, 1);
    }

    #[test]
    fn top_folders_anywhere_excludes_scope_node() {
        let t = build_tree();
        let tops = compute_top(&t, 0, TopScope::FoldersAnywhere, "");
        let ids: Vec<u32> = tops.rows.iter().map(|r| r.id).collect();
        assert!(!ids.contains(&0), "scope node must not be a row");
        assert_eq!(tops.rows[0].id, 1); // alpha
        assert!(tops.rows[0].parent_path.ends_with("Base"));
    }

    #[test]
    fn cached_filter_re_rank() {
        let t = build_tree();
        let mut tops = compute_top(&t, 0, TopScope::FilesAnywhere, "");
        assert_eq!(tops.rows.len(), 4);
        tops = apply_filter(tops, Some("a"));
        assert_eq!(tops.rows.len(), 2); // names containing 'a': a1.mp4, a2.txt
        for (i, r) in tops.rows.iter().enumerate() {
            assert_eq!(r.rank, i as u32 + 1);
        }
        let fresh = compute_top(&t, 0, TopScope::FilesAnywhere, "");
        let unfiltered = apply_filter(fresh, None);
        assert_eq!(unfiltered.rows.len(), 4); // None = untouched copy
    }

    #[test]
    fn list_children_sorted_with_triangles() {
        let t = build_tree();
        let rows = compute_list(&t, 0, "");
        assert_eq!(rows.len(), 3);
        assert!(rows[0].size >= rows[1].size);
        // Sorted by size: alpha 120 > root.bin 10 > beta 5.
        assert_eq!(rows[0].id, 1);
        assert!(rows[0].has_children); // alpha (a1, a2)
        assert_eq!(rows[1].id, 3);
        assert!(!rows[1].is_dir && !rows[1].has_children); // root.bin
        assert_eq!(rows[2].id, 2);
        assert!(rows[2].has_children); // beta (b1.bin)
        let filtered = compute_list(&t, 0, "beta");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, 2);
    }

    #[test]
    fn details_fields_complete() {
        let t = build_tree();
        let d = compute_details(&t, 1);
        assert_eq!(d.name, "alpha");
        assert_eq!(d.kind, "Folder");
        assert!(d.path.ends_with("alpha"));
        assert_eq!(d.files, 2);
        assert_eq!(d.folders, 0);
        assert_eq!(d.largest.len(), 2);
        assert_eq!(d.largest[0].name, "a1.mp4");
        assert!((d.share_of_scan - 120.0 / 135.0).abs() < 1e-9);
        assert!((d.of_parent - 120.0 / 135.0).abs() < 1e-9);
    }

    #[test]
    fn details_drive_root_reads_as_disk() {
        // A This-PC scan: each drive node carries a root path ("C:\"),
        // exactly like production build_root. The drive reads as "Disk"
        // in the inspector; "This PC" and deeper folders stay "Folder".
        let mut t = Tree::new(11);
        t.add_root_path(0, "This PC");
        t.set_name(0, "This PC");
        t.append_batch(
            0,
            vec![dir_entry("Local Disk (C:)"), dir_entry("Data (D:)")],
        );
        t.append_batch(
            1,
            vec![dir_entry("Users"), file_entry("pagefile.sys", 5, 1)],
        );
        // Production parity: drives are path roots (node_path stops here).
        t.add_root_path(1, "C:\\");
        t.add_root_path(2, "D:\\");
        diskgenie_core::scan::rollup::finalize(&mut t);
        assert_eq!(compute_details(&t, 0).kind, "Folder"); // This PC stays Folder
        assert_eq!(compute_details(&t, 1).kind, "Disk");
        assert_eq!(compute_details(&t, 2).kind, "Disk");
        assert_eq!(compute_details(&t, 3).kind, "Folder"); // Users stays Folder
    }

    #[test]
    fn age_map_buckets_and_big_rows() {
        let t = build_tree();
        let now = 1_800_000_000;
        let d = compute_age_map(&t, 0, now);
        assert_eq!(d.total, 135);
        assert_eq!(d.buckets[5], 135); // all mtimes are ~57y old
        assert_eq!(d.big.len(), 0); // nothing ≥ 40 MB here
        assert_eq!(d.bucket_labels.len(), 6);
        assert_eq!(d.bucket_labels[0], "Last 7 days");
    }

    #[test]
    fn breadcrumb_root_to_node() {
        let t = build_tree();
        // Node 4 = a1.mp4 (0 Base → 1 alpha → 4 a1.mp4); node 3 is
        // root.bin, one hop from the root.
        let crumbs = compute_breadcrumb(&t, 4);
        assert_eq!(crumbs.len(), 3);
        assert_eq!(crumbs[0].name, "Base");
        assert_eq!(crumbs[1].name, "alpha");
        assert_eq!(crumbs[2].name, "a1.mp4");
    }

    #[test]
    fn scope_parse_roundtrip() {
        assert_eq!(TopScope::parse("in-folder"), Some(TopScope::InFolder));
        assert_eq!(
            TopScope::parse("files-anywhere"),
            Some(TopScope::FilesAnywhere)
        );
        assert_eq!(
            TopScope::parse("folders-anywhere"),
            Some(TopScope::FoldersAnywhere)
        );
        assert_eq!(TopScope::parse("nonsense"), None);
        assert_eq!(TopScope::InFolder.as_id(), "in-folder");
    }
}
