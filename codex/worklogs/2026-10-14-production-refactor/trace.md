# Production refactor trace — 2026-10-14

## Scope and invariant

Refactor and harden DiskGenie without silently changing its filesystem safety,
duplicate-equality, platform, or UI contracts.  Behavioural improvements require
their own measured, tested vertical slice; structural work must preserve the
existing external contract.

## Session start

- Branch: `codex/production-refactor-2026-10-14`
- Base: `a2bec34` (`test(core): engine property suite — random populations through real files`)
- Working tree: clean before this worklog was added.
- Remotes: only `upstream` is configured. No credentials are stored in this
  repository or command history.

## Discovery completed

- Read repository agent instructions, principles, affected task protocols, main
  README, acceptance evidence, and prior worklogs.
- Indexed 33,077 source lines across Rust and TypeScript.  Largest modules are
  recorded in the session inventory; `license.rs`, `CanvasViz.tsx`, mock
  adapters, duplicate commands, and explore/scan engines warrant focused smell
  review before any refactor.
- Cloned reference repositories under the workspace `references/` directory.
  They are hypotheses and design references only; their code and licenses are
  not copied into DiskGenie.

## Baseline plan

1. Run portable local gates and record exact outcomes.
2. Map duplicate scanning and Age Map end-to-end: React -> state -> IPC -> Rust
   command -> core/platform adapter -> returned view model.
3. Reproduce the reported missing Age Map monthly data using the deterministic
   mock and add a regression test before changing wiring.
4. Profile duplicates with the repository's deterministic corpus, then make one
   bounded, separately measured engine change only if a measurable bottleneck is
   proven.
5. Run focused and full gates; record limitations of this non-Windows host.

## Baseline results

Executed after a clean `npm ci --ignore-scripts` on this host:

- `npm run typecheck` — pass.
- `npm test` — 14 files / 131 tests pass.
- `npm run build` — pass.  The build reports existing mixed static/dynamic-import
  warnings for IPC, analytics, and Zustand stores, and emits an 803.61 kB
  uncompressed primary JS asset.  No bundle change has been made from this
  observation alone.
- `cargo` and `rustc` are not installed on this host.  Therefore `cargo fmt`,
  clippy, workspace tests, native Tauri execution, Windows filesystem tests,
  installers, and real GUI screenshots have not been run locally.

## Age Map vertical slice

- Mapped and recorded React → IPC → command → core → platform wiring in
  `codex/index/ui-core-wiring.md`.
- Corrected the real-core/TypeScript contract: `MonthHeatmap.busiest` now uses
  a zero-based month, matching the dense `bytes` vector and browser `Date`
  convention. The old Rust payload used 1–12 while the UI/mock used 0–11.
- Made mock Age Map aggregation use allocated bytes, the same quantity the
  native core uses, and clear an old heatmap while the new generation/folder is
  loading so stale-looking data cannot mask a failed real request.
- Post-change frontend gates: typecheck pass; Vitest 14 files / 131 tests pass;
  production build pass; `git diff --check` pass. Core regression tests were
  updated but cannot run without Rust on this host.

## Reference review note

The cloned reference projects reinforce existing DiskGenie choices rather than
justify a blind rewrite: bounded/native traversal, on-disk accounting, explicit
selection and review before cleanup, platform-aware behavior, and generated
realistic corpora. Their code has not been copied. Any future adoption must be
an independently measured DiskGenie change with compatible licensing.

## Commit / remote status

- Commit: `0a0afa7 fix(age-map): align monthly heatmap contract`.
- Push to the configured `upstream` remote was attempted and stopped because
  this environment has no configured GitHub credential (`could not read
  Username`). No token was written to the repository, Git configuration, or
  command history.

## Risks and rollback

- Filesystem cleanup and duplicate selection are destructive-adjacent. Changes
  must retain identity revalidation, hard-link exclusion, authoritative byte
  equality, cancellation, and Recycle Bin-only behaviour.
- Windows native/Tauri GUI verification cannot be claimed from this host; it is
  delegated to the existing Windows CI matrix after a push.
- Each coherent change is independently committable and can be reverted without
  touching the accumulated baseline evidence.
