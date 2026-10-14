/**
 * Snapshots tab (spec §13): Take Snapshot Now (disabled without a scan
 * or while saving), rows with path / "size · date" / Before & After
 * toggles / delete; diff = top-200 |change| with +red / −green and
 * different-roots notice. Every failure surfaces as a banner (the
 * production-readiness sweep: silent failure — the button re-enabling
 * with nothing happening — is indistinguishable from data loss here).
 */
import { useEffect, useState } from "react";
import { Clock3Icon, CameraIcon, Trash2Icon } from "../components/Icon";
import { TailPath } from "../components/TailPath";
import { SCAN_THIS_PC } from "../lib/platform";
import { EmptyState, SkeletonRows } from "../components/buttons";
import { invoke } from "../lib/ipc";
import { bytes } from "../lib/format";
import { userFacingError } from "../lib/userFacingError";
import { useScanStore } from "../state/scan";
import { useExploreStore } from "../state/explore";

interface SnapshotView {
  id: string;
  root: string;
  takenAt: number;
  total: number;
  folders: number;
}

interface DiffView {
  sameRoot: boolean;
  totalBefore: number;
  totalAfter: number;
  changes: { path: string; before: number; after: number; delta: number }[];
}

function fmtDate(unix: number): string {
  return new Date(unix * 1000).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

export function SnapshotsView() {
  const status = useScanStore((s) => s.status);
  const startScan = useScanStore((s) => s.startScan);
  const generation = useScanStore((s) => s.generation);
  const currentFolder = useExploreStore((s) => s.currentFolder);
  const [list, setList] = useState<SnapshotView[] | null>(null);
  const [saving, setSaving] = useState(false);
  const [before, setBefore] = useState<string | null>(null);
  const [after, setAfter] = useState<string | null>(null);
  const [diff, setDiff] = useState<DiffView | null>(null);
  const [diffing, setDiffing] = useState(false);
  // Surfaced failures: take / list-refresh / delete (banner at the top)
  // and diff (inline in the Changes section, where the user is looking).
  const [snapErr, setSnapErr] = useState<string | null>(null);
  const [diffErr, setDiffErr] = useState<string | null>(null);
  // Two-step delete: the first click arms "Delete?" on the row (danger),
  // the second confirms. Snapshots are irreversible user data — every
  // other destructive action in the app confirms first; this did not.
  const [armDel, setArmDel] = useState<string | null>(null);

  const refresh = async () => {
    try {
      setList(await invoke<SnapshotView[]>("list_snapshots"));
      setSnapErr(null);
    } catch (e) {
      // A failed refresh keeps the prior list (wiping it would claim the
      // user's snapshots are GONE); on the very first load the skeleton
      // resolves to the honest failed state, not an empty one.
      setList((prev) => prev ?? []);
      setSnapErr(userFacingError(e));
    }
  };

  useEffect(() => {
    void refresh();
  }, []);

  const take = async () => {
    setSaving(true);
    setSnapErr(null);
    try {
      await invoke("take_snapshot", { generation, node: currentFolder });
      await refresh();
    } catch (e) {
      setSnapErr(userFacingError(e));
    } finally {
      setSaving(false);
    }
  };

  const runDiff = async (b: string, a: string) => {
    setBefore(b);
    setAfter(a);
    setDiffing(true);
    setDiff(null);
    setDiffErr(null);
    try {
      setDiff(await invoke<DiffView>("diff_snapshots", { beforeId: b, afterId: a }));
    } catch (e) {
      setDiff(null);
      setDiffErr(userFacingError(e));
    } finally {
      setDiffing(false);
    }
  };

  const del = async (id: string) => {
    setArmDel(null);
    try {
      await invoke("delete_snapshot", { id });
    } catch (e) {
      setSnapErr(userFacingError(e));
    }
    if (before === id) setBefore(null);
    if (after === id) setAfter(null);
    void refresh();
  };

  if (status === "idle" || status === "scanning") {
    return (
      <div className="db-tab db-scroll">
        <EmptyState
          icon={<Clock3Icon size={28} />}
          title="Snapshots"
          body={status === "scanning" ? "Scan in progress. Snapshots capture the tree once it’s done." : "Before/after diffs of any two scans of the same root."}
          action={
            status !== "scanning" ? (
              <button type="button" className="db-ink-button auto" onClick={() => void startScan("ThisPC")}>
                <CameraIcon size={14} /> {SCAN_THIS_PC}
              </button>
            ) : undefined
          }
        />
      </div>
    );
  }

  return (
    <div className="db-tab db-scroll">
      <div className="db-tab-head">
        <div>
          <h1>Snapshots</h1>
          <span className="db-tab-sub">{list === null ? "Loading…" : list.length > 0 ? `${list.length} saved · pick Before + After to diff` : ""}</span>
        </div>
        {(list ?? []).length > 0 && (
          <div className="db-tab-head-actions">
            <button
              type="button"
              className="db-ink-button auto"
              disabled={saving || status !== "done"}
              onClick={() => void take()}
            >
              <CameraIcon size={15} />
              {saving ? "Saving…" : "Take Snapshot Now"}
            </button>
          </div>
        )}
      </div>

      {snapErr && (
        <div className="db-pop-failed" style={{ margin: "0 0 14px" }}>
          <strong>Snapshot operation failed</strong>
          <p style={{ margin: 0, fontSize: 11 }}>{snapErr}</p>
        </div>
      )}

      {list === null ? (
        // Initial list load (loading system v2): snapshot-row skeletons —
        // the old view showed only “…” in the sub and no body at all.
        <SkeletonRows rows={4} className="db-tab-skeleton" />
      ) : list.length === 0 ? (
        <EmptyState
          icon={<CameraIcon size={28} />}
          title="No snapshots yet"
          body="Take a snapshot now, run your cleanup, then take another. The diff shows exactly what changed."
          action={
            <button
              type="button"
              className="db-ink-button"
              disabled={saving || status !== "done"}
              onClick={() => void take()}
            >
              <CameraIcon size={15} />
              {saving ? "Saving…" : "Take Snapshot Now"}
            </button>
          }
        />
      ) : (
        <>
          {list.map((s) => (
            <div className="db-snap-row" key={s.id}>
              <Clock3Icon size={15} />
              <div>
                <strong>{s.root}</strong>
                <small>
                  <b>{bytes(s.total)}</b> · {fmtDate(s.takenAt)} · {s.folders.toLocaleString()} folders
                </small>
              </div>
              <div className="db-snap-toggle" role="group" aria-label="Before or after">
                <button type="button" data-active={before === s.id} onClick={() => before === s.id ? setBefore(null) : before && after && before !== s.id ? void runDiff(s.id, after) : setBefore(s.id)}>
                  Before
                </button>
                <button type="button" data-active={after === s.id} onClick={() => after === s.id ? setAfter(null) : before && after !== s.id ? void runDiff(before, s.id) : setAfter(s.id)}>
                  After
                </button>
              </div>
              {before && after && (
                <button type="button" className="db-outline compact auto" onClick={() => void runDiff(before, after)} disabled={diffing}>
                  {diffing ? "Diffing…" : "Compare"}
                </button>
              )}
              <button
                type="button"
                className={`db-icon-button ${armDel === s.id ? "db-del-armed" : ""}`}
                style={armDel === s.id ? { width: "auto", height: 30, padding: "0 10px", fontSize: 11 } : { width: 30, height: 30 }}
                aria-label={armDel === s.id ? `Confirm delete snapshot ${s.id}` : `Delete snapshot ${s.id}`}
                title={armDel === s.id ? "Click again to delete permanently" : "Delete snapshot"}
                onClick={() => (armDel === s.id ? void del(s.id) : setArmDel(s.id))}
                onBlur={() => armDel === s.id && setArmDel(null)}
              >
                {armDel === s.id ? "Delete?" : <Trash2Icon size={13} />}
              </button>
            </div>
          ))}
        </>
      )}

      {before && after && (
        <div>
          <div className="db-tab-head db-changes-head">
            <div>
              <h1 className="db-changes-title">Changes</h1>
              <span className="db-tab-sub">
                {diff ? (
                  <>
                    {bytes(diff.totalBefore)} → <b>{bytes(diff.totalAfter)}</b> · {diff.changes.length} changed folders
                  </>
                ) : (
                  "…"
                )}
              </span>
            </div>
          </div>
          {diff && !diff.sameRoot && (
            <div className="db-pop-failed" style={{ margin: "0 0 10px" }}>
              <strong>Different roots</strong>
              <p style={{ margin: 0, fontSize: 11 }}>These snapshots were taken at different roots, so the diff is not meaningful.</p>
            </div>
          )}
          {diffErr && !diffing && (
            <div className="db-pop-failed" style={{ margin: "0 0 10px" }}>
              <strong>Couldn’t compare these snapshots</strong>
              <p style={{ margin: 0, fontSize: 11 }}>{diffErr}</p>
            </div>
          )}
          {diffing && (
            <div className="db-loading-block" role="status">
              <span>Loading both snapshots…</span>
            </div>
          )}
          {(() => {
            const d = diff;
            if (!d) return null;
            const maxAbs = Math.max(1, ...d.changes.map((x) => Math.abs(x.delta)));
            return d.changes.map((c) => {
            const pct = Math.max(2, (Math.abs(c.delta) / maxAbs) * 100);
            return (
              <div className="db-diff-row" key={c.path} title={c.path}>
                <span className="db-diff-path"><TailPath path={c.path} /></span>
                <em className="tnum">{bytes(c.before)}</em>
                {c.delta > 0 ? (
                  <b className="grew tnum">+{bytes(c.delta)}</b>
                ) : (
                  <b className="shrank tnum">−{bytes(Math.abs(c.delta))}</b>
                )}
                <span
                  className="db-diff-bar"
                  aria-hidden
                >
                  <i
                    style={{
                      width: `${pct}%`,
                      background: c.delta > 0 ? "var(--used)" : "var(--free)",
                    }}
                  />
                </span>
              </div>
              );
            });
          })()}
          {diff && diff.changes.length === 0 && <div className="db-substate">No folder changed between these snapshots.</div>}
        </div>
      )}
    </div>
  );
}
