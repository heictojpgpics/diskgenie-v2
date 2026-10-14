/**
 * Duplicates tab (spec §10) — the v2 page (session 19 redesign):
 * a decision tool, not a list.
 *
 * - STAT STRIP: wasted / groups / duplicate files over the FILTERED
 *   set (what the filters hide doesn't count toward the wins).
 * - KEEP RULES: newest / oldest / shallowest — one click applies a
 *   survivor to every visible group ("Apply to all"), because a user
 *   with 200 groups cannot click each one. Per-group "Keep this"
 *   still wins over the rule.
 * - LOCATION + AGE COLUMNS: every member row shows its folder and
 *   its last-write age (the evidence the keep rules use), plus
 *   reveal-in-Explorer.
 * - FILTERS + SORTS: focus on 1 MB+ / 100 MB+ wins; order by wasted,
 *   size, copies, or newest.
 * - The queue is the TRUTH for staged state: the row tags derive
 *   from the cleanup store, so unstaging from the popover reverts
 *   the row honestly.
 *
 * Session-5 contracts preserved untouched: the scan lifecycle lives
 * in `state/dupes` (tab switches never orphan the pipeline); the
 * busy row is an ISOLATED component subscribing to the progress slice
 * alone (9 Hz ticks repaint one row, not 500 cards); the bar follows
 * the engine's monotonic `overall` fraction. Anti-flicker: no new
 * mount animations (rows ride the `.db-tab` fade-up), keep-tag
 * transitions stay CSS-only.
 */
import { memo, useEffect, useMemo, useRef, useState } from "react";
import { CopyIcon, FileIcon, SearchIcon, CheckIcon, XIcon, LocateIcon } from "../components/Icon";
import { TailPath } from "../components/TailPath";
import { SCAN_THIS_PC } from "../lib/platform";
import { EmptyState } from "../components/buttons";
import { bytes, relativeAge } from "../lib/format";
import { invoke } from "../lib/ipc";
import { useScanStore } from "../state/scan";
import { pathIdentity, useCleanupStore, type QueueItem } from "../state/cleanup";
import { useDupesStore, type DupesProgress, type DupesResult } from "../state/dupes";
import {
  applyRule,
  filterGroups,
  groupKey,
  MIN_SIZE_FILTERS,
  pickKeep,
  sortGroups,
  totalsOf,
  type DupeSort,
  type KeepRule,
} from "../state/dupesRules";

const PHASE_LABEL: Record<DupesProgress["phase"], string> = {
  collect: "Collecting candidates",
  screen: "Fingerprinting candidates",
  verify: "Verifying byte-for-byte",
  done: "Done",
  cancelled: "Cancelled",
};

/** "1m 42s" / "about 3 min" — coarse, honest, never jumpy (rounded to
 * the widest bucket the value fits so ticks don't re-render new text). */
function etaText(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 5) return "";
  if (seconds < 90) return `about ${Math.max(5, Math.round(seconds / 10) * 10)} s left`;
  if (seconds < 5400) return `about ${Math.round(seconds / 60)} min left`;
  return `about ${Math.round(seconds / 3600)} h left`;
}

/**
 * The live busy row. Blink-free by construction:
 * - ONE stable structure (phase, counts, rate, ETA, Cancel are always
 *   mounted while busy — no span pops in/out at phase boundaries, the
 *   old "Collecting artifacts… / counts / Collecting artifacts…"
 *   strobe).
 * - The bar reads `overall` (monotonic, engine-weighted) — it can
 *   never snap backwards at a boundary; CSS paces the width so ticks
 *   read as motion, not jumps.
 * - The rate derives from the CUMULATIVE `bytesDoneAll` counter —
 *   phase resets no longer freeze it.
 */
const BusyRow = memo(function BusyRow({ onCancel }: { onCancel: () => void }) {
  const progress = useDupesStore((s) => s.progress);
  const cancelling = useDupesStore((s) => s.cancelling);
  const rate = useRef<{ at: number; bytes: number; v: number } | null>(null);

  let mbps = 0;
  let eta = "";
  if (progress) {
    const now = performance.now();
    const r = rate.current;
    if (r && now - r.at > 500 && progress.bytesDoneAll >= r.bytes) {
      const v = (progress.bytesDoneAll - r.bytes) / ((now - r.at) / 1000) / (1024 * 1024);
      if (v > 0) rate.current = { at: now, bytes: progress.bytesDoneAll, v };
    } else if (!r) {
      rate.current = { at: now, bytes: progress.bytesDoneAll, v: 0 };
    }
    mbps = rate.current?.v ?? 0;
    if (mbps > 0.5 && progress.bytesTotal > progress.bytesDone) {
      eta = etaText((progress.bytesTotal - progress.bytesDone) / (mbps * 1024 * 1024));
    }
  }

  const phase = progress?.phase ?? "collect";
  const pct = Math.round((progress?.overall ?? 0) * 1000) / 10;
  const counting = (progress?.filesTotal ?? 0) > 0 || (progress?.filesDoneAll ?? 0) > 0;

  return (
    <div className="db-loading-block db-dupes-busy" role="status">
      <div className="db-dupes-busy-line">
        <span className="db-dupes-phase" data-phase={phase} data-cancelling={cancelling || undefined}>
          <i className="db-dupes-phase-dot" aria-hidden="true" />
          {cancelling ? "Cancelling…" : PHASE_LABEL[phase]}
          {counting && progress && progress.filesTotal > 0 && (
            <span className="tnum db-dupes-counts">
              {progress.filesDone.toLocaleString()} / {progress.filesTotal.toLocaleString()} files
              {progress.bytesTotal > 0 && (
                <> · {bytes(progress.bytesDone)} / {bytes(progress.bytesTotal)}</>
              )}
            </span>
          )}
        </span>
        <span className="tnum db-dupes-rate">
          {mbps > 0.5 ? <>{mbps >= 100 ? mbps.toFixed(0) : mbps.toFixed(1)} MB/s{eta ? ` · ${eta}` : ""}</> : "\u00A0"}
        </span>
        <button type="button" className="db-outline compact auto" onClick={onCancel} disabled={cancelling}>
          <XIcon size={12} /> {cancelling ? "Stopping…" : "Cancel"}
        </button>
      </div>
      <div className="db-dupes-bar" aria-hidden="true" style={{ ["--pct" as string]: `${pct}%` }} />
    </div>
  );
});

const KEEP_RULES: readonly { id: KeepRule; label: string; hint: string }[] = [
  { id: "newest", label: "Newest", hint: "Keep the most recently written copy" },
  { id: "oldest", label: "Oldest", hint: "Keep the original (earliest) copy" },
  { id: "shallowest", label: "Top folder", hint: "Keep the copy closest to the drive root" },
];

const SORTS: readonly { id: DupeSort; label: string }[] = [
  { id: "wasted", label: "Most wasted" },
  { id: "size", label: "Biggest files" },
  { id: "count", label: "Most copies" },
  { id: "newest", label: "Newest first" },
];

export function DuplicatesView() {
  const status = useScanStore((s) => s.status);
  const generation = useScanStore((s) => s.generation);
  const startScan = useScanStore((s) => s.startScan);
  const stageMany = useCleanupStore((s) => s.stageMany);
  const unstage = useCleanupStore((s) => s.unstage);
  // The staged TRUTH: tags derive from the queue, so unstaging from
  // the popover reverts rows without this view knowing.
  const queueItems = useCleanupStore((s) => s.items);
  // The app-lifetime scan lifecycle (survives tab switches):
  const running = useDupesStore((s) => s.running);
  const result = useDupesStore((s) => s.result);
  const error = useDupesStore((s) => s.error);
  const scopePath = useDupesStore((s) => s.scopePath);
  const start = useDupesStore((s) => s.start);
  const cancel = useDupesStore((s) => s.cancel);
  const refresh = useDupesStore((s) => s.refresh);
  const invalidate = useDupesStore((s) => s.invalidate);

  const [keeps, setKeeps] = useState<Map<string, string>>(new Map());
  const [expanded, setExpanded] = useState<Set<number>>(new Set());
  const [minSize, setMinSize] = useState(0);
  const [sort, setSort] = useState<DupeSort>("wasted");
  const [rule, setRule] = useState<KeepRule>("newest");
  const [applied, setApplied] = useState(0);

  const stagedSet = useMemo(
    () => new Set(queueItems.map((i) => (i.path ? pathIdentity(i.path) : `i:${i.id}`))),
    [queueItems],
  );

  const now = useMemo(() => Date.now() / 1000, []);

  // Re-attach on mount: adopt a running pipeline (or the sticky last
  // result) from the backend's app-lifetime status. THE page-switch fix
  // — the remounted view continues the scan instead of offering a
  // fresh "Start scan" over a still-hashing pipeline.
  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Adopt freshly-delivered results into the expand state (the store
  // holds the result; only view sugar resets per mount).
  useEffect(() => {
    if (result) setExpanded(new Set(result.groups.slice(0, 3).map((g) => g.id)));
  }, [result]);

  // Tree-change invalidation: a new scan or a cleanup commit bumps the
  // generation while status stays "done" — the old result's groups are
  // stale (paths may no longer exist). The backend's start_scan also
  // cancels any in-flight dupes run; the store's `running` clears when
  // the pipeline folds.
  useEffect(() => {
    invalidate(generation);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status, generation]);

  // Tour hook (CI): run the scan when the tour reaches the duplicates
  // step so production screenshots show the real result state. The
  // store owns the invoke — the event handler is mount-safe either way.
  useEffect(() => {
    const run = () => start(useScanStore.getState().generation);
    window.addEventListener("db-tour-dupes-run", run);
    return () => window.removeEventListener("db-tour-dupes-run", run);
  }, [start]);

  // The visible slice: filters + sorts are PURE (dupesRules.ts).
  const visible = useMemo(
    () => sortGroups(filterGroups(result?.groups ?? [], minSize), sort),
    [result, minSize, sort],
  );
  const stats = useMemo(() => totalsOf(visible), [visible]);
  const maxWasted = useMemo(
    () => visible.reduce((m, g) => (g.wasted > m ? g.wasted : m), 0),
    [visible],
  );

  const stageRest = (g: DupesResult["groups"][number], keepPath: string) => {
    const rest = g.files.filter((f) => f.path !== keepPath);
    stageMany(
      rest.map((f) => ({ id: 0, path: f.path, size: g.size, reason: "Duplicate" }) satisfies QueueItem),
    );
  };

  const keepOne = (g: DupesResult["groups"][number], keepPath: string) => {
    setKeeps((m) => new Map(m).set(groupKey(g), keepPath));
    // If the new keep was staged (rule applied, then user overrides):
    // remove it from the queue — the survivor must never be staged.
    unstage(0, keepPath);
    stageRest(g, keepPath);
  };

  /** Apply the keep rule to every VISIBLE group: keeps are recorded,
   * the rest are staged (the queue dedupes by path identity). */
  const applyRuleToAll = () => {
    const next = applyRule(visible, rule);
    const toStage: QueueItem[] = [];
    for (const g of visible) {
      const keep = next.get(groupKey(g));
      if (!keep) continue;
      for (const f of g.files) {
        if (f.path === keep) continue;
        toStage.push({ id: 0, path: f.path, size: g.size, reason: "Duplicate" });
      }
    }
    setKeeps((m) => {
      const merged = new Map(m);
      for (const [k, v] of next) merged.set(k, v);
      return merged;
    });
    stageMany(toStage);
    setApplied(Date.now());
  };

  const scan = () => start(generation);

  const stale = result != null && result.generation !== generation;

  const subtitle = useMemo(() => {
    if (running) {
      return scopePath
        ? `Scanning for duplicates inside ${scopePath}. This tab updates live.`
        : "Scanning for duplicates. Keep using the app, this tab updates live.";
    }
    if (!result || stale) return "Byte-identical files, grouped for safe removal.";
    if (result.groups.length === 0) {
      return result.scopePath
        ? `No duplicates inside ${result.scopePath}.`
        : "No duplicates found.";
    }
    return (
      <>
        <b>{bytes(result.wastedTotal)}</b> could be reclaimed across <b>{result.groups.length.toLocaleString()}</b>{" "}
        {result.groups.length === 1 ? "group" : "groups"}
        {result.scopePath ? <> inside <b>{result.scopePath}</b></> : undefined} · {result.files.toLocaleString()} files considered
      </>
    );
  }, [running, result, stale, scopePath]);

  if (status !== "done") {
    return (
      <div className="db-tab db-scroll">
        <EmptyState
          icon={<CopyIcon size={28} />}
          title="Duplicates"
          body={status === "scanning" ? "Scan in progress. Duplicate detection starts once the tree is complete." : "Complete a disk scan to find duplicate files."}
          action={
            status !== "scanning" ? (
              <button type="button" className="db-ink-button auto" onClick={() => void startScan("ThisPC")}>
                <SearchIcon size={15} /> {SCAN_THIS_PC}
              </button>
            ) : undefined
          }
        />
      </div>
    );
  }

  const hasVisible = visible.length > 0;
  const filteredOut = (result?.groups.length ?? 0) - visible.length;

  return (
    <div className="db-tab db-scroll">
      <div className="db-tab-head">
        <div>
          <h1>Duplicates</h1>
          <span className="db-tab-sub">{subtitle}</span>
        </div>
        {(result || running) && (
          <div className="db-tab-head-actions">
            <button type="button" className="db-ink-button auto" disabled={running} onClick={scan}>
              <SearchIcon size={15} />
              {running ? "Scanning…" : "Scan Again"}
            </button>
          </div>
        )}
      </div>

      {error && (
        <div className="db-pop-failed" style={{ margin: "0 0 14px" }}>
          <strong>Couldn’t scan for duplicates</strong>
          <p style={{ margin: 0, fontSize: 11 }}>{error}</p>
        </div>
      )}

      {running && <BusyRow onCancel={cancel} />}

      {!result && !running && status === "done" && (
        <EmptyState
          icon={<CopyIcon size={28} />}
          title="Find duplicate files"
          body={
            scopePath
              ? `Byte-identical copies inside ${scopePath}. Keep one, stage the rest. Hit the button to scan the whole tree, or pick a folder in Explore and use “Duplicates here” to narrow it down.`
              : "One pass screens every file. Survivors get verified byte-for-byte so only true duplicates show up. Keep rules, filters, and one-click staging turn the list into reclaimed space. Pick a folder in Explore and use “Duplicates here” to scope the scan."
          }
          action={
            <button type="button" className="db-ink-button auto" onClick={scan}>
              <SearchIcon size={15} /> Scan for Duplicates
            </button>
          }
        />
      )}

      {result && result.groups.length === 0 && !running && (
        <EmptyState
          icon={<CheckIcon size={28} />}
          title="No duplicates"
          body={
            result.scopePath
              ? `Every file inside ${result.scopePath} is unique. Nothing to reclaim here.`
              : "Every file on this scan is unique. Nothing to reclaim."
          }
        />
      )}

      {result && result.groups.length > 0 && !stale && (
        <>
          {/* ── Stat strip: the filtered set's headline numbers ── */}
          <div className="db-dup-stats">
            <div className="db-dup-stat">
              <span className="db-dup-stat-label">Reclaimable</span>
              <span className="db-dup-stat-num tnum">{bytes(stats.wasted)}</span>
              <span className="db-dup-stat-sub">{filteredOut > 0 ? `${filteredOut} smaller group${filteredOut === 1 ? "" : "s"} hidden` : `across the whole scan`}</span>
            </div>
            <div className="db-dup-stat">
              <span className="db-dup-stat-label">Groups</span>
              <span className="db-dup-stat-num tnum">{stats.groups.toLocaleString()}</span>
              <span className="db-dup-stat-sub">byte-identical sets</span>
            </div>
            <div className="db-dup-stat">
              <span className="db-dup-stat-label">Duplicate files</span>
              <span className="db-dup-stat-num tnum">{stats.files.toLocaleString()}</span>
              <span className="db-dup-stat-sub">copies you can stage</span>
            </div>
          </div>

          {/* ── Toolbar: keep rules + filters ── */}
          <div className="db-dup-toolbar" role="toolbar" aria-label="Duplicates controls">
            <div className="db-dup-toolbar-group" role="group" aria-label="Keep rule">
              <span className="db-dup-toolbar-label">Keep</span>
              <div className="db-segmented">
                {KEEP_RULES.map((r) => (
                  <button
                    key={r.id}
                    type="button"
                    title={r.hint}
                    data-active={rule === r.id}
                    onClick={() => setRule(r.id)}
                  >
                    {r.label}
                  </button>
                ))}
              </div>
              <button type="button" className="db-outline compact auto" onClick={applyRuleToAll} disabled={running}>
                <CheckIcon size={12} /> Apply to all groups
              </button>
              {applied > 0 && (
                <span className="db-dup-applied" key={applied}>
                  Rule applied. Review the queue before deleting
                </span>
              )}
            </div>
            <div className="db-dup-toolbar-group" role="group" aria-label="Filter and sort">
              <div className="db-segmented" role="group" aria-label="Minimum size">
                {MIN_SIZE_FILTERS.map((f) => (
                  <button key={f.value} type="button" data-active={minSize === f.value} onClick={() => setMinSize(f.value)}>
                    {f.label}
                  </button>
                ))}
              </div>
              <div className="db-segmented" role="group" aria-label="Sort order">
                {SORTS.map((s) => (
                  <button key={s.id} type="button" data-active={sort === s.id} onClick={() => setSort(s.id)}>
                    {s.label}
                  </button>
                ))}
              </div>
            </div>
          </div>

          {!hasVisible && (
            <EmptyState
              icon={<SearchIcon size={28} />}
              title="No groups at this size"
              body={`Every group is smaller than ${bytes(minSize)}. Relax the size filter to see them.`}
            />
          )}

          {/* ── Group cards ── */}
          {visible.map((g) => {
            const isOpen = expanded.has(g.id) || visible.length <= 3;
            const kept = keeps.get(groupKey(g));
            const ruleKeep = kept ?? pickKeep(g.files, rule);
            const share = maxWasted > 0 ? Math.max(2, Math.round((g.wasted / maxWasted) * 100)) : 0;

            return (
              <div className="db-dup-group" key={groupKey(g)}>
                <header>
                  <div className="db-dup-group-line">
                    <strong>
                      {g.count.toLocaleString()} {g.count === 1 ? "copy" : "copies"} · {bytes(g.size)} each
                    </strong>
                    <span className="tnum db-dup-wasted">{bytes(g.wasted)} wasted</span>
                    <button
                      type="button"
                      className="db-outline compact auto"
                      onClick={() => setExpanded((s) => (s.has(g.id) ? new Set([...s].filter((x) => x !== g.id)) : new Set([...s, g.id])))}
                    >
                      {isOpen ? "Collapse" : "Show files"}
                    </button>
                  </div>
                  {/* Relative-wasted bar: the group's share of the
                   * biggest group's waste — the visual ranking cue
                   * (colors follow the theme's --used token). */}
                  <div className="db-dup-share" aria-hidden="true">
                    <i style={{ width: `${share}%` }} />
                  </div>
                </header>
                {isOpen &&
                  g.files.map((f) => {
                    const isKept = kept === f.path;
                    const isRuleKeep = !kept && f.path === ruleKeep;
                    const isStaged = stagedSet.has(pathIdentity(f.path));
                    return (
                      <div key={f.path} className="db-dup-file">
                        <FileIcon size={15} />
                        <div className="db-dup-file-main">
                          <TailPath path={f.path} />
                          <span className="db-dup-file-meta">
                            <span className="db-dup-folder" title={f.path}>{folderShort(f.path)}</span>
                            <span className="db-dup-age" aria-label={`Modified ${relativeAge(f.modified, now)}`}>
                              {relativeAge(f.modified, now)}
                            </span>
                          </span>
                        </div>
                        <button
                          type="button"
                          className="db-dup-locate"
                          title="Show in file manager"
                          aria-label={`Show ${f.path} in the file manager`}
                          onClick={() =>
                            void invoke("reveal_in_explorer", { generation, id: f.nodeId }).catch(() => undefined)
                          }
                        >
                          <LocateIcon size={13} />
                        </button>
                        {isKept || isRuleKeep ? (
                          <span className="db-keep-tag keep" title={isKept ? undefined : "Suggested by the keep rule. Click another copy to override"}>
                            <CheckIcon size={11} /> Keep
                          </span>
                        ) : isStaged ? (
                          <span className="db-keep-tag stage is-disabled">
                            Staged
                          </span>
                        ) : (
                          <button
                            type="button"
                            className="db-keep-tag stage"
                            onClick={() => keepOne(g, f.path)}
                            aria-label={`Keep ${f.path} and stage the other copies`}
                          >
                            <CheckIcon size={11} /> Keep this
                          </button>
                        )}
                        <b className="tnum">{bytes(g.size)}</b>
                      </div>
                    );
                  })}
                {isOpen && (
                  <div className="db-dup-guidance">
                    {kept
                      ? `${g.count - 1} ${g.count - 1 === 1 ? "copy is" : "copies are"} staged; the kept file will remain untouched.`
                      : kept === undefined && applied > 0
                        ? `Rule suggests keeping ${shortName(ruleKeep)}. “Keep this” on any copy overrides it.`
                        : `Choose the copy to keep. DiskGenie will stage the other ${g.count - 1} for review (${bytes(g.wasted)}).`}
                  </div>
                )}
              </div>
            );
          })}
        </>
      )}
    </div>
  );
}

/** The folder column: the member's parent, shortened from the LEFT
 * (tail-kept — the last folders matter more than the drive prefix). */
function folderShort(path: string): string {
  const i = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  if (i <= 0) return path;
  const parent = path.slice(0, i);
  const j = Math.max(parent.lastIndexOf("/"), parent.lastIndexOf("\\"));
  return j <= 0 ? parent : `…${parent.slice(j)}`;
}

function shortName(path: string): string {
  const i = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return i < 0 ? path : path.slice(i + 1);
}
