/**
 * Top Sizes mode (spec §7.7): scope picker (In this folder / Biggest
 * files anywhere / Biggest folders anywhere — top-200 background-
 * computed in Rust, cached), ranked rows with pastel bars + parent
 * path for "anywhere" scopes, % of folder, size.
 */
import { useEffect, useState } from "react";
import { FolderIcon, FileIcon } from "../../components/Icon";
import { getTopSizes, type TopScopeId, type TopSizesData } from "../../viz/exploreIpc";
import { bytes } from "../../lib/format";
import { SkeletonRows } from "../../components/buttons";
import { useArrowNav } from "../../lib/useArrowNav";
import { useVizUiStore } from "../../state/vizUi";

const TONES = ["blue", "mint", "violet", "amber", "rose", "green", "sky", "slate"] as const;

const SCOPES: { id: TopScopeId; label: string }[] = [
  { id: "in-folder", label: "In this folder" },
  { id: "files-anywhere", label: "Biggest files anywhere" },
  { id: "folders-anywhere", label: "Biggest folders anywhere" },
];

export interface TopSizesModeProps {
  generation: number;
  folder: number;
  filter: string;
  selectedId: number | null;
  onSelect: (id: number | null) => void;
  onOpen: (id: number) => void;
  onContextMenu: (id: number, x: number, y: number) => void;
  onHover: (id: number | null, x: number, y: number) => void;
}

export function TopSizesMode(props: TopSizesModeProps) {
  // Scope lives in the vizUi store: switching modes/tabs and returning
  // used to reset it to "In this folder" every remount.
  const scope = useVizUiStore((s) => s.topScope);
  const setScope = useVizUiStore((s) => s.setTopScope);
  const [data, setData] = useState<TopSizesData | null>(null);
  const [stale, setStale] = useState(false);

  const key = `${props.generation}:${props.folder}:${scope}:${props.filter}`;

  useEffect(() => {
    let disposed = false;
    // No silent catch: a dropped stale response used to render the
    // EMPTY state ("Nothing to rank yet") — a lie during a rescan.
    // Show the honest reload state instead.
    // Smooth refetch (session 13): the PREVIOUS rows stay on screen
    // while the new generation's data lands (a cleanup commit bumps
    // the generation — skeletons flashing over rows the user is
    // reading is the jank the "realtime everywhere" report called
    // out). Skeletons appear only on the very first load (data null).
    setStale(false);
    void (async () => {
      try {
        const d = await getTopSizes(props.generation, props.folder, scope, props.filter);
        if (!disposed) setData(d);
      } catch {
        if (!disposed) setStale(true);
      }
    })();
    return () => {
      disposed = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  const rows = data?.rows ?? [];
  const max = rows.length > 0 ? Math.max(...rows.map((r) => r.size), 1) : 1;

  // Explorer-parity keyboard navigation: ↑/↓ move, Enter = dblclick
  // (open folder).
  useArrowNav({
    count: rows.length,
    selectedId: props.selectedId,
    idOf: (i) => rows[i].id,
    onMove: (i) => {
      props.onSelect(rows[i].id);
      document.querySelector(`[data-rank="${rows[i].rank}"]`)?.scrollIntoView({ block: "nearest" });
    },
    onActivate: (i) => props.onOpen(rows[i].id),
  });

  return (
    <div className="db-top-sizes">
      <div className="db-top-head">
        <div className="db-segmented" role="group" aria-label="Scope">
          {SCOPES.map((s) => (
            <button key={s.id} type="button" data-active={scope === s.id} onClick={() => setScope(s.id)}>
              {s.label}
            </button>
          ))}
        </div>
        <span className="db-shown tnum">{rows.length} shown</span>
      </div>
      {!data && stale && <div className="db-substate">Scan changed. Reloading…</div>}
      {!data && !stale && <SkeletonRows rows={10} className="db-ranked-skeleton" />}
      {data && rows.length === 0 && (
        <div className="db-substate">{props.filter ? `Nothing matches “${props.filter}”.` : "Nothing to rank yet."}</div>
      )}
      {data && rows.length > 0 && (
        <div className="db-ranked">
          {rows.map((r, i) => {
            const share = (r.size / max) * 100;
            const Icon = r.isDir ? FolderIcon : FileIcon;
            return (
              <button
                key={r.id}
                type="button"
                data-rank={r.rank}
                className={props.selectedId === r.id ? "is-selected" : ""}
                onClick={() => props.onSelect(r.id)}
                onDoubleClick={() => props.onOpen(r.id)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  props.onContextMenu(r.id, e.clientX, e.clientY);
                }}
                onPointerEnter={(e) => props.onHover(r.id, e.clientX, e.clientY)}
                onPointerLeave={() => props.onHover(null, 0, 0)}
              >
                <span className="db-rank tnum">{i + 1}</span>
                <Icon size={16} />
                <span
                  className={`rank-bar tone-${TONES[i % TONES.length]}`}
                  style={{ ["--share" as string]: `${Math.max(2.5, share)}%` }}
                >
                  <span>
                    <strong>{r.name}</strong>
                    {scope !== "in-folder" && r.parentPath && <small>{r.parentPath}</small>}
                  </span>
                </span>
                <small className="tnum">{r.isDir ? "" : r.kind}</small>
                <em className="tnum">{(r.share * 100).toFixed(1)}%</em>
                <b className="tnum">{bytes(r.size)}</b>
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
