/**
 * Cleanup Queue store (BuildPrompt §9) — its OWN Zustand store, exactly
 * as the spec demands (never copy queue items into other stores or
 * component state; the badge and popover subscribe via selectors).
 *
 * `commitToRecycleBin` (M5) invokes the Rust commit (IFileOperation +
 * pre-flight refusals + tree surgery); recycled items leave the queue,
 * failed items STAY with their reasons surfaced to the caller (the
 * popover shows the failure alert).
 */
import { create } from "zustand";
import { invoke } from "../lib/ipc";
import { EVENTS, track } from "../lib/analytics";
import { PLATFORM, type Platform } from "../lib/platform";

/** One staged item (spec §9: id, path, size, reason). */
export interface QueueItem {
  /** Real node id, or the synthetic id of a regroup group. */
  id: number;
  /** Display path of the staged item. */
  path: string;
  /** Size on disk in bytes. */
  size: number;
  /** Why it was staged ("Duplicate", "Leftovers: …", "Large media", …). */
  reason: string;
}

/** The Rust commit response (cleanup-committed shape). */
export interface CommitResult {
  generation: number;
  trashed: { path: string; alreadyGone: boolean; nested: boolean }[];
  failed: { path: string; reason: string }[];
  stats: [number, number, number, number] | null;
  currentFolder: number;
  selectedNode: number | null;
}

interface CleanupState {
  items: QueueItem[];
  stage: (item: QueueItem) => void;
  stageMany: (items: QueueItem[]) => void;
  unstage: (id: number, path?: string) => void;
  remove: (id: number, path?: string) => void;
  clear: () => void;
  contains: (id: number, path?: string) => boolean;
  totalSize: () => number;
  /** Commit to the Recycle Bin after explicit confirmation (M5).
   *  Rejects on stale generation / COM failure — the queue stays intact
   *  (safe default) and the popover surfaces the error. */
  commitToRecycleBin: () => Promise<CommitResult>;
  /** Permanently delete after explicit confirmation (owner decision,
   *  session 13 — the "Delete permanently" popover action). Same error
   *  contract: the queue survives failures. */
  commitToDeletePermanently: () => Promise<CommitResult>;
}

/** Stable identity: PATH-first (paths are unique on disk — the same
 * file staged through the inspector (real id) AND a path-only surface
 * (id 0, e.g. a leftover or duplicate) must land in the queue ONCE;
 * id-only keying let it appear twice and unstage(0) nuked every
 * path-only row). Path-less items fall back to the node id. */
export const pathIdentityFor = (path: string, platform: Platform): string =>
  platform === "windows" ? path.toLocaleLowerCase("en-US") : path;

export const pathIdentity = (path: string): string => pathIdentityFor(path, PLATFORM);

const keyOf = (i: Pick<QueueItem, "id" | "path">): string =>
  i.path && i.path.length > 0 ? `p:${pathIdentity(i.path)}` : `i:${i.id}`;

/** The staged queue. Popover + badge subscribe via selectors (spec §9). */
export const useCleanupStore = create<CleanupState>((set, get) => ({
  items: [],

  stage: (item) =>
    set((s) => {
      if (s.items.some((i) => keyOf(i) === keyOf(item))) return s; // idempotent
      return { items: [...s.items, item] };
    }),

  stageMany: (items) => {
    if (items.length > 0) {
      const source =
        // Reason shapes: "Leftovers: App (Cache)", "Downloads (Quick win)",
        // "Big & Untouched", "Duplicate", "Manual". Legacy " — " separators
        // still parse during the transition.
        items[0].reason
          .split(":")[0]
          ?.split(" — ")[0]
          ?.split(" (")[0]
          ?.trim() ?? "manual";
      track(EVENTS.cleanupStaged, {
        source,
        items: items.length,
        bytes: items.reduce((a, b) => a + b.size, 0),
      });
    }
    set((s) => {
      // Dedupe against the queue AND within the batch (same keyOf).
      const seen = new Set(s.items.map((i) => keyOf(i)));
      const add = items.filter((i) => {
        const k = keyOf(i);
        if (seen.has(k)) return false;
        seen.add(k);
        return true;
      });
      return add.length ? { items: [...s.items, ...add] } : s;
    });
  },

  /** Remove by id — with a path, removes exactly one synthetic item
   * (Duplicates id=0 rows); without, removes every item with that id
   * (real node ids are unique in the queue). */
  unstage: (id, path) =>
    set((s) => ({
      items: s.items.filter((i) => (path === undefined ? i.id !== id : !(i.id === id && i.path === path))),
    })),

  remove: (id, path) =>
    set((s) => ({
      items: s.items.filter((i) => (path === undefined ? i.id !== id : !(i.id === id && i.path === path))),
    })),

  clear: () => set({ items: [] }),

  contains: (id, path) =>
    get().items.some((i) => (path === undefined ? i.id === id : i.id === id && i.path === path)),

  totalSize: () => get().items.reduce((acc, i) => acc + i.size, 0),

  commitToRecycleBin: () => commitItems("commit_cleanup"),

  commitToDeletePermanently: () => commitItems("delete_permanently"),
}));

/** The shared commit body for both delete modes (recycle / permanent):
 * resolve the generation, send the UI's live navigation with the items
 * (the surgery may remove the ON-SCREEN folder — the response carries
 * the survivor fixup), then drop every recycled path from the queue.
 * Failures reject — the queue stays intact and the popover surfaces
 * the error. */
async function commitItems(cmd: "commit_cleanup" | "delete_permanently"): Promise<CommitResult> {
  const status = await invoke<{ generation: number }>("get_status");
  const items = useCleanupStore.getState().items.map((i) => ({
    id: i.id,
    path: i.path,
    size: i.size,
    reason: i.reason,
  }));
  // The live navigation rides along so the tree surgery can fix up a
  // view whose folder it removes (module-level import: function-only
  // usage, init-safe).
  const { useExploreStore } = await import("./explore");
  const nav = useExploreStore.getState();
  const result = await invoke<CommitResult>(cmd, {
    generation: status.generation,
    items,
    currentFolder: nav.currentFolder,
    selectedNode: nav.selectedNode,
  });
  // Recycled (incl. already-gone + nested-with-parent + duplicate
  // identities) leave the queue. Windows path identity is
  // case-insensitive; macOS preserves case because APFS may be
  // case-sensitive and `/Data/A` need not be `/Data/a`.
  const recycled = new Set(result.trashed.map((t) => pathIdentity(t.path)));
  useCleanupStore.setState((s) => ({
    items: s.items.filter((i) => !recycled.has(pathIdentity(i.path))),
  }));
  return result;
}
