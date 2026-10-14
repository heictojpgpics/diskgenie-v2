/**
 * Hover chip (spec §7): bottom-left over any mode — icon, name, size,
 * % of scan, file-count pill. Driven by DIRECT DOM WRITES through an
 * imperative handle (hover NEVER re-renders React — spec §9 pitfall).
 */
import { createElement, forwardRef, useImperativeHandle, useRef } from "react";
import { createRoot, type Root } from "react-dom/client";
import { categoryIcon, FolderIcon } from "./Icon";

export interface HoverChipHandle {
  show: (data: HoverChipData, x: number, y: number) => void;
  move: (x: number, y: number) => void;
  hide: () => void;
}

export interface HoverChipData {
  name: string;
  size: number;
  shareOfScan: number;
  fileCount: number;
  isDir: boolean;
  category: string;
  categoryColor: number;
  isCloud: boolean;
  isProtected: boolean;
}

/** Shared placement: 16/18 px offset from the pointer, clamped so the
 * chip never leaves the viewport (both the first show and every move
 * — the old show() placed at the RAW pointer, undoing the clamp). */
function placeChip(el: HTMLElement, x: number, y: number): void {
  const W = 380;
  const left = Math.min(Math.max(12, x + 16), window.innerWidth - W - 12);
  const top = Math.min(window.innerHeight - 70, y + 18);
  el.style.left = `${left}px`;
  el.style.top = `${top}px`;
}

export const HoverChip = forwardRef<HoverChipHandle, { sizeFmt: (b: number) => string }>(function HoverChip(
  { sizeFmt },
  ref,
) {
  const root = useRef<HTMLDivElement>(null);
  const visible = useRef(false);
  // Persistent icon root: created ONCE on first show, re-rendered after.
  // The old code called createRoot() on every hover and never unmounted
  // — every hover leaked a detached React root (fiber state retained).
  const iconRoot = useRef<Root | null>(null);

  useImperativeHandle(ref, () => ({
    show(data, x, y) {
      const el = root.current;
      if (!el) return;
      visible.current = true;
      const Icon = data.isDir ? FolderIcon : categoryIcon(data.category);
      const icon = el.querySelector<HTMLElement>(".db-hc-icon");
      if (icon) {
        icon.style.background = `#${(data.categoryColor || 0xcbd5e1).toString(16).padStart(6, "0")}dd`;
        // Lazy-init the persistent root, then re-render the glyph into it.
        if (!iconRoot.current) {
          const host = document.createElement("span");
          iconRoot.current = createRoot(host);
          icon.replaceChildren(host);
        }
        iconRoot.current.render(createElement(Icon, { size: 15 }));
      }
      const strong = el.querySelector<HTMLElement>(".db-hc-body > strong");
      if (strong) strong.textContent = data.name;
      const small = el.querySelector<HTMLElement>(".db-hc-body > small");
      if (small) {
        small.textContent =
          `${sizeFmt(data.size)} · ${(data.shareOfScan * 100).toFixed(1)}% of scan` +
          (data.isProtected ? " · managed by Windows" : "") +
          (data.isCloud ? " · in the cloud" : "");
      }
      const pill = el.querySelector<HTMLElement>(".db-hc-pill");
      if (pill) {
        pill.textContent = data.isDir
          ? `${Math.round(data.fileCount).toLocaleString()} files`
          : data.category;
        pill.style.display = "";
      }
      el.style.opacity = "1";
      el.style.transform = "translateY(0)";
      placeChip(el, x, y);
    },
    move(x, y) {
      const el = root.current;
      if (!el || !visible.current) return;
      placeChip(el, x, y);
    },
    hide() {
      const el = root.current;
      if (!el) return;
      visible.current = false;
      el.style.opacity = "0";
      el.style.transform = "translateY(4px)";
    },
  }));

  return (
    <div ref={root} className="db-hover-chip" style={{ opacity: 0, pointerEvents: "none" }} aria-hidden>
      <span className="db-hc-icon" />
      <span className="db-hc-body">
        <strong>Name</strong>
        <small>n/a</small>
      </span>
      <span className="db-hc-pill">n/a</span>
    </div>
  );
});
