/**
 * DiskGenie app shell (spec §3): 56px top bar (brand, tabs, window drag
 * region, Windows caption buttons / macOS traffic-light reserve) → body
 * (sidebar | main | inspector on Explore only), 1px dividers. Hosts the
 * 5 tabs, the inspector, the preview overlay, the cleanup queue popover,
 * and the license dialog. Also mounts the DISKGENIE_TOUR driver (dev
 * hook §15 — CI screenshot tours).
 */
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { MotionConfig } from "framer-motion";

import { TopBar } from "./shell/TopBar";
import { Sidebar } from "./sidebar";
import { ExploreView } from "./explore/ExploreView";
import { DuplicatesView } from "./tabs/DuplicatesView";
import { ApplicationsView } from "./tabs/ApplicationsView";
import { MonitorView } from "./tabs/MonitorView";
import { SnapshotsView } from "./tabs/SnapshotsView";
import { InspectorPanel } from "./inspector/InspectorPanel";
import { PreviewOverlay } from "./components/PreviewOverlay";
import { CleanupQueuePopover } from "./components/CleanupQueuePopover";
import { LicenseDialog } from "./components/LicenseDialog";
import { ActivationGate } from "./components/ActivationGate";
import { useViewStore } from "./state/view";
import { useScanStore } from "./state/scan";
import { useExploreStore } from "./state/explore";
import { useLicenseStore, attachLicenseEvents } from "./state/license";
import { bootstrapMonitor } from "./state/monitor";
import { bootstrapDupes } from "./state/dupes";
import { preloadApplications } from "./state/applications";
import { getBreadcrumb, getNodeDetails, openNode, type CrumbData } from "./viz/exploreIpc";
import { invoke } from "./lib/ipc";
import { pushRecent, recentTargetLabel } from "./sidebar/RecentSection";
import { TourDriver } from "./shell/TourDriver";
import { AppErrorBoundary } from "./shell/AppErrorBoundary";
import { CheckIcon, Trash2Icon, UacShieldIcon } from "./components/Icon";
import { listen } from "./lib/ipc";
import "./theme/tokens.css";
import "./styles/base.css";
import "./styles/shell.css";
import "./styles/sidebar.css";
import "./styles/explore.css";
import "./styles/viz.css";
import "./styles/inspector.css";
import "./styles/tabs.css";
import "./styles/overlays.css";

/** Tab-swap wrapper (session-5 "settle-in" design, CSS-driven): the
 * entering view fades 0→1 over the solid page background; the old view
 * unmounts INSTANTLY (conditional render — no exit tween, no
 * lingering layer). Two content layers are NEVER on screen at once,
 * so the "page in page" double exposure is impossible by construction
 * (the old veil kept the exiting view visible under a semi-transparent
 * sheet for 200 ms — mid-ramp you literally saw both pages blended,
 * plus a 5 px rise that read as zoom). The wrapper carries the solid
 * `background: var(--background)` so the fade lands on the page's own
 * color — no flash in dark mode.
 *
 * The fade is a pure CSS keyframe animation (`db-settle-in`, shell.css),
 * NOT framer: framer drove the same tween via WAAPI while the inline
 * style stayed `opacity: 0`, and its cleanup is asynchronous — for one
 * full painted frame after the ramp completed the finished animation
 * was already gone but the final inline style hadn't landed, so the
 * element fell back to its initial `0` (a blank background-colored
 * flash ~150 ms after EVERY switch — the residual blink). A CSS
 * animation reverts to the element's underlying value (1) in the same
 * style recalc the moment it ends — the gap cannot exist. It also runs
 * on the compositor: main-thread jank while mounting the new view
 * can never stutter it. */
function TabSwap({ children }: { children: ReactNode }) {
  return (
    <div className="db-tab-swap">
      {children}
    </div>
  );
}

function AppShell() {
  const tab = useViewStore((s) => s.tab);
  const inspectorVisible = useViewStore((s) => s.inspectorVisible);
  const nameFilter = useViewStore((s) => s.nameFilter);
  const setNameFilter = useViewStore((s) => s.setNameFilter);
  const generation = useScanStore((s) => s.generation);
  const status = useScanStore((s) => s.status);
  const licensePosture = useLicenseStore((s) => s.status?.posture ?? null);
  const licenseGate = useLicenseStore((s) => s.gate);
  const currentFolder = useExploreStore((s) => s.currentFolder);
  const openFolder = useExploreStore((s) => s.openFolder);
  const goBack = useExploreStore((s) => s.goBack);
  const canGoBack = useExploreStore((s) => s.folderStack.length > 0);

  const [crumbs, setCrumbs] = useState<CrumbData[]>([]);
  const [previewId, setPreviewId] = useState<number | null>(null);
  const [queueOpen, setQueueOpen] = useState(false);
  const [licenseOpen, setLicenseOpen] = useState(false);
  const [toast, setToast] = useState<string | null>(null);
  const [toastIcon, setToastIcon] = useState<"shield" | "trash" | "check">("shield");
  // The exit fade flag (session-7 settle-out): the toast DOM outlives
  // its text by one 140ms data-closing transition so it never pops
  // (see overlays.css .db-toast; the old framer exit tween flashed
  // the toast back to full opacity for a frame before unmounting,
  // and its WAAPI enter left the same one-frame blank ~300ms in).
  const [toastClosing, setToastClosing] = useState(false);

  useEffect(() => {
    attachLicenseEvents();
    void useLicenseStore.getState().load();
    useScanStore.getState().ensureListeners(); // scan-progress / scan-done / cleanup-committed (once)
    // Duplicates: ONE persistent progress listener at boot — the
    // scan lifecycle survives every tab switch (session-5 fix).
    bootstrapDupes();
    // Sampler warms at BOOT, not at first Monitor-tab entry: the 2s
    // cadence is app-lifetime, so the tab renders live data the moment
    // it is opened (no mount-then-wait). See state/monitor.ts.
    bootstrapMonitor();
    // Applications warm at BOOT too: one background enumeration fills
    // the Rust app-lifetime cache — the first tab visit is a cache hit
    // (no skeletons, no per-mount IPC round trip).
    preloadApplications();
  }, []);

  // ── The "zoom" fix ────────────────────────────────────────────
  // `.db-body` animates its grid tracks (the inspector toggle's 250 ms
  // spring — a beloved WITHIN-Explore interaction). But a TAB switch
  // also flips `has-inspector`, so the main column used to RESIZE under
  // the content swap: the new view (and its canvas) mounted into a
  // still-animating container — the "zoom in / zoom out" the user
  // saw on every page change. For the swap's duration we snap the
  // grid (`transition: none`): the new view mounts at its FINAL
  // geometry and the fade is the only motion on screen. The inspector
  // column's entrance no longer needs suppressing here — it is a
  // class-driven transition now (shell.css) which never fires on
  // mount. useLayoutEffect (NOT useEffect): the snap must reach the
  // DOM BEFORE the browser paints the tab change — an effect lands one
  // paint late and that first frame showed the grid track partially
  // open (a ~10 px column jitter).
  // v2: the LICENSE lock flip also changes `has-inspector` (the
  // inspector column unmounts while locked) — the same snap applies,
  // so a posture change never animates the grid under the swap.
  // Locked = unlicensed or degraded (docs §6 STRICT posture semantics)
  // — computed ONCE per render, above every consumer below.
  const licenseLocked = licensePosture !== null && licensePosture !== "pro" && licensePosture !== "grace";
  const bodyRef = useRef<HTMLDivElement>(null);
  const prevTab = useRef(tab);
  const prevLocked = useRef(licenseLocked);
  useLayoutEffect(() => {
    const lockFlipped = prevLocked.current !== licenseLocked;
    if (prevTab.current === tab && !lockFlipped) return;
    prevTab.current = tab;
    prevLocked.current = licenseLocked;
    const el = bodyRef.current;
    if (!el) return;
    el.classList.add("db-tab-snap");
    const t = window.setTimeout(() => el.classList.remove("db-tab-snap"), 280);
    return () => window.clearTimeout(t);
  }, [tab, licenseLocked]);

  // Toast bus: any surface can raise a transient toast via the
  // `db-toast` window event (detail: { text, icon? }). The elevation
  // decline listener below and the cleanup commit both use it.
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | null = null;
    let hideTimer: ReturnType<typeof setTimeout> | null = null;
    const showToast = (text: string, icon?: string) => {
      setToast(text);
      setToastClosing(false); // a replacement toast cancels any fade-out
      if (icon === "shield" || icon === "trash" || icon === "check") setToastIcon(icon);
      else setToastIcon("check");
      if (timer) clearTimeout(timer);
      if (hideTimer) clearTimeout(hideTimer);
      timer = setTimeout(() => {
        setToastClosing(true); // start the CSS exit fade...
        hideTimer = setTimeout(() => setToast(null), 170); // ...then unmount
      }, 5200);
    };
    const onToast = (e: Event) => {
      const detail = (e as CustomEvent<{ text: string; icon?: string }>).detail;
      if (detail?.text) showToast(detail.text, detail.icon);
    };
    window.addEventListener("db-toast", onToast);
    return () => {
      window.removeEventListener("db-toast", onToast);
      if (timer) clearTimeout(timer);
      if (hideTimer) clearTimeout(hideTimer);
    };
  }, []);

  // Elevation decline feedback: the Rust side emits `admin-restart-failed`
  // when the UAC prompt is declined or the elevated launch fails — surface
  // it as a transient toast so the click is never silently swallowed.
  useEffect(() => {
    let un: (() => void) | null = null;
    void listen<string>("admin-restart-failed", (reason) => {
      window.dispatchEvent(
        new CustomEvent("db-toast", { detail: { text: reason || "Elevation was declined, so the administrator restart failed.", icon: "shield" } }),
      );
    }).then((u) => {
      un = u;
    }).catch(() => undefined);
    return () => {
      un?.();
    };
  }, []);

  // License gate interception: a command refused by the Rust license
  // layer (marker prefix) opens the activation flow — the UI lock and
  // the command gate protect each other (docs §2 L6).
  useEffect(() => {
    if (!licenseGate) return;
    setLicenseOpen(true);
    useLicenseStore.getState().dismissGate();
  }, [licenseGate]);

  // First unlicensed boot: surface the activation flow once (Later
  // dismisses it; every action re-raises it through the gate).
  const licenseAutoShown = useRef(false);
  useEffect(() => {
    if (licensePosture === null || licenseAutoShown.current) return;
    licenseAutoShown.current = true;
    if (licensePosture === "unlicensed") setLicenseOpen(true);
  }, [licensePosture]);

  // Tour-driver overlay events (CI screenshot tours).
  useEffect(() => {
    const openLicense = () => setLicenseOpen(true);
    const openQueue = () => setQueueOpen(true);
    const closeOverlays = () => {
      setLicenseOpen(false);
      setQueueOpen(false);
    };
    const closeLicense = () => setLicenseOpen(false);
    window.addEventListener("db-open-license", openLicense);
    window.addEventListener("db-open-queue", openQueue);
    window.addEventListener("db-tour-step", closeOverlays);
    window.addEventListener("db-tour-close-license", closeLicense);
    return () => {
      window.removeEventListener("db-open-license", openLicense);
      window.removeEventListener("db-open-queue", openQueue);
      window.removeEventListener("db-tour-step", closeOverlays);
      window.removeEventListener("db-tour-close-license", closeLicense);
    };
  }, []);

  // Scan lifecycle housekeeping: remember recent + reset navigation.
  useEffect(() => {
    if (status !== "done") return;
    useExploreStore.getState().resetNavigation();
    // First completed scan reveals the inspector (details exist now);
    // an explicit user toggle always wins over this one-time nudge.
    const v = useViewStore.getState();
    if (!v.inspectorTouched) v.setInspectorVisible(true);
    // CI tour hook: the DISKGENIE_SCAN dev-hook target lands in Recents
    // here (user-started scans are pushed on the scanning transition
    // below — the dev hook bypasses the UI click). The RAW value
    // (e.g. "ThisPC" from an elevated --scan relaunch) is normalized
    // through the same single label point as every other entry — the
    // twin "This PC"/"ThisPC" rows died there (session 13).
    void (async () => {
      try {
        const hooks = await invoke<{ scan: string | null }>("get_dev_hooks").catch(() => null);
        if (hooks?.scan) pushRecent(recentTargetLabel(hooks.scan));
      } catch {
        /* ignore */
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status]);

  // Every user-started scan lands in Recents (normalized to ONE label
  // per target). Runs on the scanning transition so the entry exists
  // even if the scan is cancelled midway. Reads the store directly —
  // no stale-closure risk on the [status] dep.
  useEffect(() => {
    if (status !== "scanning") return;
    const t = useScanStore.getState().scanTarget;
    if (t && t.length > 0) {
      pushRecent(recentTargetLabel(t));
    }
  }, [status]);

  // Breadcrumb chain + THE view-location resolver (session 15): one
  // effect resolves "where is the view" (crumbs for the top bar, the
  // path + name into the explore store) on every navigation and
  // generation change. The store's viewPath is the single source of
  // truth every sidebar consumer derives from — before this, each
  // section re-resolved it ad-hoc and the Disk Storage card keyed on
  // [generation, status], neither of which changes on a C:↔D:
  // navigation INSIDE the standing tree (the owner's flip report: the
  // main view moved to D:, the sidebar kept C:'s card).
  useEffect(() => {
    if (status !== "done") {
      setCrumbs([]);
      useExploreStore.setState({ viewPath: "", viewName: null });
      return;
    }
    let disposed = false;
    void (async () => {
      const chain = await getBreadcrumb(generation, currentFolder).catch(() => null);
      if (!disposed && chain) setCrumbs(chain);
      const d = await getNodeDetails(generation, currentFolder).catch(() => null);
      if (!disposed && d) {
        useExploreStore.setState({ viewPath: d.path, viewName: d.name });
      }
    })();
    return () => {
      disposed = true;
    };
  }, [generation, currentFolder, status]);

  const openPreview = useCallback((id: number) => setPreviewId(id), []);

  return (
    <div className="db-app">
      {licensePosture === "degraded" && (
        <div className="db-degrade-banner" role="alert">
          License couldn’t be verified for over 14 days. Pro features are paused until you reconnect (License in the top bar).
        </div>
      )}
      <TopBar
        crumbs={crumbs}
        onNavigateCrumb={(id) => openFolder(id)}
        onFolderBack={goBack}
        canGoBack={canGoBack}
        query={nameFilter}
        onQueryChange={setNameFilter}
        onOpenQueue={() => {
          if (licenseLocked) setLicenseOpen(true);
          else setQueueOpen((o) => !o);
        }}
        queueOpen={queueOpen}
        onOpenLicense={() => setLicenseOpen(true)}
      />
      <div
        ref={bodyRef}
        className={`db-body ${inspectorVisible && tab === "explore" && !licenseLocked ? "has-inspector" : ""}`}
      >
        <div className="db-sidebar-col">
          <Sidebar />
        </div>
        <div className="db-main-col">
          {/* Tab settle-in swap (see TabSwap): old view unmounts
           * instantly, the new one fades in over the solid background
           * at its FINAL geometry (the grid snapped — see bodyRef).
           * v2: the key includes the LOCK state — a license flip
           * (activate/deactivate/lock-down) remounts the swap wrapper
           * so the Activation Gate/view crossfade runs the SAME
           * settle-in as a tab switch (v1 hard-cut the content). */}
          {/* The license gate (docs §7): while unlicensed/degraded every
           * tab except Monitor shows the Activation Gate — the views
           * themselves never mount (and their Rust commands refuse
           * independently anyway). */}
          <TabSwap key={`${tab}:${licenseLocked ? "locked" : "open"}`}>
            {licenseLocked && tab !== "monitor" ? (
              <ActivationGate tab={tab} />
            ) : (
              <>
            {tab === "explore" && <ExploreView onPreview={openPreview} />}
            {tab === "duplicates" && <DuplicatesView />}
            {tab === "applications" && <ApplicationsView />}
            {tab === "monitor" && <MonitorView />}
            {tab === "snapshots" && <SnapshotsView />}
              </>
            )}
          </TabSwap>
        </div>
        {/* Mounted whenever Explore is active (the track animates 0px ↔
         * --inspector-w; a conditional mount could only hard-snap) —
         * visibility is the .has-inspector class on .db-body. */}
        {tab === "explore" && !licenseLocked && (
          <div className="db-inspector-col">
            <InspectorPanel onPreview={openPreview} />
          </div>
        )}
      </div>

      {previewId != null && (
        <PreviewOverlay
          generation={generation}
          id={previewId}
          onClose={() => setPreviewId(null)}
          onOpenDefault={(id) => {
            // openNode toasts on failure — never a silent no-op.
            void openNode(generation, id);
            setPreviewId(null);
          }}
        />
      )}

      <CleanupQueuePopover open={queueOpen && !licenseLocked} onClose={() => setQueueOpen(false)} anchor="topbar" />
      <LicenseDialog open={licenseOpen} onClose={() => setLicenseOpen(false)} />
      {/* Toast (settle-in CSS motion — see overlays.css .db-toast): a
       * plain div with a keyframe entrance and a data-closing exit;
       * no AnimatePresence, no WAAPI cleanup gap. */}
      {toast && (
        <div className="db-toast" role="status" data-closing={toastClosing || undefined}>
          {toastIcon === "trash" ? <Trash2Icon size={15} /> : toastIcon === "shield" ? <UacShieldIcon size={15} /> : <CheckIcon size={15} />}
          {toast}
        </div>
      )}
      <TourDriver />
    </div>
  );
}


export default function App() {
  return (
    <AppErrorBoundary>
      {/* reducedMotion="user": the CSS kill switch only covers
       * stylesheet animations — every framer spring (tab pill, badge,
       * toast, popover) ran regardless of the OS preference. This makes
       * the JS motion system honor it too (springs become instant). */}
      <MotionConfig reducedMotion="user">
        <AppShell />
      </MotionConfig>
    </AppErrorBoundary>
  );
}
