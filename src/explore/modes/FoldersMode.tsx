/**
 * Folders mode (spec §7.2, the default): virtualized grid of
 * folder-shaped cards (tab + sheen + pastel tint, hover lift, 3
 * category dots, "N items", size) + the Files tiles below (category
 * icon, name, "Category · relative age", size; dblclick → Preview).
 * The whole body scrolls inside the visual stage (one scroller).
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { CheckIcon, LockKeyholeIcon, categoryIcon } from "../../components/Icon";
import { getFolderView, type FolderViewData } from "../../viz/exploreIpc";
import { bytes, relativeAge } from "../../lib/format";
import { useCleanupStore } from "../../state/cleanup";
import { Skeleton } from "../../components/buttons";
import { useArrowNav } from "../../lib/useArrowNav";

const TONES = ["blue", "mint", "violet", "amber", "rose", "green", "sky", "slate"];

export interface FoldersModeProps {
  generation: number;
  folder: number;
  filter: string;
  selectedId: number | null;
  onSelect: (id: number | null) => void;
  onOpen: (id: number) => void;
  onPreview: (id: number) => void;
  onContextMenu: (id: number, x: number, y: number) => void;
  onHover: (id: number | null, x: number, y: number) => void;
}

export function FoldersMode(props: FoldersModeProps) {
  const [data, setData] = useState<FolderViewData | null>(null);
  const [stale, setStale] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  // Live staged-set mirror: cards flip to the staged badge the instant
  // an item joins/leaves the cleanup queue (spec §9 — the queue is the
  // single source of truth; components subscribe via selectors).
  const queueItems = useCleanupStore((s) => s.items);
  const staged = useMemo(() => new Set(queueItems.map((i) => i.id)), [queueItems]);

  const key = `${props.generation}:${props.folder}:${props.filter}`;

  useEffect(() => {
    let disposed = false;
    setStale(false);
    void (async () => {
      try {
        const d = await getFolderView(props.generation, props.folder, props.filter);
        if (!disposed) setData(d);
      } catch (e) {
        if (!disposed) setStale(String(e).includes("stale generation"));
      }
    })();
    return () => {
      disposed = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  // Responsive column count (the grid column axis; rows are virtual).
  const [cols, setCols] = useState(2);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      setCols(Math.max(1, Math.floor((el.clientWidth - 16) / 288)));
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [data]);

  const rows = useMemo(() => {
    if (!data) return [];
    const out: typeof data.folders[] = [];
    for (let i = 0; i < data.folders.length; i += cols) {
      out.push(data.folders.slice(i, i + cols));
    }
    return out;
  }, [data, cols]);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 162,
    overscan: 4,
  });

  // Explorer-parity grid navigation: arrows move between cards (↑/↓ by
  // column, ←/→ by one), Enter opens. Files below the grid extend the
  // SAME linear order (folderCount + fileCount items; the file rows
  // share the 240px-min column template, so the measured `cols` applies
  // to both sections) — the comment used to claim this; now it's true.
  const folderCount = data?.folders.length ?? 0;
  const fileCount = data?.files.length ?? 0;
  useArrowNav({
    count: folderCount + fileCount,
    selectedId: props.selectedId,
    idOf: (i) =>
      i < folderCount
        ? (data?.folders[i]?.id ?? -1)
        : (data?.files[i - folderCount]?.id ?? -1),
    grid: { columns: () => cols },
    onMove: (i) => {
      if (i < folderCount) {
        const f = data?.folders[i];
        if (!f) return;
        props.onSelect(f.id);
        virtualizer.scrollToIndex(Math.floor(i / cols), { align: "auto" });
      } else {
        const f = data?.files[i - folderCount];
        if (!f) return;
        props.onSelect(f.id);
        document
          .querySelector(`[data-file-nav="${f.id}"]`)
          ?.scrollIntoView({ block: "nearest" });
      }
    },
    onActivate: (i) => {
      if (i < folderCount) {
        const f = data?.folders[i];
        if (f) props.onOpen(f.id);
      } else {
        // Enter on a file previews it (the row's own key handler parity).
        const f = data?.files[i - folderCount];
        if (f) props.onPreview(f.id);
      }
    },
  });

  if (stale || !data) {
    // Structure preview: folder-card skeletons in the real grid rhythm
    // (auto-fill minmax(240px,1fr)) + the heading chip — the stage keeps
    // its height, so data landing causes no reflow. Stale adds the
    // honest label.
    return (
      <div className="db-folders-view">
        <div className="db-view-heading">
          <h2>Folders</h2>
          <Skeleton w={38} h={17} pill />
        </div>
        <div className="db-folders-scroll db-scroll">
          <div className="db-folder-skeleton-grid">
            {Array.from({ length: 6 }, (_, i) => (
              <div key={i} className="db-folder-skeleton" aria-hidden="true">
                <span className="db-skeleton" style={{ width: "34%", height: 12 }} />
                <span className="db-skeleton" style={{ width: "22%", height: 10, opacity: 0.75 }} />
                <span className="db-skeleton" style={{ width: "58%", height: 10, marginTop: 6 }} />
              </div>
            ))}
          </div>
          <div className="db-loading-block" role="status">
            <span>{stale ? "Scan changed. Reloading…" : "Loading folders…"}</span>
          </div>
        </div>
      </div>
    );
  }

  const now = Math.floor(Date.now() / 1000);

  return (
    <div className="db-folders-view">
      <div className="db-view-heading">
        <h2>Folders</h2>
        <span className="db-count-chip tnum">{data.folders.length}</span>
      </div>
      <div ref={scrollRef} className="db-folders-scroll db-scroll">
        {data.folders.length === 0 ? (
          <div className="db-substate">{props.filter ? `No folders match “${props.filter}”.` : "This folder is empty."}</div>
        ) : (
          <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
            {virtualizer.getVirtualItems().map((vi) => (
              <div
                key={vi.key}
                style={{
                  position: "absolute",
                  top: 0,
                  left: 0,
                  width: "100%",
                  transform: `translateY(${vi.start}px)`,
                }}
              >
                <div
                  style={{
                    display: "grid",
                    gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`,
                    gap: 14,
                    padding: "0 0 14px",
                  }}
                >
                  {rows[vi.index].map((f, i) => (
                    <button
                      key={f.id}
                      type="button"
                      className={`db-folder-card tone-${TONES[(vi.index * cols + i) % TONES.length]} ${
                        props.selectedId === f.id ? "is-selected" : ""
                      } ${f.protected ? "is-protected" : ""}`}
                      style={{ ["--i" as string]: vi.index * cols + i }}
                      onClick={() => props.onSelect(f.id)}
                      onDoubleClick={() => props.onOpen(f.id)}
                      onContextMenu={(e) => {
                        e.preventDefault();
                        props.onContextMenu(f.id, e.clientX, e.clientY);
                      }}
                      onPointerEnter={(e) => props.onHover(f.id, e.clientX, e.clientY)}
                      onPointerLeave={() => props.onHover(null, 0, 0)}
                    >
                      {f.protected && (
                        <span className="db-folder-protected-badge" title="Windows manages this item">
                          <LockKeyholeIcon size={13} />
                        </span>
                      )}
                      {staged.has(f.id) && (
                        <span className="db-folder-staged-badge" title="Staged for cleanup">
                          <CheckIcon size={12} /> Staged
                        </span>
                      )}
                      <span className="db-folder-tab" />
                      <span className="db-folder-name">{f.name}</span>
                      <span className="db-folder-meta">
                        <span className="db-dots">
                          {f.categories.slice(0, 3).map((c) => (
                            <i
                              key={c.label}
                              style={{ background: `#${c.color.toString(16).padStart(6, "0")}` }}
                              title={c.label}
                            />
                          ))}
                          <span style={{ marginLeft: 7, fontWeight: 550 }}>
                            {f.itemCount.toLocaleString()} items
                          </span>
                        </span>
                        <strong className="tnum">{bytes(f.size)}</strong>
                      </span>
                    </button>
                  ))}
                </div>
              </div>
            ))}
          </div>
        )}
        {data.folders.length > 0 && (
          <div className="db-files-heading db-view-heading" style={{ paddingTop: 18, marginTop: 4 }}>
            <h2>Files</h2>
            <span className="db-count-chip tnum">
              {data.files.length}
              {data.filesCapped ? "+" : ""}
            </span>
          </div>
        )}
        {data.files.length === 0 ? (
          /* Shown only when folders DID match — the folders section
           * already announced the unmatched filter; stacking both
           * notices repeated one condition twice. */
          data.folders.length > 0 ? (
            <div className="db-substate inline">
              {props.filter ? `No files match “${props.filter}”.` : "No files in this folder."}
            </div>
          ) : null
        ) : (
          <div className="db-files-grid">
            {data.files.map((f) => {
              const Icon = categoryIcon(f.category);
              return (
                <button
                  key={f.id}
                  type="button"
                  data-file-nav={f.id}
                  className={`db-file-row ${props.selectedId === f.id ? "is-selected" : ""}`}
                  style={{ ["--file-cat" as string]: `#${f.categoryColor.toString(16).padStart(6, "0")}` }}
                  onClick={() => props.onSelect(f.id)}
                  onDoubleClick={() => props.onPreview(f.id)}
                  /* Enter on a focused file row previews it: dblclick was
                   * the ONLY path to the preview from the grid — three
                   * clicks deep for keyboard users otherwise. */
                  onKeyDown={(e) => {
                    if (e.key === "Enter") props.onPreview(f.id);
                  }}
                  onContextMenu={(e) => {
                    e.preventDefault();
                    props.onContextMenu(f.id, e.clientX, e.clientY);
                  }}
                  onPointerEnter={(e) => props.onHover(f.id, e.clientX, e.clientY)}
                  onPointerLeave={() => props.onHover(null, 0, 0)}
                >
                  <Icon size={18} />
                  <span>
                    <strong>{f.name}</strong>
                    <small>
                      {f.category} · {relativeAge(f.modified, now)}
                      {f.cloud ? " · cloud" : ""}
                    </small>
                  </span>
                  <b className="tnum">{f.cloud ? "n/a" : bytes(f.size)}</b>
                </button>
              );
            })}
          </div>
        )}
        {data.filesCapped && (
          <div className="db-files-more">
            Showing the first {data.files.length} files. Refine the filter to see more.
          </div>
        )}
      </div>
    </div>
  );
}
