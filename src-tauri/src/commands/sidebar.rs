//! Sidebar commands (spec §6; doc 03 M6): drives, home, disk storage,
//! elevation state, elevated relaunch, Quick Wins (cached per
//! generation), Quick Wins item paths, and the File Types bar data.
//!
//! Quick Wins categories are computed from the ALREADY-BUILT tree (no
//! extra disk pass — spec §6.7) via `core::quickwins::resolve` with the
//! known-folder env roots resolved through the platform seam.

use std::collections::HashMap;
use std::sync::Arc;

use diskgenie_core::platform::Platform;
use diskgenie_core::quickwins;
use diskgenie_core::scan::categories::FileCategory;
use diskgenie_core::scan::node::Tree;
use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::platform::HostPlatform;
use crate::state::AppState;

/// One drive chip (spec §6.3).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveChip {
    /// Display letter, e.g. `C:`.
    pub letter: String,
    /// Scan target for this drive (`C:\`).
    pub target: String,
}

/// Fixed-drive chips (spec §6.3 — one per fixed drive).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn get_drive_chips(platform: State<'_, Arc<HostPlatform>>) -> Vec<DriveChip> {
    (*platform)
        .fixed_drive_roots()
        .iter()
        .filter_map(|root| {
            // `\\?\C:\` → letter `C:`.
            let trimmed = root.trim_start_matches(r"\\?\");
            let bytes = trimmed.as_bytes();
            if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                Some(DriveChip {
                    letter: format!("{}:", bytes[0] as char),
                    target: format!("{}:\\", bytes[0] as char),
                })
            } else {
                None
            }
        })
        .collect()
}

/// The user profile path (the Home button — spec §6.2).
///
/// # Errors
/// String error when the known folder cannot be resolved.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn get_home_path(platform: State<'_, Arc<HostPlatform>>) -> Result<String, String> {
    (*platform)
        .known_folder(diskgenie_core::platform::KnownFolder::Profile)
        .ok_or_else(|| "Couldn't resolve your user profile folder.".into())
}

/// Resolve a display path to a node id in the CURRENT tree, so the Home
/// button can NAVIGATE when the path is inside the last scan (no
/// rescan) and only start a new scan when it isn't. Returns `None`
/// (not an error) when there is no tree, the generation is stale, or
/// the path is outside the scan — the caller decides what to do.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn resolve_path(generation: u64, path: String, state: State<'_, AppState>) -> Option<u32> {
    let guard = state.tree.read();
    let tree = guard.as_ref()?;
    if tree.generation != generation {
        return None;
    }
    tree.resolve_display_path(&path)
}

/// The disk storage snapshot (spec §6.5): the volume containing the
/// current VIEW (session 15 — the sidebar follows navigation, so a
/// C:↔D: flip swaps the card), the scan root when no view path is
/// offered, the system drive when there is no scan.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageInfo {
    pub label: String,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    /// Fraction used (0..1).
    pub used_pct: f64,
}

/// Split a display path into `\`-`/` segments (the core
/// `resolve_display_path` convention — matching must agree with the
/// path resolver that drives navigation).
fn path_segments(path: &str) -> Vec<String> {
    path.split(['\\', '/'])
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The longest tree root whose segments prefix `path` (case-insensitive
/// — `eq_ignore_ascii_case`, exactly like `Tree::resolve_display_path`).
fn containing_root<'a>(roots: &'a [String], path: &str) -> Option<&'a String> {
    let incoming = path_segments(path);
    let mut best: Option<(usize, &String)> = None;
    for root in roots {
        let root_segs = path_segments(root);
        if root_segs.is_empty() || root_segs.len() > incoming.len() {
            continue;
        }
        if root_segs
            .iter()
            .zip(&incoming)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
            && best.map_or(true, |(l, _)| root_segs.len() > l)
        {
            best = Some((root_segs.len(), root));
        }
    }
    best.map(|(_, r)| r)
}

/// Volume label for the card: bare on single-root scans (the reference
/// design's "Macintosh HD"); on multi-root trees two volumes can share
/// a label ("Local Disk" ×2), so the drive letter rides along.
fn volume_label(raw: &str, root: &str, multi_root: bool) -> String {
    let label = raw.trim_end_matches('\0').trim();
    if !multi_root {
        return label.to_string();
    }
    // The letter from the root's first segment ("C:" from "C:\").
    let letter = root
        .split(['\\', '/'])
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_default();
    if letter.is_empty() {
        label.to_string()
    } else {
        format!("{label} ({letter})")
    }
}

/// Read the storage snapshot for the CURRENT VIEW (`path` from the
/// sidebar's view-location store) — the volume containing that path.
/// The whole-PC view (the virtual root label, or any path outside the
/// tree) aggregates every root of the standing tree instead, so the
/// card answers "how full is what I'm looking at" at every level.
///
/// # Errors
/// String error when the volume cannot be queried.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn disk_storage(
    path: Option<String>,
    state: State<'_, AppState>,
) -> Result<StorageInfo, String> {
    let roots: Vec<String> = {
        let guard = state.tree.read();
        guard
            .as_ref()
            .map(|t| t.roots.iter().map(|r| r.path.clone()).collect())
            .unwrap_or_default()
    };
    // The view's volume when the path resolves inside the tree…
    if let Some(view) = path.as_deref() {
        if let Some(root) = containing_root(&roots, view) {
            let snap = crate::platform::os::disk_storage(root)
                .ok_or_else(|| "Couldn't read this volume's free space.".to_string())?;
            let label = volume_label(
                &String::from_utf16_lossy(&snap.label.0),
                root,
                roots.len() > 1,
            );
            return Ok(StorageInfo {
                label,
                total: snap.total,
                used: snap.used,
                free: snap.free,
                used_pct: if snap.total > 0 {
                    snap.used as f64 / snap.total as f64
                } else {
                    0.0
                },
            });
        }
    }
    // …the multi-root aggregate for the whole-PC view (and any path the
    // tree doesn't cover): sums across every probeable root, labelled
    // with the app's own whole-scan name. A root that fails to probe
    // (an unplugged removable) is skipped, not fatal — the card stays
    // honest for the volumes that are there.
    if roots.len() > 1 {
        let mut total = 0u64;
        let mut used = 0u64;
        let mut free = 0u64;
        let mut probed = 0usize;
        for root in &roots {
            if let Some(snap) = crate::platform::os::disk_storage(root) {
                total = total.saturating_add(snap.total);
                used = used.saturating_add(snap.used);
                free = free.saturating_add(snap.free);
                probed += 1;
            }
        }
        if probed > 0 {
            return Ok(StorageInfo {
                label: diskgenie_core::scan::scanner::this_pc_display_label().to_string(),
                total,
                used,
                free,
                used_pct: if total > 0 {
                    used as f64 / total as f64
                } else {
                    0.0
                },
            });
        }
        return Err("Couldn't read this volume's free space.".into());
    }
    // Single root / no view path / no tree: the standing root's volume
    // (the pre-session-15 behavior), else the system drive (spec §6.5).
    let probe = roots.first().cloned().unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            "/".to_string()
        } else {
            std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())
        }
    });
    let snap = crate::platform::os::disk_storage(&probe)
        .ok_or_else(|| "Couldn't read this volume's free space.".to_string())?;
    let label = String::from_utf16_lossy(&snap.label.0);
    Ok(StorageInfo {
        label: volume_label(&label, &probe, false),
        total: snap.total,
        used: snap.used,
        free: snap.free,
        used_pct: if snap.total > 0 {
            snap.used as f64 / snap.total as f64
        } else {
            0.0
        },
    })
}

/// True when running elevated (hides the restart-as-admin button).
#[tauri::command]
pub fn is_elevated() -> bool {
    crate::platform::os::is_elevated()
}

/// Restart as administrator and re-run the same scan (spec §6.4/§7):
/// ShellExecuteW "runas" with `--scan <target>`, then exit this
/// instance so only the elevated window remains.
///
/// # Errors
/// String error when elevation is declined or the launch fails.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // AppHandle is the tauri command contract
pub fn restart_as_admin(scan_target: &str, turbo: Option<bool>, app: AppHandle) {
    // The elevated instance takes over; this one exits (restart never
    // returns). Failures surface as a user-readable error first.
    let run = |args: String| crate::platform::os::relaunch_elevated_with(scan_target, &args);
    let result = if turbo.unwrap_or(false) {
        run("--turbo".into())
    } else {
        run(String::new())
    };
    match result {
        // Exit cleanly — do NOT `tauri::process::restart`, which would
        // relaunch a second, still-unelevated copy alongside the new one.
        Ok(()) => app.exit(0),
        Err(reason) => {
            let _ = tauri::Emitter::emit(&app, "admin-restart-failed", reason);
        }
    }
}

/// One Quick Wins row (spec §6.7).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickWinRow {
    pub id: String,
    pub title: String,
    pub icon: String,
    pub count: u64,
    pub size: u64,
    /// Review-only rows refuse "Add all" (VM disks, Windows.old).
    pub review_only: bool,
    /// Context line (e.g. the ms-settings link hint for Windows.old).
    pub extra: Option<String>,
    /// The biggest match (row click navigates there — spec §6.7).
    pub biggest_match: Option<u32>,
}

/// Quick Wins for the current tree (cached per generation; spec §6.7:
/// computed from the already-built tree, no extra disk pass).
///
/// # Errors
/// String error when no scan exists yet or the generation is stale.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub async fn quick_wins(
    generation: u64,
    state: State<'_, AppState>,
    platform: State<'_, Arc<HostPlatform>>,
    cache: State<'_, QuickWinsCache>,
) -> Result<Vec<QuickWinRow>, String> {
    let tree = super::resolve_tree(&state, generation)?;
    if let Some(hit) = cache.done.lock().get(&generation) {
        return Ok(Arc::clone(hit).as_ref().clone());
    }
    let env_roots = env_roots(**platform);
    let rows = tauri::async_runtime::spawn_blocking(move || {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        compute_quick_wins(&tree, &env_roots, now)
    })
    .await
    .map_err(|e| format!("quick-wins thread failed: {e}"))?;
    cache.done.lock().insert(generation, Arc::new(rows.clone()));
    Ok(rows)
}

/// Quick Wins cache (per generation — cleared implicitly by keying).
#[derive(Default)]
pub struct QuickWinsCache {
    done: Mutex<HashMap<u64, Arc<Vec<QuickWinRow>>>>,
}

/// Managed state constructor.
#[must_use]
pub fn quick_wins_cache() -> QuickWinsCache {
    QuickWinsCache::default()
}

impl QuickWinsCache {
    /// Clear (scan-swap / surgery path; called cross-module).
    pub fn clear_pub(&self) {
        self.done.lock().clear();
    }
}

/// The pure computation (host-testable shape: env roots injected).
fn compute_quick_wins(
    tree: &Tree,
    env_roots: &HashMap<String, String>,
    now: i64,
) -> Vec<QuickWinRow> {
    quickwins::resolve(tree, env_roots, now)
        .into_iter()
        .map(|c| QuickWinRow {
            id: c.id.to_string(),
            title: c.title.to_string(),
            icon: c.icon.to_string(),
            count: c.items.len() as u64,
            size: c.size,
            review_only: c.review_only,
            extra: c.extra.map(std::string::ToString::to_string),
            biggest_match: c.items.first().copied(),
        })
        .collect()
}

/// Known-folder env roots for the matcher (spec §6 pattern table).
fn env_roots(platform: HostPlatform) -> HashMap<String, String> {
    let mut m: HashMap<String, String> = HashMap::new();
    let pairs = [
        (
            "%USERPROFILE%",
            diskgenie_core::platform::KnownFolder::Profile,
        ),
        (
            "%LOCALAPPDATA%",
            diskgenie_core::platform::KnownFolder::LocalAppData,
        ),
        (
            "%APPDATA%",
            diskgenie_core::platform::KnownFolder::RoamingAppData,
        ),
        (
            "%PROGRAMDATA%",
            diskgenie_core::platform::KnownFolder::ProgramData,
        ),
    ];
    for (key, folder) in pairs {
        if let Some(path) = platform.known_folder(folder) {
            m.insert(key.to_string(), path);
        }
    }
    if cfg!(target_os = "macos") {
        // Mac aliases for the Mac BuildPrompt §5 pattern table.
        if let Some(home) = platform.known_folder(diskgenie_core::platform::KnownFolder::Profile) {
            let support = format!("{home}/Library/Application Support");
            m.insert("%HOME%".into(), home);
            m.insert("%APP_SUPPORT%".into(), support);
        }
    }
    m
}

/// One staged-able Quick Wins item (for "Add all N to Cleanup").
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickWinItem {
    pub id: u32,
    pub path: String,
    pub size: u64,
}

/// The item list for a Quick Wins category (Add-all staging + Show in
/// Explorer targets).
///
/// # Errors
/// String error when no scan exists or the generation is stale.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn quick_win_items(
    generation: u64,
    category_id: String,
    state: State<'_, AppState>,
    platform: State<'_, Arc<HostPlatform>>,
) -> Result<Vec<QuickWinItem>, String> {
    let guard = state.tree.read();
    let Some(tree) = guard.as_ref() else {
        return Err("no scan yet".into());
    };
    if tree.generation != generation {
        return Err(format!(
            "stale generation {} (current {})",
            generation, tree.generation
        ));
    }
    let env_roots = env_roots(**platform);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let cats = quickwins::resolve(tree, &env_roots, now);
    let Some(cat) = cats.into_iter().find(|c| c.id == category_id) else {
        return Err(format!("unknown Quick Wins category {category_id}"));
    };
    Ok(cat
        .items
        .into_iter()
        .take(quickwins::CATEGORY_CAP)
        .map(|id| QuickWinItem {
            id,
            path: tree.node_path(id),
            size: tree.node(id).map_or(0, |n| n.on_disk),
        })
        .collect())
}

/// One File Types bar segment (spec §6.8).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeSegment {
    pub label: String,
    pub color: u32,
    pub size: u64,
}

/// The File Types stacked-bar data for the scan root.
///
/// # Errors
/// String error when no scan exists or the generation is stale.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn file_types(generation: u64, state: State<'_, AppState>) -> Result<Vec<TypeSegment>, String> {
    let guard = state.tree.read();
    let Some(tree) = guard.as_ref() else {
        return Err("no scan yet".into());
    };
    if tree.generation != generation {
        return Err(format!(
            "stale generation {} (current {})",
            generation, tree.generation
        ));
    }
    let root = tree.root;
    let Some(n) = tree.node(root) else {
        return Ok(Vec::new());
    };
    let sizes: [u64; 9] = tree
        .dir_extras
        .get(n.dir_index as usize)
        .map_or([0; 9], |e| e.type_sizes);
    let mut out: Vec<TypeSegment> = (0..9u8)
        .filter_map(|bits| {
            let cat = FileCategory::from_bits(bits);
            let size = sizes[bits as usize];
            (size > 0).then(|| TypeSegment {
                label: cat.label().to_string(),
                color: cat.color(),
                size,
            })
        })
        .collect();
    out.sort_unstable_by_key(|s| std::cmp::Reverse(s.size));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containing_root_matches_longest_prefix() {
        // The multi-root This-PC tree: a view path resolves to ITS drive.
        let roots = vec!["C:\\".to_string(), "D:\\".to_string()];
        assert_eq!(
            containing_root(&roots, "C:\\Users\\dev"),
            Some(&"C:\\".to_string())
        );
        assert_eq!(
            containing_root(&roots, "d:\\Games\\Steam"),
            Some(&"D:\\".to_string())
        );
        // The virtual-root LABEL matches nothing → the aggregate path.
        assert_eq!(containing_root(&roots, "This PC"), None);
    }

    #[test]
    fn containing_root_prefers_nested_folder_scan_root() {
        // A folder scan's root is itself the longest matching prefix.
        let roots = vec!["C:\\Users\\dev".to_string()];
        assert_eq!(
            containing_root(&roots, "C:\\Users\\dev\\Desktop"),
            Some(&"C:\\Users\\dev".to_string())
        );
        assert_eq!(
            containing_root(&roots, "C:\\Users\\dev"),
            Some(&"C:\\Users\\dev".to_string())
        );
        // Outside the scan (a different user's folder): no match.
        assert_eq!(containing_root(&roots, "C:\\Windows"), None);
    }

    #[test]
    fn volume_letter_only_on_multi_root() {
        // Single-root scans keep the bare label (the reference design).
        assert_eq!(volume_label("Local Disk", "C:\\", false), "Local Disk");
        // Multi-root trees disambiguate same-named volumes.
        assert_eq!(volume_label("Local Disk", "C:\\", true), "Local Disk (C:)");
        assert_eq!(volume_label("Games", "D:\\", true), "Games (D:)");
        // A label-less root degrades to the bare label.
        assert_eq!(volume_label("Games", "", true), "Games");
    }

    #[test]
    fn path_segments_split_both_separators() {
        assert_eq!(path_segments("C:\\Users\\dev"), vec!["C:", "Users", "dev"]);
        assert_eq!(path_segments("/Users/dev"), vec!["Users", "dev"]);
        assert_eq!(path_segments("This PC"), vec!["This PC"]);
    }

    #[test]
    fn quick_win_row_shape() {
        let row = QuickWinRow {
            id: "downloads".into(),
            title: "Downloads".into(),
            icon: "downloads".into(),
            count: 3,
            size: 120,
            review_only: false,
            extra: None,
            biggest_match: Some(7),
        };
        assert_eq!(row.id, "downloads");
        assert!(!row.review_only);
    }

    #[test]
    fn type_segments_sorted_desc() {
        #[rustfmt::skip]
        let mut segs = [
            TypeSegment { label: "A".into(), color: 1, size: 10 },
            TypeSegment { label: "B".into(), color: 2, size: 30 },
            TypeSegment { label: "C".into(), color: 3, size: 20 },
        ];
        segs.sort_unstable_by_key(|s| std::cmp::Reverse(s.size));
        assert_eq!(segs[0].label, "B");
    }
}
