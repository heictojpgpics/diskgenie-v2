//! The mirrored command modules: the REAL files under test plus the
//! stubs their cross-module references call.

#[path = "../../../src-tauri/src/commands/dupes.rs"]
pub mod dupes;
#[path = "../../../src-tauri/src/commands/layout.rs"]
pub mod layout;
#[path = "../../../src-tauri/src/commands/license.rs"]
pub mod license;
#[path = "../../../src-tauri/src/commands/scan.rs"]
pub mod scan;
#[path = "../../../src-tauri/src/commands/sidebar.rs"]
pub mod sidebar;
#[path = "../../../src-tauri/src/commands/snapshots_cmd.rs"]
pub mod snapshots_cmd;

// The REAL commands/license.rs is #[path]-included above (session 17:
// the async + spawn_blocking refactor). Its `require_licensed` gate is
// exercised by the real module's own unit tests on the host.
// The REAL commands/layout.rs is #[path]-included above (session 20:
// the LRU cache + `(async)` command changes compile-checked on host;
// the scan.rs clear-cache call sites resolve against the real types).

/// The explore cache stubs (`crate::commands::explore`).
pub mod explore {
    /// The Top Sizes cache (stub shape).
    pub struct TopCache;
    impl TopCache {
        /// Real signature (clears done + inflight).
        #[allow(clippy::unused_self)] // signature parity with the real cache
        pub fn clear(&self) {}
    }
    /// The Age Map cache (stub shape).
    pub struct AgeCache;
    impl AgeCache {
        /// Real signature.
        #[allow(clippy::unused_self)] // signature parity with the real cache
        pub fn clear(&self) {}
    }
}

/// The shared stale-tree guards (real bodies: the mirror's dupes.rs and
/// sidebar.rs call these through `super::`, matching the real
/// commands/mod.rs surface added in session 20).
pub(crate) mod guard {
    use crate::state::AppState;
    use diskgenie_core::scan::node::Tree;
    use std::sync::Arc as TreeArc;

    /// Tree-only form: resolves a snapshot `Arc` or errors.
    pub(crate) fn resolve_tree(state: &AppState, generation: u64) -> Result<TreeArc<Tree>, String> {
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

    /// Tree + node form (the explore datasets' guard).
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
}
pub(crate) use guard::resolve_tree;
// The real crate's explore.rs consumes this through `commands::*`; the
// mirror does not #[path]-include explore.rs, so the re-export is
// unused here and only here.
#[allow(unused_imports)]
pub(crate) use guard::resolve_node;
