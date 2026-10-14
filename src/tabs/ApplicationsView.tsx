/**
 * Applications tab (spec §11): installed apps sorted by total footprint,
 * rows expandable to "Program files" + leftover groups; uninstall
 * dialog (Run uninstaller / Stage leftovers only / Cancel); failure
 * alert with Restart-as-administrator.
 */
import { useEffect, useRef, useState } from "react";
import { AppWindowIcon, CheckIcon, PackageOpenIcon, RefreshCwIcon, Trash2Icon, UacShieldIcon } from "../components/Icon";
import { TailPath } from "../components/TailPath";
import { SCAN_THIS_PC } from "../lib/platform";
import { EmptyState, SkeletonRows, Spinner } from "../components/buttons";
import { useFocusTrap } from "../lib/useFocusTrap";
import { bytes } from "../lib/format";
import { invoke } from "../lib/ipc";
import { useApplicationsStore, type AppEntry, type UninstallResult } from "../state/applications";
import { useCleanupStore } from "../state/cleanup";
import { useScanStore } from "../state/scan";
import { EVENTS, track } from "../lib/analytics";

export function ApplicationsView() {
  const status = useScanStore((s) => s.status);
  const startScan = useScanStore((s) => s.startScan);
  const apps = useApplicationsStore((s) => s.apps);
  const busy = useApplicationsStore((s) => s.busy);
  const error = useApplicationsStore((s) => s.error);
  const load = useApplicationsStore((s) => s.load);
  const uninstall = useApplicationsStore((s) => s.uninstall);
  const stageMany = useCleanupStore((s) => s.stageMany);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [confirm, setConfirm] = useState<AppEntry | null>(null);
  const [uninstalling, setUninstalling] = useState(false);
  const [failPaths, setFailPaths] = useState<string[] | null>(null);
  const confirmRef = useRef<HTMLDivElement>(null);
  useFocusTrap(confirmRef, confirm !== null);

  // Esc closes the uninstall confirm — every other dialog (license,
  // preview, queue) closes on Esc; this one was the lone exception.
  // Guarded while RUNNING: the buttons are disabled mid-run, but Esc
  // would otherwise close the dialog out from under the live uninstall.
  useEffect(() => {
    if (!confirm) return;
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !uninstalling) setConfirm(null);
    };
    window.addEventListener("keydown", esc);
    return () => window.removeEventListener("keydown", esc);
  }, [confirm, uninstalling]);

  useEffect(() => {
    if (apps === null && !busy) {
      void load();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (status === "idle" || status === "scanning") {
    return (
      <div className="db-tab db-scroll">
        <EmptyState
          icon={<AppWindowIcon size={28} />}
          title="Applications"
          body={status === "scanning" ? "Scan in progress. App footprints land here after the tree is ready." : "Every installed app, its leftovers, and one-click cleanup."}
          action={
            status !== "scanning" ? (
              <button type="button" className="db-ink-button auto" onClick={() => void startScan("ThisPC")}>
                <RefreshCwIcon size={14} /> {SCAN_THIS_PC}
              </button>
            ) : undefined
          }
        />
      </div>
    );
  }

  const sorted = [...(apps ?? [])].sort((a, b) => b.total - a.total);
  const totalFootprint = sorted.reduce((a, x) => a + x.total, 0);

  const runUninstall = async (app: AppEntry) => {
    setUninstalling(true);
    track(EVENTS.uninstallRun, { app: app.id });
    try {
      const result: UninstallResult = await uninstall(app.id);
      if (result.remainingLeftovers.length > 0 || !result.removedEntry) {
        setFailPaths(result.remainingLeftovers.flatMap((g) => g.paths.map((p) => p.path)));
      } else {
        setConfirm(null);
        void load(true);
      }
    } catch (e) {
      setFailPaths([String(e)]);
    } finally {
      setUninstalling(false);
    }
  };

  const stageLeftovers = (app: AppEntry) => {
    const items = app.leftovers.flatMap((g) =>
      g.paths.map((p) => ({ id: 0, path: p.path, size: p.size, reason: `Leftovers: ${app.name} (${g.label})` })),
    );
    if (items.length > 0) stageMany(items);
    setConfirm(null);
  };

  return (
    <div className="db-tab db-scroll">
      <div className="db-tab-head">
        <div>
          <h1>Applications</h1>
          <span className="db-tab-sub">
            {busy ? (
              "Measuring bundles & leftovers…"
            ) : (
              <><b>{sorted.length.toLocaleString()}</b> installed · <b>{bytes(totalFootprint)}</b> total</>
            )}
          </span>
        </div>
        <div className="db-tab-head-actions">
          <button type="button" className="db-outline auto" disabled={busy} onClick={() => void load(true)}>
            <RefreshCwIcon size={14} /> Refresh
          </button>
        </div>
      </div>

      {error && (
        <div className="db-pop-failed" style={{ margin: "0 0 14px" }}>
          <strong>Couldn’t list applications</strong>
          <p style={{ margin: 0, fontSize: 11 }}>{error}</p>
        </div>
      )}

      {failPaths && (
        <div className="db-pop-failed" role="alert" style={{ margin: "0 0 14px" }}>
          <strong>Some locations need administrator rights</strong>
          <ul>
            {failPaths.slice(0, 6).map((p) => (
              <li key={p}>{p}</li>
            ))}
          </ul>
          <button
            type="button"
            className="db-outline compact danger" style={{ marginTop: 8 }}
            onClick={() => void invoke("restart_as_admin", { scanTarget: "ThisPC", turbo: false }).catch(() => undefined)}
          >
            <UacShieldIcon size={13} /> Restart as administrator
          </button>
        </div>
      )}

      {busy && !apps && (
        // Structure preview (loading system v2): app-row skeletons keep
        // the table's rhythm instead of collapsing to a spinner-in-a-void.
        // Empty-content only: during a REFRESH (apps already rendered),
        // stacking skeletons over real rows double-paints the tab.
        <>
          <SkeletonRows rows={7} className="db-tab-skeleton" />
          <div className="db-loading-block" role="status">
            <span>Listing registry + Store apps, measuring sizes…</span>
          </div>
        </>
      )}

      {sorted.map((app) => {
        const isOpen = expanded.has(app.id);
        const leftoversTotal = app.leftovers.reduce((a, g) => a + g.size, 0);
        return (
          <div key={app.id} className="db-app-row" style={{ display: "block" }}>
            <div style={{ display: "grid", gridTemplateColumns: "44px minmax(0,1fr) auto auto", alignItems: "center", gap: 14 }}>
              {app.icon ? (
                <img className="db-app-icon" src={app.icon} alt="" />
              ) : (
                <span className="db-app-icon-fallback">
                  <AppWindowIcon size={20} />
                </span>
              )}
              <div>
                <strong>{app.name}</strong>
                <div className="db-app-meta">
                  <em>{app.publisher || "Unknown"}</em>
                  <span>{app.version ? `v${app.version}` : "Unknown"}</span>
                  {app.leftovers.length > 0 && (
                    <span className="db-leftover-count">+{app.leftovers.length} leftovers</span>
                  )}
                </div>
              </div>
              <b className="tnum">{bytes(app.total)}</b>
              <button
                type="button"
                className="db-outline compact db-app-uninstall"
                onClick={() => setConfirm(app)}
              >
                Uninstall
              </button>
            </div>
            <button
              type="button"
              className="db-outline compact" style={{ marginTop: 9, padding: "0 10px" }}
              onClick={() => setExpanded((s) => (s.has(app.id) ? new Set([...s].filter((x) => x !== app.id)) : new Set([...s, app.id])))}
            >
              {isOpen ? "Hide" : "Show"} breakdown{leftoversTotal > 0 ? ` · ${bytes(leftoversTotal)} leftovers` : ""}
            </button>
            {isOpen && (
              <div className="db-app-detail">
                {app.installLocation && (
                  <div className="db-app-detail-row">
                    <span>Program files</span>
                    <div><TailPath path={app.installLocation} /></div>
                    <b className="tnum">{bytes(app.bundleSize)}</b>
                  </div>
                )}
                {app.packageFullName && (
                  <div className="db-app-detail-row">
                    <span>Store package</span>
                    <div><TailPath path={app.packageFullName} /></div>
                    <b className="tnum">{bytes(app.bundleSize)}</b>
                  </div>
                )}
                {app.leftovers.map((g) => (
                  <div className="db-app-detail-row" key={g.label}>
                    <span>{g.label}</span>
                    {/* One TailPath line per path — a joined "p1 · p2"
                     * string would blunt-clip with no ellipsis (the
                     * container has overflow:hidden; nowrap). */}
                    <div>
                      {g.paths.map((p) => (
                        <TailPath key={p.path} path={p.path} />
                      ))}
                    </div>
                    <b className="tnum">{bytes(g.size)}</b>
                  </div>
                ))}
              </div>
            )}
          </div>
        );
      })}

      {!busy && sorted.length === 0 && (
        <EmptyState icon={<PackageOpenIcon size={28} />} title="No applications found" body="Registry and Store enumeration found nothing. Try Refresh." />
      )}

      {confirm && (
        <div className="db-scrim" role="dialog" aria-modal="true" aria-labelledby="db-uninstall-title">
          <div className="db-dialog db-uninstall-dialog" ref={confirmRef}>
            <div className="db-uninstall-head">
              {confirm.icon ? (
                <img className="db-uninstall-icon" src={confirm.icon} alt="" />
              ) : (
                <span className="db-uninstall-icon db-uninstall-icon-fallback">
                  <AppWindowIcon size={22} />
                </span>
              )}
              <div className="db-uninstall-title">
                <h3 id="db-uninstall-title">Uninstall {confirm.name}?</h3>
                {/* Two truncating lines instead of one nowrap run: real
                 * registry publishers can be long, and the old single
                 * inline span let the "publisher · v… · Store package"
                 * run spill past the dialog edge (inline elements
                 * ignore text-overflow — the "text goes out of the
                 * popup" report). */}
                <span className="db-uninstall-meta">
                  <span className="db-uninstall-publisher">
                    {confirm.publisher || "Unknown publisher"}
                  </span>
                  <span className="db-uninstall-version">
                    {confirm.version ? `v${confirm.version}` : "Version unknown"}
                    {confirm.source === "msix" ? " · Store package" : ""}
                  </span>
                </span>
              </div>
            </div>

            <div className="db-uninstall-stats" aria-label="Footprint breakdown">
              <div className="db-uninstall-stat">
                <span>Program files</span>
                <b className="tnum">{bytes(confirm.bundleSize)}</b>
              </div>
              <div className="db-uninstall-stat">
                <span>Leftovers</span>
                <b className="tnum">{bytes(confirm.leftovers.reduce((a, g) => a + g.size, 0))}</b>
              </div>
              <div className="db-uninstall-stat db-uninstall-stat-total">
                <span>Total footprint</span>
                <b className="tnum">{bytes(confirm.total)}</b>
              </div>
            </div>

            <p>
              Runs the app’s own uninstaller. Registry and installer state stay intact; program
              files are never trashed directly. Leftover data can be staged for review afterwards.
            </p>

            <div className="db-dialog-actions db-uninstall-actions">
              <button
                type="button"
                className="db-outline auto"
                disabled={uninstalling}
                onClick={() => setConfirm(null)}
              >
                Cancel
              </button>
              {confirm.leftovers.length > 0 && (
                <button
                  type="button"
                  className="db-outline auto"
                  disabled={uninstalling}
                  onClick={() => stageLeftovers(confirm)}
                >
                  <Trash2Icon size={13} /> Stage leftovers only
                </button>
              )}
              <button
                type="button"
                className="db-ink-button auto"
                disabled={uninstalling}
                onClick={() => void runUninstall(confirm)}
              >
                {uninstalling ? (
                  <>
                    <Spinner size={15} weight={2.4} /> Running uninstaller…
                  </>
                ) : (
                  <>
                    <CheckIcon size={14} /> Run uninstaller
                  </>
                )}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
