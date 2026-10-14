# UI-to-core wiring index

## Age Map

| Layer | Entry point | Contract |
|---|---|---|
| React view | `src/explore/modes/AgeMapMode.tsx` | Requests on `(generation, folder)` changes; clears prior state before loading; renders buckets, heatmap and Big & Untouched. |
| TypeScript IPC | `src/viz/exploreIpc.ts#getAgeMap` | `invoke("age_map", { generation, node })`; `busiest` is `[year, zero_based_month]`. |
| Tauri command | `src-tauri/src/commands/explore.rs#age_map` | Validates generation/node, performs calculation on the blocking runtime, caches by `(generation, node)`. |
| Core policy | `src-tauri/core/src/age.rs` | Walks live descendants, sums allocated bytes, excludes invalid timestamps from heatmap, reports zero-based month indexes. |
| Platform input | `platform/{win,mac}/dir.rs` and `turbo/tree.rs` | Converts native mtime to Unix seconds and provides allocated size. |
| Test seam | `core/src/age.rs` tests | Validates insertion, multi-year accumulation, empty timestamps, and zero-based busiest month. |

## Invariants

- Data is generation-tagged; stale trees fail rather than being rendered as current.
- Age buckets and monthly heatmap use allocated (`on_disk`) bytes, not logical
  length. This keeps sparse/compressed-file visual proportions honest.
- Month indexes crossing IPC are zero-based, matching the dense byte-vector
  columns and JavaScript's UTC month convention.
