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
