/**
 * Sidebar §5 (spec §6.7): Quick Wins — total in the caption, rows with
 * icon / title / count / size. Click → navigate to the biggest match;
 * right-click → custom menu ("Add all N to Cleanup" disabled for
 * review-only rows, "Show in Explorer"). Categories come from the
 * already-built tree (no extra disk pass).
 */
import { useEffect, useRef, useState } from "react";
import {
  ArchiveIcon, AppWindowIcon, BoxIcon, BubblesIcon, CheckIcon, ChevronRightIcon,
  Clock3Icon, DownloadIcon, ExternalLinkIcon, FileImageIcon, FileCode2Icon,
  FileVideoIcon, FlameIcon, GlobeIcon, HammerIcon, HardDriveIcon, PackageOpenIcon,
  RefreshCwIcon, SmartphoneIcon, type AnyIcon, TONE_KEYS,
} from "../components/Icon";
import { SectionCaption } from "../components/buttons";
import { invoke } from "../lib/ipc";
import { bytes } from "../lib/format";
import { useMenuBehavior } from "../lib/useMenuBehavior";
import { useExploreStore } from "../state/explore";
import { useScanStore } from "../state/scan";
import { useViewStore } from "../state/view";
import { useCleanupStore } from "../state/cleanup";
import { EVENTS, track } from "../lib/analytics";

export interface QuickWinRow {
  id: string;
  title: string;
  icon: string;
  count: number;
  size: number;
  reviewOnly: boolean;
  extra: string | null;
  biggestMatch: number | null;
}

interface QuickWinItem {
  id: number;
  path: string;
  size: number;
}

const ICONS: Record<string, AnyIcon> = {
  archive: ArchiveIcon,
  refresh: RefreshCwIcon,
  image: FileImageIcon,
  box: BoxIcon,
  package: PackageOpenIcon,
  code: FileCode2Icon,
  app: AppWindowIcon,
  blocks: BubblesIcon,
  // Production engine icon tags (core quickwins.rs). Every one of these
  // was missing from this map — production rendered the generic BoxIcon
  // fallback for 8 of 10 categories because the mock used a different
  // vocabulary and the gap never surfaced in dev.
  download: DownloadIcon,
  temp: FlameIcon,
  browser: GlobeIcon,
  phone: SmartphoneIcon,
  hammer: HammerIcon,
  video: FileVideoIcon,
  server: HardDriveIcon,
  clock: Clock3Icon,
};

// One tone order app-wide (Icon.tsx is the source — this local copy
// had violet twice and dropped mint).
const TONES = TONE_KEYS;

export function QuickWinsSection() {
  const status = useScanStore((s) => s.status);
  const generation = useScanStore((s) => s.generation);
  const openFolder = useExploreStore((s) => s.openFolder);
  const select = useExploreStore((s) => s.select);
  const setTab = useViewStore((s) => s.setTab);
  const stageMany = useCleanupStore((s) => s.stageMany);
  const [rows, setRows] = useState<QuickWinRow[] | null>(null);
  const [menu, setMenu] = useState<{ row: QuickWinRow; x: number; y: number } | null>(null);

  useEffect(() => {
    if (status !== "done") {
      setRows(null);
      return;
    }
    let disposed = false;
    void (async () => {
      const res = await invoke<QuickWinRow[]>("quick_wins", { generation }).catch(() => null);
      if (!disposed) setRows(res);
    })();
    return () => {
      disposed = true;
    };
  }, [status, generation]);

  const menuRef = useRef<HTMLDivElement>(null);
  // Shared menu behavior (keyboard + outside-close + focus) — the old
  // local version had no arrow keys and closed on click+blur (survived
  // a drag that started inside and ended outside).
  useMenuBehavior(menuRef, menu != null, () => setMenu(null));

  if (status !== "done" || !rows || rows.length === 0) return null;
  const total = rows.reduce((a, r) => a + r.size, 0);

  const goto = (row: QuickWinRow) => {
    setTab("explore");
    track(EVENTS.quickWinsCategoryOpened, { category: row.id });
    if (row.biggestMatch != null && row.biggestMatch > 0) {
      openFolder(row.biggestMatch);
      select(row.biggestMatch);
    }
  };

  const addAll = async (row: QuickWinRow) => {
    if (row.reviewOnly) return;
    const items = await invoke<QuickWinItem[]>("quick_win_items", { generation, categoryId: row.id }).catch(() => null);
    if (items && items.length > 0) {
      stageMany(
        items.map((i) => ({
          id: i.id,
          path: i.path,
          size: i.size,
          reason: `${row.title} · Quick win`,
        })),
      );
      track(EVENTS.quickWinsAddAll, { category: row.id, items: items.length });
    }
    setMenu(null);
  };

  return (
    <>
      <SectionCaption right={total > 0 ? bytes(total) : undefined}>
        <span>Quick Wins</span>
      </SectionCaption>
      <div className="db-quick-list">
        {rows.map((row, i) => {
          const Icon = ICONS[row.icon] ?? BoxIcon;
          return (
            <button
              key={row.id}
              type="button"
              onClick={() => goto(row)}
              onContextMenu={(e) => {
                e.preventDefault();
                setMenu({ row, x: e.clientX, y: e.clientY });
              }}
              title={row.reviewOnly ? `${row.title} · review only` : row.title}
            >
              <span className={`db-quick-icon tone-${TONES[i % TONES.length]}`}>
                <Icon size={15} />
              </span>
              <span>
                <strong>{row.title}</strong>
                <small>
                  {row.count.toLocaleString()} {row.count === 1 ? "item" : "items"}
                  {row.reviewOnly ? " · review only" : ""}
                </small>
              </span>
              <b className="tnum">{bytes(row.size)}</b>
              <span className="db-quick-chevron">
                <ChevronRightIcon size={14} />
              </span>
            </button>
          );
        })}
      </div>

      {menu && (
        <div ref={menuRef} className="db-context" style={{ left: Math.min(menu.x, window.innerWidth - 230), top: Math.min(menu.y, window.innerHeight - 140) }} role="menu">
          <button
            type="button"
            role="menuitem"
            className="db-ctx-item"
            disabled={menu.row.reviewOnly}
            title={menu.row.reviewOnly ? (menu.row.extra ?? "Review-only row. Add all is disabled") : undefined}
            onClick={() => void addAll(menu.row)}
          >
            <CheckIcon size={14} />
            Add all {menu.row.count.toLocaleString()} to Cleanup
          </button>
          <div className="db-ctx-sep" />
          <button
            type="button"
            role="menuitem"
            className="db-ctx-item"
            onClick={() => {
              if (menu.row.biggestMatch != null && menu.row.biggestMatch > 0) {
                void invoke("reveal_in_explorer", { generation, id: menu.row.biggestMatch }).catch(() => undefined);
              }
              setMenu(null);
            }}
          >
            <ExternalLinkIcon size={14} />
            Show in Explorer
          </button>
        </div>
      )}
    </>
  );
}
