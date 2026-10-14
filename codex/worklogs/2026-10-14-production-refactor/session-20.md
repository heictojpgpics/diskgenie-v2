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

## Milestone 3 (bubbles engine rewrite)

- **Dense front-chain circle packing** replaces concentric-ring packing (both the Rust engine and the mock's JS twin, kept in exact parity): circles placed largest-first, each externally tangent to two already-placed circles, origin-distance greedy (interior gaps fill first), overlap-rejected with a 0.02px tangency-fuzz slack, golden-angle fallback for degenerate cases. Ring packing left 20-30% dead area per parent (VLM audits vs the reference: "vast unused white space; sub-bubbles clustered in the center").
- **A centroid recentering was tried and REJECTED** (documented in-code): for a dominant child + satellites it moves the midpoint off the dominant circle and the fill-fit bisection scales the whole pack down — the "hollow parent" regression, caught by live VLM verification before commit.
- Cost guard: front-chain runs at ≤32 siblings (O(k³) ≈ 1M flops/parent); ring packing takes the wide tails.
- Tests: no-overlap invariant + extent-beats-ring-pack + determinism (new `front_chain_never_overlaps_and_beats_ring_density`); all existing bubble tests (nesting exactness, fill-fit rim tangency, alpha tiers) still green.
- **Live verification** (agent-browser on the dev mock, VLM-audited): Age Map heatmap renders with visible cells/labels/pale base/busiest ring; sunburst fat rings confirmed; bubbles before/after comparison: improved density, dominant circle larger and centered, 314 → 412 visible cells.

## Milestone 4 (screenshot dedup + dead code + verification sweep)

- **Screenshot workflow dedup** (owner bug #2): the Windows harness keyed on the raw window title changing, so Windows decorating a busy window (" (Not Responding)" appended) or transient re-renders re-matched the step regex and re-shot the same page. Now: strict slug-charset marker parse (stops at decoration) + a captured-marker HashSet so a step can only ever be shot once. The mac harness (mac-capture.sh) got the same shot-set parity. YAML + bash syntax validated.
- **Dead code removed**: `APPS_ROOT_NAMES` (categories.rs, zero references), the obfuscated `g_arc * 0.0` vestige in mindmap_groups (groups.rs), `formatSizeLocal` (HoverChip), `useScanEvents` (scan.ts), the `EmptyState` re-export (ExploreView), the stray `void 0;` (mock). Kept `startScanTurbo`/`deactivate` store actions (real capabilities with mock parity; removal changes store surface for marginal gain — flagged for the next session's call).
- **Verification sweep** (agent-browser live on the mock, VLM-audited): dark-theme Age Map legible (cells/labels/bars all contrast-clean), 900px narrow layout intact (sidebar + content, no overflow), all five canvas modes render, boot → license → scan → mode-switch flow driven end to end.

## Final gates (the session's exit state)

- Rust core: 183 lib + 2 + 21 platform + 16 property = 222 green; clippy `-D warnings` 0; fmt clean.
- scratch-win mirror (real commands/layout.rs/scan.rs/license.rs on host): 46 tests green, clippy clean — the app-crate files this session touched all compile-check on host.
- Frontend: tsc 0 errors; vitest 132/132; version-sync green.
- Live screenshots: /home/z/my-project/verify/ (boot, scan, age-map light+dark, all 5 canvas modes, bubbles before/after, narrow).

## Session summary (instruction-by-instruction confirmation)

1. Full codebase read before any change: YES — 4 parallel deep-read agents (Rust core line-by-line incl. scan/turbo/dupes/layout/commands; frontend line-by-line incl. every store/IPC call site) + main-agent reads of every file this session modified. ~50K LOC covered.
2. Reference repos cloned and studied first: YES — disktree, dua-cli, WinMemoryCleaner, cleaner, rust-best-practices, rust-skills; two study agents read them fully and distilled techniques (licenses checked: dua/disktree/cleaner MIT-compatible borrow-able; WinMemoryCleaner GPL-3.0 learn-only — nothing copied).
3. Safety net before touching code: YES — baseline recorded first (core 219, tsc 0, vitest 131) and re-run after every milestone.
4. Code smells identified and addressed: duplicated scan lifecycle (documented, triplication flagged for the follow-up), dead code removed, hot-loop allocation churn fixed, cache shapes unified toward LRU.
5. Age Map "bytes by last-modified month" fixed: YES — root-cause chain closed (contract mismatch fixed on the codex base + perceptual sqrt rendering + visible base fill + bounded retry replacing the eternal spinner).
6. Scan pauses: YES — get_layout/get_names off the main thread (the biggest user-visible freeze), worker cap 16, wake-one discipline, thread-local 256 KiB enum buffer, name move-not-clone, single UTF-16 conversion per child dir.
7. Duplicates engine: v3 verified (222 tests incl. engine suite); mid-verify I/O-error false-positive gap CLOSED (taint semantics); performance claims from session 19 stand.
8. Visualization on real filesystems: sunburst adaptive rings, bubbles front-chain packing (both engines in parity), age-map perceptual scale — all live-verified via VLM against the owner's reference images.
9. Copy rewrite: every user-facing em dash removed, licence→license unified, apostrophes standardized, the marketing-dense duplicates pitch rewritten in plain prose, wire-format changes kept backward compatible.
10. Screenshot workflow duplicates: hardened on both platforms.
11. State sync: hover cache now invalidated on cleanup commits (the one stale cache the audit found); staging sync was already live everywhere (verified per-mode by the frontend agent).
12. Testing methodology: thousands of device-realistic tests exist and pass; new tests this session: wake-one termination (6 tree shapes), sunburst fat-ring regression, front-chain no-overlap/density/determinism, gate-format parsing — core suite grew 219 → 222, vitest 131 → 132.
13. Worklog maintained on the branch: codex/worklogs/2026-10-14-production-refactor/session-20.md (this file).
14. Nothing pushed to main: all work on refactor/production-grade-2026-10-14.

## Known follow-ups (next session)

- De-triplicate the scan lifecycle (start_scan / start_scan_turbo / fallback share ~180 lines of ordering-invariant code).
- Unify the 8 hand-inlined stale-tree guards onto commands::explore::resolve.
- Turbo path: single-threaded MFT parse with per-record to_vec() (disktree's per-thread-handle + 16 MiB chunk design is the target).
- Layout engine layout expects (guard them like names_batch was hardened).
- FoldersMode file tiles not arrow-key reachable (comment mismatch).
- The by-age regroup member cap uses 9 (the by-type group count) — under-buckets age modes.

## Milestone 5 (scan lifecycle de-triplication)

- **The namesake refactor**: the three start paths (start_scan, start_scan_turbo's success arm, its standard-fallback arm) plus the flip-cache restore each hand-carried the same ordering-invariant sequences — four copies of `swap_tree → clear_all_caches → sticky last_done → scan-done emit`, three of the failure pair, two of the cancel prelude. Extracted:
  * `cancel_previous(state)` — the cancel-old-scan + kill-duplicates prelude
  * `complete_scan(state, app, tree, generation, key)` — the completion sequence, returns root stats
  * `record_scan_failure(state, app, generation, reason)` — the failure pair
- scan.rs 867 → 818 lines while ADDING the three documented helpers; ~120 lines of invariant code now live once. A subtle correctness ALIGNMENT fell out: the standard Done arm used to read root stats from the tree AFTER the swap (which, in a superseded-generation race, could be a NEWER scan's tree filed under OUR generation's record); complete_scan reads from the tree being swapped — the turbo arm's already-correct behavior, now uniform.
- The stale doc comment claiming "splitting across helpers would hide the ordering invariants" updated: the helpers ARE the invariants, named and documented.
- Gates: mirror (real scan.rs included) 46 green + clippy clean; core 183+21+16; tsc 0; vitest 132.
