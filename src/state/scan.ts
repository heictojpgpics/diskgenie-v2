/**
 * Scan state (spec §4; doc 03 M3.7): idle / scanning / done, the live
 * progress from `scan-progress` events (150 ms ticks), and the finished
 * stats from `scan-done`. Requests and results are generation-tagged;
 * stale ticks drop (spec §9 IPC rule).
 */
import { useEffect } from "react";
import { create } from "zustand";
import { invoke } from "../lib/ipc";
import { EVENTS, track } from "../lib/analytics";
import { listen, type UnlistenFn } from "../lib/ipc";
import { userFacingError } from "../lib/userFacingError";
import { invalidateLayouts, invalidateHoverCache } from "../viz/layoutIpc";
// Realtime refresh wiring (session 13): a tree change (fresh scan OR
// cleanup commit) must reach EVERY consumer — duplicates results are
// tree-scoped (stale after surgery), and the applications list shrinks
// when staged leftovers get removed. Function-level usage on both
// sides — the module cycle is init-safe (same pattern as ipc ↔
// state/license).
import { useDupesStore } from "./dupes";
import { useApplicationsStore } from "./applications";
import { useExploreStore } from "./explore";

export interface ScanProgress {
  files: number;
  folders: number;
  bytes: number;
  currentPath: string;
  denied: number;
  deniedSamples: string[];
}

export interface RootStats {
  logical: number;
  onDisk: number;
  files: number;
  folders: number;
}

export type ScanStatus = "idle" | "scanning" | "done" | "error";

/** turbo-report event payload. */
export interface TurboReportData {
  generation: number;
  records: number;
  torn: number;
  bad: number;
  unreferenced: number;
  ms: number;
}

interface ScanStore {
  status: ScanStatus;
  generation: number;
  progress: ScanProgress | null;
  stats: RootStats | null;
  error: string | null;
  /** Exact target used by the active/latest scan, retained for elevation retries. */
  scanTarget: string;
  /** Measured client-side scan duration for the completed generation. */
  scanDurationMs: number | null;
  startScan: (target: string, force?: boolean) => Promise<void>;
  /** Turbo (MFT) scan; ELEVATION_REQUIRED → shield state. */
  startScanTurbo: (target: string) => Promise<void>;
  /** Stop the running scan (cooperative server-side). Reverts to the
   * previous finished tree when one exists (generation restored so
   * every cache stays valid), otherwise back to the idle welcome. */
  cancelScan: () => Promise<void>;
  /** The turbo fallback reason (shown until the standard scan lands). */
  turboFallback: string | null;
  /** The turbo report (records/ms/warnings). */
  turboReport: TurboReportData | null;
  /** Apply a tree update WITHOUT a rescan (M5 surgery commit: the
   *  generation bumped, views re-key every cache on it). */
  applyTreeUpdate: (generation: number, stats: [number, number, number, number] | null) => void;
  /** Attach event listeners (call once from the Explore view; idempotent
   *  under StrictMode — see the spec §9 React pitfalls). */
  ensureListeners: () => void;
}

/** The `start_scan` result (session 14): the generation + whether the
 * backend INSTANTLY RESTORED a cached tree for the exact target (the
 * drive-flip fix — no scan ran, the tree already swapped, the
 * scan-done event already fired). `stats` rides along on restores so
 * the store adopts without a poll. */
interface ScanStartResult {
  generation: number;
  restored: boolean;
  stats: [number, number, number, number] | null;
}

interface ProgressEvent {
  generation: number;
  progress: ScanProgress;
}

interface ScanDoneEvent {
  generation: number;
  stats: [number, number, number, number] | null;
  error: string | null;
}

/** cleanup-committed payload (M5; the trashed/failed lists go to the
 *  cleanup store via the command's return value — the event carries the
 *  tree-refresh half + the surgery's navigation fixup). */
interface CommitEvent {
  generation: number;
  stats: [number, number, number, number] | null;
  /** Where the view lands when the commit removed the on-screen folder
   *  (the server walked the UI's live navigation up to a survivor). */
  currentFolder?: number;
  /** The post-surgery selection (null when the selected node was
   *  removed). */
  selectedNode?: number | null;
}

let listenersAttached = false;
let unlisteners: UnlistenFn[] = [];
let scanStartedAt: number | null = null;
/** The generation of the last FINISHED tree the UI holds (0 before the
 * first scan). `generation` tracks the scan lifecycle; this tracks the
 * tree so a cancelled scan can revert every fetch key cleanly. */
let treeGeneration = 0;
let treeStats: RootStats | null = null;

/** Record the tree the store now points at (every path that sets
 * status "done" or refreshes the tree via surgery). */
function recordTree(stats: [number, number, number, number] | null): void {
  treeGeneration = useScanStore.getState().generation;
  treeStats = stats
    ? { logical: stats[0], onDisk: stats[1], files: stats[2], folders: stats[3] }
    : null;
}

/** Adopt a FINISHED scan outcome — the ONE code path for every
 * arrival route (the `scan-done` event, the get_status reconcile, the
 * watchdog, and the instant-restore return). Before session 14 these
 * four sites hand-duplicated the adoption and only the EVENT path ran
 * the per-id cache invalidations — a tree that arrived through the
 * reconcile (the tiny-tree timing: scan-done fired before the store
 * held the new generation) left stale hover/layout caches keyed by
 * arena ids the fresh tree REUSES: names, sizes and chips from the
 * previous target. Restores hit exactly that window by construction,
 * so the adoption was unified first. */
function adoptDone(
  generation: number,
  stats: [number, number, number, number] | null,
  error: string | null,
  engine: "standard" | "cache",
): void {
  // A new tree (or a restored one) invalidates every consumer keyed
  // by arena ids or generation — the event path's discipline, now
  // every path's.
  invalidateHoverCache();
  invalidateLayouts();
  useDupesStore.getState().invalidate(generation);
  if (error) {
    scanStartedAt = null;
    useScanStore.setState({ status: "error", error });
    return;
  }
  // A new tree's arena reuses node ids for different nodes: navigation
  // (currentFolder / selectedNode / folderStack) holds ids from the
  // PREVIOUS tree and must reset here — not only on the status
  // transition App.tsx watches. A RESTORE never leaves the "done"
  // status (no transition fires), so without this the view kept
  // drilling into a stale id that now resolves to an unrelated node
  // in the restored tree (session 14, the drive-flip follow-up).
  useExploreStore.getState().resetNavigation();
  const scanDurationMs = scanStartedAt == null ? null : Math.max(0, performance.now() - scanStartedAt);
  scanStartedAt = null;
  useScanStore.setState({
    status: "done",
    scanDurationMs,
    stats: stats
      ? { logical: stats[0], onDisk: stats[1], files: stats[2], folders: stats[3] }
      : null,
  });
  recordTree(stats);
  track(EVENTS.scanCompleted, {
    engine,
    files: stats ? stats[2] : 0,
    bytes: stats ? stats[1] : 0,
  });
  // Denied-folder notice data for the sidebar (spec §7/6.4). Only the
  // event path carries live progress; restores and reconciles skip
  // when no progress is standing.
  const prog = useScanStore.getState().progress;
  if (prog) {
    try {
      window.localStorage.setItem(
        "diskgenie.last-denied",
        JSON.stringify({ count: prog.denied, samples: prog.deniedSamples }),
      );
    } catch {
      /* storage unavailable: notice stays hidden */
    }
  }
}

/** The lost-event reconcile: a tiny tree can finish scanning before the
 *  `start_scan` invoke resolves, so the `scan-done` event lands while
 *  the store still holds the previous generation and is dropped as
 *  stale. `get_status` carries the sticky last-done record — after the
 *  invoke resolves we poll once and adopt the outcome if it already
 *  finished (scans still running are left to the event stream). */
async function reconcileDone(generation: number): Promise<void> {
  try {
    const st = await invoke<{
      scanning: boolean;
      lastDone: { generation: number; stats: [number, number, number, number] | null; error: string | null } | null;
    }>("get_status");
    const rec = st?.lastDone;
    if (!rec || st?.scanning || rec.generation !== generation) return;
    adoptDone(rec.generation, rec.stats, rec.error, "standard");
  } catch {
    /* get_status unavailable: the event stream still works */
  }
}

/** Server-authoritative state (the watchdog's source). */
interface StatusPayload {
  generation: number;
  scanning: boolean;
  hasTree: boolean;
  progress: ScanProgress | null;
  lastDone: { generation: number; stats: [number, number, number, number] | null; error: string | null } | null;
}

/** The scanning watchdog: while a scan runs, re-read `get_status`
 *  every 4 s. The Rust state is the authority — if the event stream
 *  dropped `scan-done`/`scan-progress` (or the store holds a stale
 *  generation after a double start), the poll heals the UI instead of
 *  spinning "Scanning…" forever (CI run 35686427186 hung exactly like
 *  that: process alive, tour running, counter frozen at 0). */
let watchdogTimer: number | null = null;

function stopWatchdog(): void {
  if (watchdogTimer !== null) {
    window.clearInterval(watchdogTimer);
    watchdogTimer = null;
  }
}

function ensureWatchdog(): void {
  if (watchdogTimer !== null) return;
  watchdogTimer = window.setInterval(() => {
    const s = useScanStore.getState();
    if (s.status !== "scanning") {
      stopWatchdog();
      return;
    }
    void (async () => {
      try {
        const st = await invoke<StatusPayload>("get_status");
        const cur = useScanStore.getState();
        if (cur.status !== "scanning") return;
        if (st.scanning) {
          // Still running server-side: adopt live progress even when
          // the 150 ms event ticks never arrived.
          if (st.generation === cur.generation && st.progress) {
            useScanStore.setState({ progress: st.progress });
          }
          return;
        }
        const rec = st.lastDone;
        if (rec && rec.generation >= cur.generation) {
          adoptDone(rec.generation, rec.stats, rec.error, "standard");
        }
      } catch {
        /* poll failure: the next tick retries */
      }
    })();
  }, 4000);
}

export const useScanStore = create<ScanStore>((set, get) => ({
  status: "idle",
  generation: 0,
  progress: null,
  stats: null,
  error: null,
  scanTarget: "ThisPC",
  scanDurationMs: null,
  turboFallback: null,
  turboReport: null,

  startScan: async (target: string, force = false) => {
    try {
      scanStartedAt = performance.now();
      const res = await invoke<ScanStartResult>("start_scan", { target, force });
      if (res.restored) {
        // Instant restore (the drive-flip fix): the backend swapped a
        // cached tree in DURING the invoke — the scan-done event fired
        // before this store held the new generation, so it was dropped
        // as stale. Adopt inline: generation + done state + the same
        // invalidations every adoption path runs. No scanning flash,
        // no watchdog, no poll round-trip.
        set({ status: "done", generation: res.generation, error: null, progress: null, scanTarget: target, scanDurationMs: 0 });
        adoptDone(res.generation, res.stats, null, "cache");
        return;
      }
      set({ status: "scanning", generation: res.generation, error: null, stats: null, progress: null, scanTarget: target, scanDurationMs: null });
      ensureWatchdog();
      await reconcileDone(res.generation);
    } catch (e) {
      scanStartedAt = null;
      set({ status: "error", error: userFacingError(e) });
    }
  },

  startScanTurbo: async (target) => {
    try {
      scanStartedAt = performance.now();
      const generation = await invoke<number>("start_scan_turbo", { target });
      set({
        status: "scanning",
        generation,
        error: null,
        stats: null,
        progress: null,
        turboFallback: null,
        turboReport: null,
        scanTarget: target,
        scanDurationMs: null,
      });
      await reconcileDone(generation);
      ensureWatchdog();
    } catch (e) {
      if (String(e).includes("ELEVATION_REQUIRED")) {
        scanStartedAt = null;
        set({ error: "ELEVATION_REQUIRED", status: "error", turboFallback: null });
      } else {
        scanStartedAt = null;
        set({ status: "error", error: userFacingError(e) });
      }
    }
  },

  cancelScan: async () => {
    const s = get();
    if (s.status !== "scanning") return;
    // Optimistic revert FIRST (the UI snaps back instantly), then the
    // server ack. The reverted generation matches the standing tree so
    // every generation-keyed fetch and cache stays valid.
    if (treeGeneration > 0) {
      set({
        status: "done",
        generation: treeGeneration,
        stats: treeStats,
        progress: null,
        scanDurationMs: null,
        error: null,
      });
    } else {
      set({ status: "idle", progress: null, error: null });
    }
    stopWatchdog();
    track(EVENTS.scanCancelled, {});
    try {
      await invoke("cancel_scan");
    } catch {
      /* the watchdog/reconcile heals any drift */
    }
  },

  applyTreeUpdate: (generation, stats) => {
    set((s) => ({
      generation,
      // Keep "done": the tree changed, not the scan lifecycle.
      status: s.status === "done" ? "done" : s.status,
      stats: stats
        ? { logical: stats[0], onDisk: stats[1], files: stats[2], folders: stats[3] }
        : s.stats,
    }));
    recordTree(stats);
  },

  ensureListeners: () => {
    if (listenersAttached) return;
    listenersAttached = true;
    void (async () => {
      const un1 = await listen<ProgressEvent>("scan-progress", (e) => {
        const { generation, progress } = e;
        // Drop stale ticks (a newer scan replaced the tree).
        if (generation !== get().generation || get().status !== "scanning") return;
        set({ progress });
      });
      const un4 = await listen<string>("turbo-fallback", (e) => {
        set({ turboFallback: e });
      });
      const un5 = await listen<TurboReportData>("turbo-report", (e) => {
        set({ turboReport: e, turboFallback: null });
      });
      const un3 = await listen<CommitEvent>("cleanup-committed", (e) => {
        const { generation, stats, currentFolder, selectedNode } = e;
        // Only a NEWER generation applies (stale commits drop).
        if (generation >= get().generation && get().status === "done") {
          get().applyTreeUpdate(generation, stats);
          // Navigation fixup: the commit may have REMOVED the folder on
          // screen — land the view on the survivor the surgery chose
          // (a hard set, not a navigation: no back-stack entry for a
          // forced landing; optional fields keep older payloads inert).
          if (typeof currentFolder === "number" && currentFolder >= 0) {
            useExploreStore.setState((s) => ({
              currentFolder: s.currentFolder === currentFolder ? s.currentFolder : currentFolder,
              selectedNode: typeof selectedNode === "number" ? selectedNode : (selectedNode === null ? null : s.selectedNode),
            }));
          }
          // Realtime refresh (session 13): the tree changed under every
          // consumer — duplicates results are now tree-stale, and staged
          // leftovers shrank the applications list (the Rust side already
          // cleared its caches; the frontend follows).
          useDupesStore.getState().invalidate(generation);
          if (useApplicationsStore.getState().apps !== null) {
            void useApplicationsStore.getState().load(true);
          }
          // Surgery reuses arena ids for surviving nodes, so the hover
          // chip's id-keyed size/share cache would keep quoting the
          // pre-commit numbers for ancestors of anything deleted (the
          // layout decode cache is generation-keyed and re-keys itself;
          // hover details were the one id-keyed cache left).
          invalidateHoverCache();
        }
      });
      const un2 = await listen<ScanDoneEvent>("scan-done", (e) => {
        const { generation, stats, error } = e;
        if (generation !== get().generation) return;
        // New tree (the normal arrival route — the reconcile/watchdog
        // paths replay the same adoption when this event was dropped
        // as stale; adoptDone is the ONE shared code path). It runs the
        // per-id invalidations: the caches (names LRU, hover details,
        // decoded layouts) are keyed by ARENA id — a fresh scan reuses
        // ids for different nodes, so stale entries would surface
        // names, sizes and chips from the PREVIOUS target — and
        // retires any standing duplicates result (the engine's groups
        // reference the old arena).
        adoptDone(generation, stats, error, "standard");
      });
      unlisteners.push(un1, un2, un3, un4, un5);
    })();
  },
}));

/** Mount this once where scan events matter; it cleans up on unmount
 *  (StrictMode double-invocation is handled by the attach-once guard +
 *  the unlisten list living outside React state). */
export function useScanEvents() {
  useEffect(() => {
    useScanStore.getState().ensureListeners();
    return () => {
      // Keep listeners attached across StrictMode remounts; the app is
      // a single window — they detach when it closes.
    };
  }, []);
}
