/**
 * Top bar (spec §3, 56px): brand → 5 tab capsules with sliding ink pill
 * (framer-motion layoutId) → breadcrumb (back + last 4 ancestors) →
 * search capsule (Ctrl/⌘K, Esc clears) → Cleanup Queue button + badge →
 * license chip → theme toggle → inspector toggle → Windows caption
 * buttons. The whole bar doubles as the window drag region (Windows 11
 * app convention — one chrome row instead of a separate title bar).
 */
import { motion } from "framer-motion";
import { useEffect, useRef, useState } from "react";
import {
  AppWindowIcon, ChevronLeftIcon, ChevronRightIcon, Clock3Icon, CopyIcon, GaugeIcon,
  LayoutGridIcon, MoonIcon, PanelRightIcon, SearchIcon, SunIcon, Trash2Icon, XIcon,
} from "../components/Icon";
import { BrandMark } from "../components/BrandMark";
import { MOD_KEY, IS_MAC } from "../lib/platform";
import { useCleanupStore } from "../state/cleanup";
import { useLicenseStore } from "../state/license";
import { useViewStore, type TabId } from "../state/view";
import { useTheme } from "../theme/useTheme";
import { bytes as formatBytes } from "../lib/format";
import type { CrumbData } from "../viz/exploreIpc";
import { CaptionButtons, useWindowControls } from "./TitleBar";
import { SPRING_UI } from "../lib/motion";

const TABS: { id: TabId; label: string; Icon: typeof LayoutGridIcon }[] = [
  { id: "explore", label: "Explore", Icon: LayoutGridIcon },
  { id: "duplicates", label: "Duplicates", Icon: CopyIcon },
  { id: "applications", label: "Applications", Icon: AppWindowIcon },
  { id: "monitor", label: "Monitor", Icon: GaugeIcon },
  { id: "snapshots", label: "Snapshots", Icon: Clock3Icon },
];

export interface TopBarProps {
  crumbs: CrumbData[];
  canGoBack: boolean;
  onFolderBack: () => void;
  onNavigateCrumb: (id: number) => void;
  query: string;
  onQueryChange: (q: string) => void;
  onOpenQueue: () => void;
  queueOpen: boolean;
  onOpenLicense: () => void;
}

export function TopBar(props: TopBarProps) {
  const tab = useViewStore((s) => s.tab);
  const setTab = useViewStore((s) => s.setTab);
  // The license lock (docs §7): a dot marks the four gated tabs while
  // unlicensed/degraded; the pill/tour geometry is untouched.
  const licensePosture = useLicenseStore((s) => s.status?.posture ?? null);
  const locked = licensePosture !== null && licensePosture !== "pro" && licensePosture !== "grace";
  const inspectorVisible = useViewStore((s) => s.inspectorVisible);
  const toggleInspector = useViewStore((s) => s.toggleInspector);
  const itemCount = useCleanupStore((s) => s.items.length);
  const { isDark, toggle } = useTheme();
  const searchRef = useRef<HTMLInputElement>(null);
  const [searchFocused, setSearchFocused] = useState(false);
  const windowApi = useWindowControls();

  // Double-click on empty bar / brand toggles maximize (Windows caption
  // convention). Interactive controls opt out via the closest() guard.
  const onDoubleClick = (e: React.MouseEvent) => {
    if (!windowApi) return;
    const t = e.target as HTMLElement;
    if (t.closest("button, input, a, [role='toolbar'], nav")) return;
    void windowApi.toggleMaximize();
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
        // Don't steal focus from a modal surface (the license key input,
        // dialogs, menus) — the old unconditional focus grabbed typing
        // out of the license field mid-keystroke.
        if (document.querySelector("[role='dialog'][aria-modal='true'], .db-scrim, [role='menu']")) return;
        e.preventDefault();
        searchRef.current?.focus();
      }
      if (e.key === "Escape" && document.activeElement === searchRef.current) {
        props.onQueryChange("");
        searchRef.current?.blur();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [props]);

  // Breadcrumb: back chevron + ancestors. Deep chains truncate to the
  // last 3 with a leading ellipsis crumb that jumps to the scan root
  // (full path in its tooltip) — without it, an 8-deep drill shows
  // "Google > Chrome > …" with no hint that Users/AppData are above.
  const full = props.crumbs;
  const truncated = full.length > 4;
  const tail = truncated ? full.slice(-3) : full;
  const isExplore = tab === "explore";

  return (
    <header
      className="db-topbar"
      data-os={IS_MAC ? "macos" : "windows"}
      data-tauri-drag-region
      onDoubleClick={onDoubleClick}
      role="banner"
    >
      <div className="db-brand" data-tauri-drag-region>
        <BrandMark size={31} />
        <strong data-tauri-drag-region>DiskGenie</strong>
      </div>

      <nav className="db-tabcaps" aria-label="Application sections">
        {TABS.map(({ id, label, Icon }) => (
          <button
            key={id}
            type="button"
            data-active={tab === id}
            data-locked={locked && id !== "monitor" ? "" : undefined}
            onClick={() => setTab(id)}
            aria-current={tab === id ? "page" : undefined}
            /* Below 1500px the visible label span is display:none (icon-only
             * tab strip) — without an explicit name every tab announces as a
             * bare "button" to assistive tech. aria-label keeps the name;
             * title doubles as the icon-only hover tooltip. */
            aria-label={label}
            title={label}
          >
            {tab === id && <motion.span layoutId="db-tab-pill" className="db-tab-pill" transition={SPRING_UI} />}
            <Icon size={15} />
            <span className="db-tab-label">{label}</span>
          </button>
        ))}
      </nav>

      <div className="db-breadcrumb" hidden={!isExplore}>
        <button className="db-breadcrumb-back" disabled={!props.canGoBack} onClick={props.onFolderBack} aria-label="Go back" title="Go back">
          <ChevronLeftIcon size={16} />
        </button>
        {tail.length === 0 && <span className="db-bcrumb current">n/a</span>}
        {truncated && (
          <>
            <button
              type="button"
              className="db-bcrumb db-bcrumb-ellipsis"
              onClick={() => props.onNavigateCrumb(full[0].id)}
              title={full.map((c) => c.name).join(" › ")}
            >
              …
            </button>
            <span className="db-bcrumb-sep">
              <ChevronRightIcon size={12} />
            </span>
          </>
        )}
        {tail.map((c, i) => (
          <span key={c.id} style={{ display: "contents" }}>
            {i > 0 && (
              <span className="db-bcrumb-sep">
                <ChevronRightIcon size={12} />
              </span>
            )}
            <button
              type="button"
              className={`db-bcrumb ${i === tail.length - 1 ? "current" : ""}`}
              onClick={() => props.onNavigateCrumb(c.id)}
              title={formatBytes(c.size)}
            >
              {c.name}
            </button>
          </span>
        ))}
      </div>

      <label className={`db-search ${searchFocused ? "is-focused" : ""}`}>
        <SearchIcon size={15} />
        <input
          ref={searchRef}
          value={props.query}
          onChange={(e) => props.onQueryChange(e.target.value)}
          onFocus={() => setSearchFocused(true)}
          onBlur={() => setSearchFocused(false)}
          placeholder="Filter by name…"
          aria-label="Filter by name"
        />
        {props.query ? (
          <button type="button" className="db-search-clear" onClick={() => props.onQueryChange("")} aria-label="Clear search">
            <XIcon size={14} />
          </button>
        ) : (
          <kbd>{MOD_KEY} K</kbd>
        )}
      </label>

      <button
        type="button"
        className={`db-queue-button ${itemCount > 0 ? "has-items" : ""}`}
        onClick={props.onOpenQueue}
        title="Cleanup Queue"
        data-open={props.queueOpen}
        aria-haspopup="dialog"
        aria-expanded={props.queueOpen}
      >
        <Trash2Icon size={15} />
        <span className="db-queue-label">Cleanup</span>
        <QueueBadge count={itemCount} />
      </button>

      <LicenseChip onClick={props.onOpenLicense} />

      <button
        type="button"
        className="db-icon-button"
        onClick={toggle}
        aria-label={isDark ? "Use light theme" : "Use dark theme"}
        title={isDark ? "Use light theme" : "Use dark theme"}
      >
        {isDark ? <SunIcon size={16} /> : <MoonIcon size={16} />}
      </button>

      <button
        type="button"
        className="db-icon-button"
        onClick={toggleInspector}
        aria-label="Toggle inspector"
        title="Toggle inspector"
        data-active={inspectorVisible}
      >
        <PanelRightIcon size={16} />
      </button>

      <CaptionButtons />
    </header>
  );
}

/** Queue count badge (session-7 settle-in, CSS-only — same contract as
 * .db-pop / .db-toast): the number pops via the `db-badge-in` keyframe
 * (fill both) and shrinks out via a data-closing transition; the DOM
 * outlives `count` by one 120ms fade. The framer AnimatePresence
 * version carried the WAAPI cleanup gap (one blank frame after the
 * spring settled, one flash-back before the unmount). */
function QueueBadge({ count }: { count: number }) {
  const [mounted, setMounted] = useState(false);
  const [closing, setClosing] = useState(false);
  useEffect(() => {
    if (count > 0) {
      setMounted(true);
      setClosing(false);
      return;
    }
    if (!mounted) return;
    setClosing(true);
    const t = window.setTimeout(() => {
      setMounted(false);
      setClosing(false);
    }, 150);
    return () => window.clearTimeout(t);
  }, [count, mounted]);
  if (!mounted) return null;
  return <b data-closing={closing || undefined}>{count}</b>;
}

function LicenseChip({ onClick }: { onClick: () => void }) {
  const posture = useLicenseStore((s) => s.status?.posture ?? "unlicensed");
  const label =
    posture === "pro" ? "Pro" : posture === "grace" ? "Pro · offline" : posture === "degraded" ? "Reconnect" : "Activate";
  return (
    <button
      type="button"
      className={`db-license-chip ${posture === "pro" || posture === "grace" ? "pro" : ""} ${posture === "degraded" ? "degraded" : ""}`}
      onClick={onClick}
      title="License"
    >
      <i className="db-dot" />
      {label}
    </button>
  );
}
