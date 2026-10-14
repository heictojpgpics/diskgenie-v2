/**
 * List mode (spec §7.9): virtualized expandable outline — icon, name,
 * mini share-of-parent bar, %, items, size; disclosure triangles only
 * for non-empty folders; 500 children per level cap.
 */
import { useEffect, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { ChevronRightIcon, FolderIcon, LockKeyholeIcon, CloudIcon } from "../../components/Icon";
import { categoryIcon } from "../../components/Icon";
import { getListChildren, type ListRowData } from "../../viz/exploreIpc";
import { bytes } from "../../lib/format";
import { Spinner, SkeletonRows } from "../../components/buttons";
import { useArrowNav } from "../../lib/useArrowNav";

interface FlatRow extends ListRowData {
  level: number;
  expanded: boolean;
  hasKids: boolean;
}

const EMPTY_ROWS: FlatRow[] = [];

export interface ListModeProps {
  generation: number;
  folder: number;
  filter: string;
  selectedId: number | null;
  onSelect: (id: number | null) => void;
  onOpen: (id: number) => void;
  onContextMenu: (id: number, x: number, y: number) => void;
  onHover: (id: number | null, x: number, y: number) => void;
}

export function ListMode(props: ListModeProps) {
  const [tree, setTree] = useState<FlatRow[] | null>(null);
  const expanded = useRef(new Set<number>());
  const scrollRef = useRef<HTMLDivElement>(null);

  const rebuild = async () => {
    // Guard against concurrent walks: rapid key changes (folder hops)
    // and quick expand/collapse clicks used to run overlapping async
    // walks where the LAST-RESOLVED tree won over the last-requested —
    // stale rows could render under a newer folder. Track the newest
    // request and drop every other walk's result.
    rebuildSeq.current += 1;
    const seq = rebuildSeq.current;
    try {
      const children = await getListChildren(props.generation, props.folder, props.filter);
      let out: FlatRow[] = [];
      const walk = async (parent: number, level: number) => {
        const kids = parent === props.folder ? children : await getListChildren(props.generation, parent, props.filter).catch(() => []);
        for (const k of kids.slice(0, 500)) {
          const hasKids = k.isDir && k.hasChildren;
          const isExpanded = expanded.current.has(k.id);
          out.push({ ...k, level, expanded: isExpanded, hasKids });
          if (k.isDir && isExpanded) await walk(k.id, level + 1);
        }
      };
      await walk(props.folder, 0);
      if (seq !== rebuildSeq.current) return; // superseded
      setTree(out);
    } catch {
      if (seq !== rebuildSeq.current) return; // superseded
      setTree([]);
    }
  };
  const rebuildSeq = useRef(0);

  const key = `${props.generation}:${props.folder}:${props.filter}`;
  useEffect(() => {
    expanded.current.clear();
    void rebuild();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  const virtualizer = useVirtualizer({
    count: tree?.length ?? 0,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 38,
    overscan: 12,
  });

  const totalItems = tree?.length ?? 0;
  const rows = tree ?? EMPTY_ROWS;

  // Explorer-parity keyboard navigation: ↑/↓ move, →/← expand and
  // collapse, Enter opens the selected folder.
  useArrowNav({
    count: totalItems,
    selectedId: props.selectedId,
    idOf: (i) => rows[i].id,
    onMove: (i) => {
      props.onSelect(rows[i].id);
      virtualizer.scrollToIndex(i, { align: "auto" });
    },
    onActivate: (i) => {
      if (rows[i].isDir) props.onOpen(rows[i].id);
    },
    onExpand: (i, open) => {
      const row = rows[i];
      if (!row.hasKids || row.expanded === open) return;
      if (open) expanded.current.add(row.id);
      else expanded.current.delete(row.id);
      void rebuild();
    },
  });

  if (!tree) {
    // Structure preview: the header + row skeletons reserve the exact
    // layout (38px rows) so data landing causes ZERO reflow — the old
    // spinner-in-a-void collapsed the column then re-inflated it.
    return (
      <div className="db-list" style={{ display: "flex", flexDirection: "column", height: "100%" }}>
        <div className="list-head">
          <span aria-hidden="true" />
          <span aria-hidden="true" />
          <span>Name</span>
          <span>Share</span>
          <span className="is-num">%</span>
          <span className="is-num">Items</span>
          <span className="is-num">Size</span>
        </div>
        <div ref={scrollRef} className="db-scroll" style={{ flex: 1, overflow: "auto" }}>
          <SkeletonRows rows={12} />
          <div className="db-loading-block" role="status">
            <Spinner size={18} />
            <span>Building outline…</span>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="db-list" style={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <div className="list-head">
        <span aria-hidden="true" />
        <span aria-hidden="true" />
        <span>Name</span>
        <span>Share</span>
        <span className="is-num">%</span>
        <span className="is-num">Items</span>
        <span className="is-num">Size</span>
      </div>
      <div ref={scrollRef} className="db-scroll" style={{ flex: 1, overflow: "auto" }}>
        <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
          {virtualizer.getVirtualItems().map((vi) => {
            const row = tree[vi.index];
            const Icon = row.isDir ? FolderIcon : categoryIcon(row.category);
            return (
              <button
                key={`${row.id}:${vi.index}`}
                type="button"
                className={`db-list-row ${props.selectedId === row.id ? "is-selected" : ""}`}
                style={{
                  position: "absolute",
                  top: 0,
                  left: 0,
                  width: "100%",
                  transform: `translateY(${vi.start}px)`,
                  paddingLeft: 8 + row.level * 16,
                }}
                onClick={() => props.onSelect(row.id)}
                onDoubleClick={() => {
                  if (row.isDir) props.onOpen(row.id);
                }}
                onContextMenu={(e) => {
                  e.preventDefault();
                  props.onContextMenu(row.id, e.clientX, e.clientY);
                }}
                onPointerEnter={(e) => props.onHover(row.id, e.clientX, e.clientY)}
                onPointerLeave={() => props.onHover(null, 0, 0)}
              >
                {row.hasKids ? (
                  <span
                    className="db-disclose"
                    data-open={row.expanded}
                    role="button"
                    tabIndex={-1}
                    onClick={(e) => {
                      e.stopPropagation();
                      if (row.expanded) expanded.current.delete(row.id);
                      else expanded.current.add(row.id);
                      void rebuild();
                    }}
                  >
                    <ChevronRightIcon size={13} />
                  </span>
                ) : (
                  <span />
                )}
                <Icon size={15} className="db-row-glyph" />
                <strong>
                  {row.name}
                  <span className="db-row-flags">
                    {row.protected && <LockKeyholeIcon size={10} />}
                    {row.cloud && <CloudIcon size={10} />}
                  </span>
                </strong>
                <i>
                  <b style={{ width: `${Math.max(2, Math.min(100, row.share * 100))}%`, background: `#${row.color.toString(16).padStart(6, "0")}` }} />
                </i>
                <em className="tnum">{(row.share * 100).toFixed(1)}%</em>
                <span className="db-list-items tnum">{row.isDir ? row.items.toLocaleString() : "n/a"}</span>
                <b className="tnum">{bytes(row.size)}</b>
              </button>
            );
          })}
        </div>
      </div>
      {totalItems === 0 && (
        <div className="db-substate">{props.filter ? `Nothing matches “${props.filter}”.` : "This folder is empty."}</div>
      )}
    </div>
  );
}
