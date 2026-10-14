//! IPC commands (thin layer, one file per domain — doc 02 §2).
//! M3: `scan` (start_scan / get_status / get_dev_hooks).
//! M4: `layout` (get_layout raw-binary + cache, get_names batching),
//! `explore` (Folders/Top Sizes/Age Map/List/inspector datasets),
//! `shell` (Open / Show in Explorer / Copy Path / text preview).
//! M8: `applications` (registry+MSIX enumeration, uninstall flow).
//! M9: `monitor` (2s sampler). M10: `license` + `analytics_cmd`.

pub mod analytics_cmd;
pub mod applications;
pub mod cleanup;
pub mod dupes;
pub mod explore;
pub mod layout;
pub mod license;
pub mod monitor;
pub mod scan;
pub mod shell;
pub mod sidebar;
pub mod snapshots_cmd;
pub mod store_update;

use std::sync::Arc as TreeArc;

use diskgenie_core::scan::node::Tree;

use crate::state::AppState;

/// Shared stale-tree guard (the tree-only form): resolves a snapshot
/// `Arc` (the read lock drops immediately) or errors with the exact
/// wire strings the frontend drops by convention ("no scan yet" /
/// "stale generation N (current M)"). The hand-inlined copies across
/// the command modules had already drifted in comment and structure;
/// every new command goes through these instead.
pub(crate) fn resolve_tree(
    state: &AppState,
    generation: u64,
) -> Result<TreeArc<Tree>, String> {
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
    Ok(TreeArc::clone(tree))
}

/// [`resolve_tree`] + the node-exists check (the form the explore
/// datasets use: a bad node rejects before any computation runs).
pub(crate) fn resolve_node(
    state: &AppState,
    generation: u64,
    node: u32,
) -> Result<TreeArc<Tree>, String> {
    let tree = resolve_tree(state, generation)?;
    if tree.node(node).is_none() {
        return Err(format!("node {node} not in tree"));
    }
    Ok(tree)
}
