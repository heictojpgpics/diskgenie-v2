/**
 * Explore view (spec §7): idle / scanning / done / error states, the
 * big header (folder name + stats + reveal), the unreadable notice,
 * the mode toolbar, and the stage hosting one of the 9 modes. Wires the
 * shared interaction set (select, dblclick-open, context menu, hover
 * chip, preview) into every mode.
 */
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { animate, motion, useMotionValue, useTransform } from "framer-motion";
import { ExternalLinkIcon, FolderIcon, HardDriveIcon, RefreshCwIcon, ScanLineIcon, SquareIcon, UacShieldIcon } from "../components/Icon";
import { EmptyState } from "../components/buttons";
import { UnreadableNotice } from "../sidebar";
import { recentTargetLabel } from "../sidebar/RecentSection";
import { MACHINE_NOUN, SCAN_THIS_PC } from "../lib/platform";
import { ExploreHeader } from "./ExploreHeader";
import { FoldersMode } from "./modes/FoldersMode";
import { CanvasMode } from "./modes/CanvasMode";
import { TopSizesMode } from "./modes/TopSizesMode";
import { AgeMapMode } from "./modes/AgeMapMode";
import { ListMode } from "./modes/ListMode";
import { HoverChip, type HoverChipHandle } from "../components/HoverChip";
import { ItemContextMenu, type ItemMenuState } from "../components/ItemContextMenu";
import { TailPath } from "../components/TailPath";
import { invoke } from "../lib/ipc";
import { bytes } from "../lib/format";
import { useExploreStore } from "../state/explore";
import { useScanStore } from "../state/scan";
import { useViewStore } from "../state/view";
import { useVizUiStore, type Mode } from "../state/vizUi";
import { useCleanupStore } from "../state/cleanup";
import { CANVAS_MODES } from "../state/vizUi";
import { getHoverDetails } from "../viz/layoutIpc";
import { getNodeDetails, openNode, type NodeDetailsData } from "../viz/exploreIpc";
import { EVENTS, track } from "../lib/analytics";

/** Smoothly-rolling "N files · X GB" live counter (motion values, no
 * per-tick React re-render churn — the 150 ms IPC ticks TWEEN into each
 * other so the numbers glide instead of jumping). */
function ScanCounter({ files, totalBytes }: { files: number; totalBytes: number }) {
  const filesMv = useMotionValue(files);
  const bytesMv = useMotionValue(totalBytes);
  useEffect(() => {
    void animate(filesMv, files, { duration: 0.5, ease: "easeOut" });
    void animate(bytesMv, totalBytes, { duration: 0.5, ease: "easeOut" });
  }, [files, totalBytes, filesMv, bytesMv]);
  const text = useTransform([filesMv, bytesMv], ([f, b]: number[]) =>
    `${Math.max(0, Math.round(f)).toLocaleString()} files · ${bytes(Math.max(0, b))}`,
  );
  // NOTE: no aria-live on this counter — it updates every 150 ms and
  // would spam assistive tech; the "Scanning…" heading already carries
  // the state, and scan completion announces through the store swap.
  return <motion.span className="db-live-counter tnum">{text}</motion.span>;
}

/** Stage-swap wrapper (mode level — see App.tsx TabSwap for the
 * settle-in design): the entering mode fades 0→1 over the solid
 * stage background; the old mode unmounts INSTANTLY. No lingering
 * exit layer → no double exposure (the "shadow / page-in-page"
 * ghost), no 5 px rise → no zoom read. The stage geometry is STABLE
 * during mode switches (no grid change), so the entering canvas
 * mounts at its final size — the fetch window (~1 IPC, cached on
 * revisits) passes at opacity ≈ 0, invisible.
 *
 * CSS keyframe animation (`db-settle-in`), NOT framer — the same
 * WAAPI async-cleanup gap as the tab level lived here (one blank
 * frame ~150 ms after every mode switch); see TabSwap for the full
 * story. */
function StageSwap({ children }: { children: ReactNode }) {
  return (
    <div className="db-stage-swap">
      {children}
    </div>
  );
}

export function ExploreView({ onPreview }: { onPreview: (id: number) => void }) {
  const status = useScanStore((s) => s.status);
  const progress = useScanStore((s) => s.progress);
  const error = useScanStore((s) => s.error);
  const scanTarget = useScanStore((s) => s.scanTarget);
  const cancelScan = useScanStore((s) => s.cancelScan);
  const generation = useScanStore((s) => s.generation);
  const currentFolder = useExploreStore((s) => s.currentFolder);
  const selectedNode = useExploreStore((s) => s.selectedNode);
  const openFolder = useExploreStore((s) => s.openFolder);
  const select = useExploreStore((s) => s.select);
  const nameFilter = useViewStore((s) => s.nameFilter);
  const mode = useVizUiStore((s) => s.mode);
  const stage = useCleanupStore((s) => s.stage);

  const chip = useRef<HoverChipHandle>(null);
  const stageEl = useRef<HTMLElement | null>(null);
  const hoverSeq = useRef(0);
  const [menu, setMenu] = useState<ItemMenuState | null>(null);
  const [folderView, setFolderView] = useState<NodeDetailsData | null>(null);
  // Shift+F10: the Windows keyboard context-menu key. Same contract as
  // right-click, but for the SELECTED item (the keyboard-nav surrogate
  // for "the item with focus"): the menu opens at the focused element
  // when one lives in the stage (DOM rows, folder cards), else at the
  // stage center (canvas modes keep their selection off-DOM). Skipped
  // while typing — the search box owns its keys.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "F10" || !e.shiftKey) return;
      const sel = useExploreStore.getState().selectedNode;
      if (sel == null) return;
      const ae = document.activeElement;
      const tag = ae?.tagName?.toLowerCase();
      if (tag === "input" || tag === "textarea" || (ae as HTMLElement | null)?.isContentEditable) return;
      e.preventDefault();
      const stage = stageEl.current;
      const inStage = ae instanceof HTMLElement && stage?.contains(ae) ? (ae as HTMLElement) : null;
      let x: number;
      let y: number;
      if (inStage) {
        const r = inStage.getBoundingClientRect();
        x = r.left + 4;
        y = r.bottom + 6;
      } else {
        const r = stage?.getBoundingClientRect();
        x = (r?.left ?? 0) + (r?.width ?? 0) / 2;
        y = (r?.top ?? 0) + (r?.height ?? 0) / 2;
      }
      setMenu({ id: sel, x, y });
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Throttled path ticker: the raw currentPath changes every 150 ms
  // (unreadable strobe); display it at ~600 ms with a soft crossfade.
  const [displayPath, setDisplayPath] = useState("");
  const lastPathSwap = useRef(0);
  useEffect(() => {
    if (status !== "scanning" || !progress?.currentPath) return;
    const now = performance.now();
    if (now - lastPathSwap.current < 600) return;
    lastPathSwap.current = now;
    setDisplayPath(progress.currentPath);
  }, [status, progress]);

  // Folder header info (name + stats for the current folder)
  useEffect(() => {
    if (status !== "done") {
      setFolderView(null);
      return;
    }
    let disposed = false;
    void (async () => {
      const d = await getNodeDetails(generation, currentFolder).catch(() => null);
      if (!disposed) setFolderView(d);
    })();
    return () => {
      disposed = true;
    };
  }, [status, generation, currentFolder]);

  // ── Hover chip controller (refs; never re-renders on pointer moves) ─
  // A view change orphans a pinned chip: programmatic navigation (mode
  // switch, folder open, new generation) never fires pointerleave, so
  // the chip kept floating over the NEW view (CI tour frames showed it
  // stuck over treemap/top-sizes). Hide on every transition.
  useEffect(() => {
    chip.current?.hide();
  }, [mode, currentFolder, status, generation, nameFilter]);

  const hoverFetch = useCallback(
    (id: number | null, x: number, y: number) => {
      if (id == null || id < 0) {
        hoverSeq.current += 1; // in-flight fetches are now stale
        chip.current?.hide();
        return;
      }
      chip.current?.move(x, y);
      // Sequence guard: hovering A→B quickly lets A's async details
      // resolve LAST and paint A's data under B's pointer. Only the
      // NEWEST hover may show.
      const seq = ++hoverSeq.current;
      void (async () => {
        const d = await getHoverDetails(generation, id).catch(() => null);
        if (seq !== hoverSeq.current) return; // superseded by a newer hover/hide
        if (d) chip.current?.show(
          {
            name: d.name,
            size: d.size,
            shareOfScan: d.shareOfScan,
            fileCount: d.fileCount,
            isDir: d.isDir,
            category: d.category,
            categoryColor: d.categoryColor,
            isCloud: d.isCloud,
            isProtected: d.isProtected,
          },
          x,
          y,
        );
      })();
    },
    [generation],
  );

  const menuResolver = useCallback(
    async (id: number) => {
      const d = await getHoverDetails(generation, id).catch(() => null);
      if (!d) return null;
      const details = await getNodeDetails(generation, id).catch(() => null);
      return {
        id,
        name: details?.name ?? "",
        isDir: d.isDir,
        isProtected: d.isProtected,
        isCloud: d.isCloud,
      };
    },
    [generation],
  );

  // ── Shared actions ──────────────────────────────────────────────────
  const actions = useMemo(
    () => ({
      // Open: folders drill in (the app's universal Open semantic);
      // files launch their default app via open_node — the context
      // menu's contract (the inspector's slot 3 moved on to
      // "Duplicates here" in session 15; drill/launch still live here,
      // on double-click, and in the preview overlay).
      open: (id: number, isDir: boolean) => {
        track(EVENTS.searchUsed, { mode });
        if (isDir) {
          openFolder(id);
          return;
        }
        // openNode toasts on failure — an Open that silently does
        // nothing is the defect that killed the Focus button.
        void openNode(generation, id);
      },
      preview: (id: number) => onPreview(id),
      reveal: (id: number) => void invoke("reveal_in_explorer", { generation, id }).catch(() => undefined),
      copyPath: (id: number) => {
        void invoke("copy_path", { generation, id }).catch(() => undefined);
      },
      stage: (id: number) => {
        void (async () => {
          const d = await getNodeDetails(generation, id).catch(() => null);
          if (d) {
            stage({ id, path: d.path, size: d.size, reason: "Manual" });
          }
        })();
      },
    }),
    [generation, mode, onPreview, openFolder, stage],
  );

  // Dev-hook auto-start (spec §15 DISKGENIE_SCAN / --scan)
  useEffect(() => {
    if (status !== "idle") return;
    void (async () => {
      const hooks = await invoke<{ scan: string | null; mode: string | null }>("get_dev_hooks").catch(() => null);
      if (hooks?.scan) {
        const startScan = useScanStore.getState().startScan;
        void startScan(hooks.scan);
      }
      if (hooks?.mode) {
        useVizUiStore.getState().setMode(hooks.mode as Mode);
      }
    })();
    // Only on mount — the hook fires once per process.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const revealCurrent = () => {
    void invoke("reveal_in_explorer", { generation, id: currentFolder }).catch(() => undefined);
  };

  // ── Render by state ────────────────────────────────────────────────
  if (status === "idle") {
    return (
      <div className="db-main">
        <div className="db-state">
          <span className="db-idle-art">
            <FolderIcon size={34} />
          </span>
          <h2>See where your space went</h2>
          <p>
            Pick a target and DiskGenie maps every file and folder. You get a live
            tree you can drill into, filter, and clean up. Whole {MACHINE_NOUN},
            your Home folder, or any single folder. Your call.
          </p>
          <div className="db-state-actions">
            <button
              type="button"
              className="db-ink-button auto"
              onClick={() => {
                track(EVENTS.scanStarted, { target: "ThisPC" });
                void useScanStore.getState().startScan("ThisPC");
              }}
            >
              <ScanLineIcon size={16} /> {SCAN_THIS_PC}
            </button>
            <button
              type="button"
              className="db-outline auto"
              onClick={async () => {
                try {
                  const { open } = await import("@tauri-apps/plugin-dialog");
                  const picked = await open({ directory: true, multiple: false, title: "Scan" });
                  if (typeof picked === "string" && picked.length > 0) {
                    track(EVENTS.scanStarted, { target: "folder" });
                    void useScanStore.getState().startScan(picked);
                  }
                } catch {
                  const home = await invoke<string>("get_home_path").catch(() => null);
                  if (home) void useScanStore.getState().startScan(home);
                }
              }}
            >
              <ExternalLinkIcon size={14} /> Choose Folder…
            </button>
          </div>
        </div>
      </div>
    );
  }

  if (status === "scanning") {
    return (
      <div className="db-main">
        <div className="db-state db-scanning">
          {/* Premium radial disk sweep v2 (transform-only CSS, 60 fps):
           * main orbit + faint counter-rotating outer tail + breathing core. */}
          <div className="db-scan-visual" aria-hidden="true">
            {/* Two guide rings (anchor + texture); a third at r=30 sat
             * inside the core's pulse-glow radius and shimmered every
             * 2.4 s cycle — removed rather than crowded. */}
            <span className="db-scan-ring r1" />
            <span className="db-scan-ring r2" />
            <span className="db-scan-sweep2" />
            <span className="db-scan-sweep" />
            <span className="db-scan-core">
              <HardDriveIcon size={22} />
            </span>
          </div>
          <h2>Scanning…</h2>
          <ScanCounter files={progress?.files ?? 0} totalBytes={progress?.bytes ?? 0} />
          <TailPath path={displayPath} className="db-current-path" />
          <button type="button" className="db-outline db-cancel-scan" onClick={() => void cancelScan()}>
            <SquareIcon size={13} /> Stop scan
          </button>
        </div>
      </div>
    );
  }

  if (status === "error") {
    const elevation = error?.includes("ELEVATION_REQUIRED");
    return (
      <div className="db-main">
        <div className="db-state db-error">
          <h2>{elevation ? "Administrator rights needed" : "Scan failed"}</h2>
          <p>{elevation ? "This scan target needs elevation. Restart as administrator and it re-runs automatically." : (error ?? "Something went wrong.")}</p>
          <div className="db-state-actions">
            <button
              type="button"
              className="db-ink-button auto"
              onClick={() => {
                if (elevation) {
                  void invoke("restart_as_admin", { scanTarget, turbo: true }).catch(() => undefined);
                } else {
                  void useScanStore.getState().startScan("ThisPC");
                }
              }}
            >
              {elevation ? <><UacShieldIcon size={14} /> Restart as administrator</> : "Try again"}
            </button>
          </div>
        </div>
      </div>
    );
  }

  // status === "done"
  const st = folderView;
  return (
    <div className="db-main">
      <div className="db-content-head">
        <div className="db-title-row">
          <div>
            <h1>{st?.name ?? "Scan"}</h1>
            <span className="db-stat size tnum">{st ? bytes(st.size) : ""}</span>
            <i className="db-dot-sep" />
            <span className="db-stat tnum">{(st?.files ?? 0).toLocaleString()} files</span>
            <i className="db-dot-sep" />
            <span className="db-stat tnum">{(st?.folders ?? 0).toLocaleString()} folders</span>
          </div>
          <div className="db-title-actions">
            <button
              type="button"
              className="db-icon-button"
              onClick={() => void useScanStore.getState().startScan(scanTarget, true)}
              aria-label="Rescan this target"
              title={`Rescan ${recentTargetLabel(scanTarget)}`}
            >
              <RefreshCwIcon size={15} />
            </button>
            <button
              type="button"
              className="db-icon-button"
              onClick={revealCurrent}
              aria-label="Show in Explorer"
              title="Show in Explorer"
            >
              <ExternalLinkIcon size={15} />
            </button>
          </div>
        </div>
        <ExploreHeader />
        <UnreadableNotice />
      </div>
      <section ref={stageEl} className="db-visual-stage db-scroll" aria-label={`${mode} visualization`}>
        {/* Mode settle-in swap (see StageSwap): the old mode unmounts
         * instantly; the new one fades in over the solid stage at its
         * final geometry. `key` covers mode + folder + generation —
         * every navigation reads as one gesture. */}
        <StageSwap key={`${generation}:${currentFolder}:${mode}`}>
          {mode === "Folders" && (
            <FoldersMode
              generation={generation}
              folder={currentFolder}
              filter={nameFilter}
              selectedId={selectedNode}
              onSelect={select}
              onOpen={(id) => actions.open(id, true)}
              onPreview={onPreview}
              onContextMenu={(id, x, y) => setMenu({ id, x, y })}
              onHover={hoverFetch}
            />
          )}
          {CANVAS_MODES.has(mode) && (
            <CanvasMode
              generation={generation}
              folder={currentFolder}
              mode={mode}
              selectedId={selectedNode}
              onSelect={select}
              onOpen={(id) => actions.open(id, true)}
              onContextMenu={(id, x, y) => setMenu({ id, x, y })}
              onHover={hoverFetch}
            />
          )}
          {mode === "Top Sizes" && (
            <TopSizesMode
              generation={generation}
              folder={currentFolder}
              filter={nameFilter}
              selectedId={selectedNode}
              onSelect={select}
              onOpen={(id) => actions.open(id, true)}
              onContextMenu={(id, x, y) => setMenu({ id, x, y })}
              onHover={hoverFetch}
            />
          )}
          {mode === "Age Map" && (
            <AgeMapMode
              generation={generation}
              folder={currentFolder}
              onSelect={select}
              onHover={hoverFetch}
              onContextMenu={(id, x, y) => setMenu({ id, x, y })}
            />
          )}
          {mode === "List" && (
            <ListMode
              generation={generation}
              folder={currentFolder}
              filter={nameFilter}
              selectedId={selectedNode}
              onSelect={select}
              onOpen={(id) => actions.open(id, true)}
              onContextMenu={(id, x, y) => setMenu({ id, x, y })}
              onHover={hoverFetch}
            />
          )}
        </StageSwap>
      </section>

      <HoverChip ref={chip} sizeFmt={bytes} />
      <ItemContextMenu target={menu} actions={actions} resolver={menuResolver} onClose={() => setMenu(null)} />
    </div>
  );
}

export { EmptyState };
