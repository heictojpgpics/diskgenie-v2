/**
 * Age Map mode (spec §7.8): "How old are these bytes?" 6 buckets,
 * year×month heatmap with less/more legend + busiest month, and
 * Big & Untouched (≥40 MB, >1 year) with +/✓ stage toggles and
 * "Stage top N for cleanup".
 */
import { useEffect, useState } from "react";
import { CheckIcon, FileIcon, PlusIcon } from "../../components/Icon";
import { TailPath } from "../../components/TailPath";
import { getAgeMap, type AgeMapDataData, type BigRowData } from "../../viz/exploreIpc";
import { bytes, relativeAge } from "../../lib/format";
import { useCleanupStore } from "../../state/cleanup";
import { getNodeDetails } from "../../viz/exploreIpc";
import { Spinner } from "../../components/buttons";

const AGE_CSS = ["var(--age-0)", "var(--age-1)", "var(--age-2)", "var(--age-3)", "var(--age-4)", "var(--age-5)"];
const MONTH_LETTERS = ["J", "F", "M", "A", "M", "J", "J", "A", "S", "O", "N", "D"];
/** Full names for the prose surfaces (busiest label, cell tooltips) —
 * the one-letter grid column labels must never leak into sentences
 * ("busiest: O 2026", the session-14 report). */
const MONTH_NAMES = [
  "January", "February", "March", "April", "May", "June",
  "July", "August", "September", "October", "November", "December",
];

export interface AgeMapModeProps {
  generation: number;
  folder: number;
  onSelect: (id: number | null) => void;
  onHover: (id: number | null, x: number, y: number) => void;
  onContextMenu: (id: number, x: number, y: number) => void;
}

export function AgeMapMode(props: AgeMapModeProps) {
  const [data, setData] = useState<AgeMapDataData | null>(null);
  const [stale, setStale] = useState(false);
  const stage = useCleanupStore((s) => s.stage);
  // Subscribe to the ITEMS themselves (not the contains() helper — a
  // stable function selector never re-renders, so the + → ✓ toggle
  // flip never painted; the badge updated but the row lied).
  const queuedIds = useCleanupStore((s) => new Set(s.items.map((i) => i.id)));
  const unstage = useCleanupStore((s) => s.unstage);

  useEffect(() => {
    let disposed = false;
    // A dropped stale response used to leave `data` null forever — the
    // bucketing spinner spun for eternity under a rescan. FoldersMode
    // already had the honest pattern: say the scan changed and wait.
    setStale(false);
    // A changed generation/folder must never leave a previous heatmap on
    // screen while the next request is in flight.  Apart from being visually
    // misleading, that hid real-data fetch failures behind mock-looking data.
    setData(null);
    void (async () => {
      try {
        const d = await getAgeMap(props.generation, props.folder);
        if (!disposed) setData(d);
      } catch {
        if (!disposed) setStale(true);
      }
    })();
    return () => {
      disposed = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.generation, props.folder]);

  if (!data || stale) {
    // Honest wait states: initial bucketing OR a dropped stale response
    // (the old code kept rendering previous-generation data forever on
    // a failed refetch — FoldersMode's pattern, applied here too).
    return (
      <div className="db-loading-block" role="status">
        <Spinner />
        <span>{stale ? "Scan changed — reloading…" : "Bucketing by age…"}</span>
      </div>
    );
  }

  const total = data.total || 1;
  const heat = data.heatmap;
  const years: number[] = [];
  for (let y = heat.firstYear; y <= heat.lastYear; y++) years.push(y);
  const busiest = heat.busiest;

  const stageBig = async (row: BigRowData) => {
    const d = await getNodeDetails(props.generation, row.id).catch(() => null);
    if (d) {
      stage({ id: row.id, path: d.path, size: row.onDisk || row.logical, reason: "Big & Untouched" });
    }
  };

  const stageTop = async () => {
    for (const row of data.big.slice(0, 25)) {
      if (row.protected) continue;
      const d = await getNodeDetails(props.generation, row.id).catch(() => null);
      if (d) stage({ id: row.id, path: d.path, size: row.onDisk || row.logical, reason: "Big & Untouched" });
    }
  };

  const now = Math.floor(Date.now() / 1000);

  return (
    <div className="db-age">
      <section>
        <header>
          <strong>How old are these bytes?</strong>
          <b className="tnum">{bytes(data.total)}</b>
        </header>
        {data.buckets.map((b, i) => (
          <div className="age-row" key={data.bucketLabels[i]}>
            <span>{data.bucketLabels[i]}</span>
            <i>
              <b style={{ width: `${Math.max(2, (b / total) * 100)}%`, background: AGE_CSS[i] }} />
            </i>
            <strong className="tnum">{bytes(b)}</strong>
            <em className="tnum">{((b / total) * 100).toFixed(1)}%</em>
          </div>
        ))}
      </section>

      <section>
        <header>
          <strong>Bytes by last-modified month</strong>
          {busiest && (
            <b className="tnum">
              busiest: {MONTH_NAMES[busiest[1]]} {busiest[0]}
            </b>
          )}
        </header>
        <div className="heat-labels">
          <span />
          {MONTH_LETTERS.map((m, i) => (
            <span key={i}>{m}</span>
          ))}
        </div>
        {years.map((y) => (
          <div className="heat-row" key={y}>
            <span className="tnum">{y}</span>
            {Array.from({ length: 12 }, (_, mi) => {
              const idx = (y - heat.firstYear) * 12 + mi;
              const v = heat.bytes[idx] ?? 0;
              const frac = heat.max > 0 ? v / heat.max : 0;
              const isBusiest = busiest && busiest[0] === y && busiest[1] === mi;
              // Reference treatment: significant months carry their size
              // IN the cell (darker fill + border) so the heavy months read
              // without hovering.
              const labeled = frac >= 0.3 && v > 0;
              return (
                <i
                  key={mi}
                  className={`${isBusiest ? "busiest" : ""} ${labeled ? "labeled" : ""}`}
                  style={{ ["--heat" as string]: Math.max(0.08, frac).toFixed(2) }}
                  title={`${MONTH_NAMES[mi]} ${y} — ${bytes(v)}`}
                >
                  {labeled && <b className="tnum">{bytes(v)}</b>}
                </i>
              );
            })}
          </div>
        ))}
        <div className="heat-legend">
          <span>less</span>
          <span className="db-heat-scale">
            <i style={{ background: "color-mix(in oklab, var(--pastel-blue) 15%, var(--track))" }} />
            <i style={{ background: "color-mix(in oklab, var(--pastel-blue) 40%, var(--track))" }} />
            <i style={{ background: "color-mix(in oklab, var(--pastel-blue) 70%, var(--track))" }} />
            <i style={{ background: "var(--pastel-blue)" }} />
          </span>
          <span>more</span>
        </div>
      </section>

      <section>
        <div className="db-big-head">
          <div className="db-big-title">
            <strong>Big &amp; Untouched</strong>
            <b className="tnum">{data.big.length} items</b>
          </div>
          <button type="button" className="db-outline compact" onClick={() => void stageTop()} disabled={data.big.length === 0}>
            <PlusIcon size={13} /> Stage top {Math.min(25, data.big.length)}
          </button>
        </div>
        {data.big.length === 0 ? (
          <div className="db-substate">No files ≥ 40 MB untouched for over a year. Nothing to reclaim here.</div>
        ) : (
          data.big.slice(0, 50).map((row) => {
            const staged = queuedIds.has(row.id);
            return (
              <div
                role="button"
                tabIndex={0}
                className="db-big-row"
                key={row.id}
                onPointerEnter={(e) => props.onHover(row.id, e.clientX, e.clientY)}
                onPointerLeave={() => props.onHover(null, 0, 0)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  props.onContextMenu(row.id, e.clientX, e.clientY);
                }}
                onClick={() => props.onSelect(row.id)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    props.onSelect(row.id);
                  }
                }}
                aria-label={`${row.name}, ${bytes(row.logical)}, ${relativeAge(row.modified, now)}`}
              >
                <FileIcon size={15} />
                <span>
                  <strong>{row.name}</strong>
                  <small><TailPath path={row.path} /></small>
                </span>
                <button
                  type="button"
                  className={`db-stage-toggle ${staged ? "staged" : ""}`}
                  aria-label={staged ? `Unstage ${row.name}` : `Stage ${row.name}`}
                  title={staged ? "Staged — click to unstage" : "Stage for cleanup"}
                  disabled={row.protected}
                  onClick={(e) => {
                    e.stopPropagation();
                    if (staged) unstage(row.id);
                    else void stageBig(row);
                  }}
                >
                  {staged ? <CheckIcon size={13} /> : <PlusIcon size={13} />}
                </button>
                <em className="tnum" style={{ color: "var(--text-tertiary)", fontStyle: "normal", fontSize: 10 }}>
                  {relativeAge(row.modified, now)}
                </em>
                <b className="tnum">{bytes(row.logical)}</b>
              </div>
            );
          })
        )}
      </section>
    </div>
  );
}
