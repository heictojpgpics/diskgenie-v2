# Session worklog — production refactor, 2026-10-14 (session 20)

Branch: `refactor/production-grade-2026-10-14` (based on `origin/codex/production-refactor-2026-10-14`, which carries the age-map heatmap-contract fix `0a0afa7`).

Start: 2026-10-14 11:14 UTC. Operator mandate: refactor to production grade, no external behavior change without intent, test-first, fix the owner-reported bugs, deep-read everything first.

## Baseline (recorded before any change)

- `cargo test` (core, host): 180 lib + 1 ignored (perf) + 2 + 21 platform + 16 property — green.
- `tsc --noEmit`: 0 errors. `vitest run`: 131/131 green.
- Repo state at start: clean tree, main @ `a2bec34`, codex branch +2 commits (`0a0afa7` age-map fix, `e05785d` baseline doc).
- Reference images extracted from upload (`refrence.zip` → 9 PNGs): agemap, bubbles, flames, folders, lists, minmap, sunburst, topSize, tree. VLM-analyzed agemap/bubbles/minmap/sunburst/tree targets (see `codex/research/` for distilled specs).
- 6 reference repos cloned under `/home/z/my-project/reference-repos/`: disktree, dua-cli, WinMemoryCleaner, cleaner, rust-best-practices, rust-skills.

## Owner-reported bugs (the contract for this session)

1. Age Map "bytes by last-modified month" shows nothing on real release builds (mock fine).
2. Screenshot workflow takes duplicate frames of the same page/mode.
3. Scan pauses mid-run on fast NVMe (512 GB Gen4, ~30 s total but with stalls).
4. Duplicates engine too slow → v3 engine exists on main (session 19); verify and push further.
5. Viz modes (mind map, bubbles, sunburst) weak on real filesystems, good on mock.
6. Hidden bugs: leaks, stalls, freezes, UI symmetry/alignment/responsiveness/theme issues.

## Session log

- 11:14 session start; environment verified (cargo 1.99.0, node 24, python 3.12).
- 11:20 repo cloned; branch cut; codex age-map fix reviewed in detail (busiest-month zero-based alignment + stale-data nulling; sound, adopted as base).
- 11:25 baseline gates recorded above. npm deps installed.
- Next: parallel deep-read of codebase + reference repos; age-map rendering fix; duplicates engine verification.

## Milestone 1 (Age Map + stall fixes + scanner perf)

- **Age Map (owner bug #1)**: perceptual sqrt scale for the month heatmap (real drives spread bytes across 30+ years; a linear fraction renders all but the busiest month invisible — the "never shows anything" report), visible pale base fill for empty cells (reference treatment), labels at sqrt-frac ≥ 0.22, legend swatches now mirror the cell formula, bounded retry (3 attempts, 120/240/360 ms) replacing the eternal "reloading…" spinner on stale-generation races. Root-cause chain: codex fix (busiest zero-based) was correct but insufficient; the rendering threshold + no-retry spinner were the release-build symptoms.
- **get_layout off the main thread**: `#[tauri::command(async)]` (Tauri sync commands run on the MAIN thread; the cold by-type/by-age path walks the whole subtree, and every resize pixel is a cold width×height key — the user-visible freeze). get_names likewise. Frontend already drops superseded responses (CanvasViz `disposed` guard) so ordering is safe.
- **Layout caches bounded**: LRU (64 layout frames / 32 regroup datasets) replacing unbounded HashMaps that only cleared on scan swap (a resize drag grew one ~600 KB entry per pixel step).
- **Scanner stall fixes** (dua-cli/disktree-informed): worker cap 16 (disktree measured 6.5 s @ 16 vs 8.1 s @ 24 threads on a 3.4M-file walk); wake-one notify per pushed job under the state lock + termination-only notify_all (was: notify_all after EVERY directory — futex/mutex herd on many-core); names MOVED into BatchEntry (was: cloned — a million-file scan saved a million Vec clones); child dir name converted UTF-16→UTF-8 once (was: twice); thread-local 256 KiB enumeration buffer in win/dir.rs (was: fresh zeroed alloc per directory, 500K+ per volume under 16 workers).
- **Hover cache stale after cleanup**: invalidateHoverCache() now runs in the cleanup-committed listener (sizes/shares previously quoted pre-commit numbers for ancestors of deleted items until the next full scan).
- **Tests**: new `wake_one_protocol_terminates_on_every_shape` (6 tree shapes: single, 120-wide fan-out, 40-deep chain, mixed, empty-dirs, interleaved). Core 220 green (219+1). Mirror (scratch-win) compiles the REAL layout.rs + clippy clean + 46 tests green. tsc 0 errors, vitest 131/131.

## Milestone 2 (viz correctness + copy pass)

- **Sunburst adaptive rings**: ring count = reachable depth (bounded by the slider), the contract flame/mind-map already enforce. A depth-8 request over a 3-level tree previously reserved 8 paper-thin mostly-empty rings. Regression test: fat rings fill the radial budget exactly.
- **Dupes engine false-positive gap closed**: a mid-verify read ERROR now taints the member (unread remainder unknown); tainted members can never join a verified group. Two members failing at the same offset with equal chains were previously emitted as "verified" duplicates whose tails were never read. All-eof tainted classes retire as honest unverifiable skips. Termination analysis documented in-code.
- **Copy pass (owner mandate: no em dashes, no AI-slop patterns, license spelling unified)**:
  * Every user-facing em dash replaced across DuplicatesView, ApplicationsView, SnapshotsView, ExploreView, FoldersMode, TopSizesMode, AgeMapMode, ListMode, CleanupQueuePopover, InspectorPanel, QuickWinsSection, UnreadableNotice, FileTypesSection, CurrentViewSection, PreviewOverlay, App, TopBar, HoverChip, ActivationGate, LicenseDialog (+ the marketing-dense Duplicates empty state rewritten in plain human prose).
  * "Licence" (British) unified to "license" (the app's dominant American spelling): Purchase Licence button, gate sub-line.
  * Data placeholders "—" → "n/a" / "Unknown" (format.ts + call sites + test parity).
  * Straight apostrophes → curly (house style): couldn't/can't/you're.
  * Wire-contract gate messages moved from "ACTIVATION_REQUIRED — msg" to "ACTIVATION_REQUIRED. msg" with the userFacingError regex widened to strip any separator run ([\s—\-.]*) — old and new formats both parse; mock mirrored; new-format tests added. Rust posture/error copies de-dashed in lockstep (license.rs, commands/{license,snapshots_cmd,shell,dupes}.rs, mac/apps.rs placeholder).
  * Quick-wins reason strings "X — Quick win" → "X (Quick win)" with the cleanup.ts source parser updated (legacy separator still tolerated) and TourDriver CI parity.
- Gates after: core 182+21+16 green + clippy + fmt; mirror 46 green + clippy; tsc 0; vitest 132/132.
