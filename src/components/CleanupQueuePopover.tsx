/**
 * Cleanup Queue popover (spec §9): 460×520 portal, opaque background,
 * click-outside/Esc; header (staged total, Delete permanently…, Move to
 * Recycle Bin…), rows with reason + ✕, tray empty state, per-mode
 * confirmation dialogs (session 13: the recycle dialog explains the
 * bin; the permanent dialog states the irreversibility plainly) and a
 * per-item failure alert.
 *
 * Motion (session-7): the enter/exit are CSS keyframe + transition —
 * NOT framer-motion. The old `motion.div` + `AnimatePresence` drove
 * the same ramp via WAAPI with the inline style parked at
 * `opacity: 0`, and framer's post-animation cleanup is asynchronous:
 * DOM-probed, one FULL painted frame ~250 ms after the spring
 * finished fell back to that inline 0 — the whole 460×520 popover
 * blinked OFF, then back ON (and on close it faded to 0, flashed
 * back to full opacity for a frame, then unmounted). Same class of
 * bug as the session-5 tab swap; same fix (see overlays.css
 * `db-pop-in` / `[data-closing]`).
 */
import { useEffect, useRef, useState } from "react";
import { CheckIcon, Trash2Icon, TriangleAlertIcon, XIcon } from "./Icon";
import { TailPath } from "./TailPath";
import { pathIdentity, useCleanupStore } from "../state/cleanup";
import { useScanStore } from "../state/scan";
import { useFocusTrap } from "../lib/useFocusTrap";
import { bytes } from "../lib/format";
import { BIN_NAME, IS_MAC } from "../lib/platform";
import { invoke } from "../lib/ipc";

/** Unmount delay: the CSS `[data-closing]` exit transition runs 140 ms
 * (overlays.css); the actual unmount waits it out + a small margin. */
const EXIT_UNMOUNT_MS = 170;

/** Which destructive action is pending confirmation. */
type ConfirmMode = "recycle" | "permanent";

export interface CommitFailure {
  path: string;
  reason: string;
}

export function CleanupQueuePopover({
  open,
  onClose,
  anchor,
}: {
  open: boolean;
  onClose: () => void;
  anchor: "topbar";
}) {
  const items = useCleanupStore((s) => s.items);
  const remove = useCleanupStore((s) => s.remove);
  const commitRecycle = useCleanupStore((s) => s.commitToRecycleBin);
  const commitPermanent = useCleanupStore((s) => s.commitToDeletePermanently);
  const scanStatus = useScanStore((s) => s.status);
  const [confirming, setConfirming] = useState<ConfirmMode | null>(null);
  const [committing, setCommitting] = useState(false);
  const confirmRef = useRef<HTMLDivElement>(null);
  useFocusTrap(confirmRef, confirming !== null);
  const [failure, setFailure] = useState<{ count: number; failed: CommitFailure[]; error: string | null } | null>(null);
  const popRef = useRef<HTMLDivElement>(null);
  // Anchor to the toolbar Cleanup button's live position instead of a
  // hardcoded top:96 — the degrade banner shifts the topbar down and a
  // fixed offset leaves the popover floating detached from its button.
  const [anchorPos, setAnchorPos] = useState<{ right: number; top: number } | null>(null);
  // The popover DOM outlives `open` by one 140 ms exit fade: `mounted`
  // is the real presence flag, `closing` drives the exit transition,
  // and anchorPos is kept (NOT nulled) while fading so the exiting
  // panel holds its anchored position instead of jumping.
  const [mounted, setMounted] = useState(false);
  const [closing, setClosing] = useState(false);
  const fadeFrame = useRef(0);
  const unmountTimer = useRef(0);
  useEffect(() => {
    if (open) {
      setMounted(true);
      setClosing(false);
      // Re-open during an interrupted close: drop the frozen bridge
      // values (see the close path below) so the panel settles back
      // to base styles — the base transition ramps it to full. The
      // inline animation:none STAYS (replaying the entrance keyframe
      // on a half-visible panel would flash; it clears on the next
      // fresh mount).
      popRef.current?.style.removeProperty("opacity");
      popRef.current?.style.removeProperty("transform");
      return;
    }
    if (!mounted) return;
    // open flipped false while on screen: fade, then unmount.
    // (A re-open during the fade cancels this — the run above resets
    // closing and the base transition ramps the panel back.)
    const startFade = () => {
      setClosing(true);
      unmountTimer.current = window.setTimeout(() => {
        setMounted(false);
        setClosing(false);
        setAnchorPos(null);
      }, EXIT_UNMOUNT_MS);
    };
    const el = popRef.current;
    if (el && el.getAnimations().length > 0) {
      // Interrupted close (entrance keyframe still running): Chrome
      // won't start a transition from a value change that lands in
      // the SAME style recalc as an animation CANCEL (DOM-probed: the
      // exit snapped). Freeze the mid-flight values inline FIRST —
      // so the cancel's own recalc cannot flash the panel to its
      // underlying full opacity — then cancel the entrance, and flip
      // data-closing two paints later: the transition interpolates
      // from the frozen value to the exit state.
      const cs = getComputedStyle(el);
      // Snapshot BEFORE any mutation: getComputedStyle returns a LIVE
      // declaration — reading after the animation cancel re-computes
      // to the underlying value (1) and the freeze would hold full
      // opacity instead of the mid-flight one.
      const op = cs.opacity;
      const tr = cs.transform;
      el.style.animation = "none";
      el.style.opacity = op;
      el.style.transform = tr;
      fadeFrame.current = requestAnimationFrame(() => {
        fadeFrame.current = requestAnimationFrame(() => {
          el.style.removeProperty("opacity");
          el.style.removeProperty("transform");
          startFade();
        });
      });
    } else {
      startFade();
    }
    return () => {
      if (fadeFrame.current) cancelAnimationFrame(fadeFrame.current);
      if (unmountTimer.current) window.clearTimeout(unmountTimer.current);
    };
  }, [open, mounted]);
  useEffect(() => {
    if (!open) return;
    const measure = () => {
      const btn = document.querySelector<HTMLButtonElement>(".db-queue-button");
      if (!btn) return;
      const r = btn.getBoundingClientRect();
      setAnchorPos({ right: Math.max(18, window.innerWidth - r.right), top: r.bottom + 10 });
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, [open]);

  // Keyboard entry (a11y): the popover is non-modal (outside click /
  // Esc dismiss) but must still be REACHABLE — Enter on the Cleanup
  // button moves focus into the panel so Tab walks its controls
  // instead of the content behind it, and focus returns to the button
  // on close.
  useEffect(() => {
    if (!open || !anchorPos) return;
    const t = window.setTimeout(() => {
      const first = popRef.current?.querySelector<HTMLElement>(
        "button:not([disabled])",
      );
      (first ?? popRef.current)?.focus();
    }, 30); // after the enter animation mounts the tree
    return () => window.clearTimeout(t);
  }, [open, anchorPos]);

  useEffect(() => {
    if (!open) {
      setConfirming(null);
      setFailure(null);
      return;
    }
    // Outside-close (capture phase). While the confirm dialog is open the
    // popover is effectively modal: the dialog renders as a sibling of the
    // popover (not inside popRef), so an unconditional containment check
    // would unmount the tree on the pointerdown that precedes every dialog
    // button click — Cancel and Commit could never fire. Suspend
    // outside-close while confirming; the dialog's own scrim/Esc handles
    // dismissal.
    const onDown = (e: PointerEvent) => {
      if (confirming !== null) return;
      if (popRef.current && !popRef.current.contains(e.target as Node)) onClose();
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (confirming !== null) setConfirming(null);
        else onClose();
      }
    };
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("keydown", esc);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("keydown", esc);
    };
  }, [open, onClose, confirming]);

  const total = items.reduce((a, i) => a + i.size, 0);
  // No free tier (owner decision, session 10): the popover only renders
  // for PRO/grace users (App gates the anchor; Rust gates the commit).
  // Committing needs a settled tree; during a rescan the generation
  // mismatches and the server would refuse. Disable with a clear
  // reason instead of surfacing a jargon error after the click.
  const scanRunning = scanStatus === "scanning";

  const doCommit = async () => {
    if (confirming === null) return;
    const mode = confirming;
    setCommitting(true);
    setFailure(null);
    const committingItems = items;
    try {
      const result = mode === "permanent" ? await commitPermanent() : await commitRecycle();
      const failed = result.failed;
      if (failed.length > 0) {
        setFailure({ count: failed.length, failed, error: null });
      } else {
        setConfirming(null);
        onClose();
        // Success feedback: the popover closing over an updated tree is
        // too quiet for a destructive-feeling action — confirm WHAT
        // moved and what happens to the space now.
        const moved = result.trashed.filter((t) => !t.alreadyGone).length;
        const freed = committingItems
          .filter((i) => result.trashed.some((t) => pathIdentity(t.path) === pathIdentity(i.path)))
          .reduce((a, i) => a + i.size, 0);
        window.dispatchEvent(
          new CustomEvent("db-toast", {
            detail:
              mode === "permanent"
                ? {
                    text: `Permanently deleted ${moved.toLocaleString()} item${moved === 1 ? "" : "s"} · ${bytes(freed)} freed.`,
                    icon: "trash",
                  }
                : {
                    text: `Moved ${moved.toLocaleString()} item${moved === 1 ? "" : "s"} · ${bytes(freed)} to the ${BIN_NAME}. Empty it to free the space.`,
                    icon: "trash",
                  },
          }),
        );
      }
    } catch (e) {
      setFailure({ count: 0, failed: [], error: String(e) });
    } finally {
      setCommitting(false);
    }
  };

  return (
    <>
      {mounted && anchorPos && (
          <div
            ref={popRef}
            className="db-pop"
            data-closing={closing ? "true" : undefined}
            style={anchor === "topbar" ? { right: anchorPos.right, top: anchorPos.top } : undefined}
            role="dialog"
            aria-modal="false"
            aria-label="Cleanup Queue"
            tabIndex={-1}
          >
            <div className="db-pop-head">
              <div>
                <h3>Cleanup Queue</h3>
                <span className="db-pop-total tnum">{items.length > 0 ? `${bytes(total)} staged` : "Nothing staged"}</span>
              </div>
              <button type="button" className="db-pop-close" onClick={onClose} aria-label="Close">
                <XIcon size={15} />
              </button>
            </div>
            <div className="db-pop-actions">
              <button
                type="button"
                className="db-btn-permanent"
                disabled={items.length === 0 || committing || scanRunning}
                title={
                  scanRunning
                    ? "Wait for the scan to finish. Cleaning needs a settled map"
                    : "Delete immediately. Nothing goes to the Recycle Bin"
                }
                onClick={() => setConfirming("permanent")}
              >
                <TriangleAlertIcon size={14} />
                Delete permanently…
              </button>
              <button
                type="button"
                className="db-btn-commit"
                disabled={items.length === 0 || committing || scanRunning}
                title={
                  scanRunning
                    ? "Wait for the scan to finish. Cleaning needs a settled map"
                    : undefined
                }
                onClick={() => setConfirming("recycle")}
              >
                <Trash2Icon size={14} />
                {committing ? "Working…" : `Move to ${BIN_NAME}…`}
              </button>
            </div>
            {failure && (
              <div className="db-pop-failed" role="alert">
                <strong>{failure.error ? "Commit failed" : `Couldn’t recycle ${failure.count} item(s)`}</strong>
                {failure.error ? (
                  <p style={{ margin: 0, fontSize: 11 }}>{failure.error}</p>
                ) : (
                  <ul>
                    {failure.failed.slice(0, 8).map((f) => (
                      <li key={f.path} title={f.path}>
                        {f.reason} · {f.path}
                      </li>
                    ))}
                  </ul>
                )}
                <button
                  type="button"
                  className="db-outline compact" style={{ marginTop: 8 }}
                  onClick={() => setFailure(null)}
                >
                  Dismiss
                </button>
              </div>
            )}
            {items.length === 0 ? (
              <div className="db-pop-empty">
                <span className="db-pop-empty-icon">
                  <Trash2Icon size={24} />
                </span>
                <p>Nothing staged yet. Pick folders or files you want gone, then commit them in one move.</p>
              </div>
            ) : (
              <div className="db-pop-list db-scroll">
                {items.map((i) => (
                  <div key={`${i.id}:${i.path}`} className="db-pop-row">
                    <Trash2Icon size={14} />
                    <div className="db-pop-item">
                      <TailPath path={i.path} className="db-pop-path" />
                      <small>{i.reason}</small>
                    </div>
                    <b className="tnum">{bytes(i.size)}</b>
                    <button
                      type="button"
                      className="db-pop-remove"
                      aria-label={`Remove ${i.path}`}
                      onClick={() => remove(i.id, i.path)}
                    >
                      <XIcon size={13} />
                    </button>
                  </div>
                ))}
              </div>
            )}
          </div>
        )}

      {open && confirming !== null && (
        <div className="db-scrim" role="dialog" aria-modal="true">
          <div className="db-dialog" ref={confirmRef}>
            {confirming === "recycle" ? (
              <>
                <h3>Move {items.length.toLocaleString()} item{items.length === 1 ? "" : "s"} to the {BIN_NAME}?</h3>
                <p>
                  {items.length.toLocaleString()} item{items.length === 1 ? "" : "s"} · {bytes(total)} of data.{" "}
                  {IS_MAC
                    ? `Items go to the Trash. Space is only freed when you empty it.`
                    : `Items go to the Recycle Bin. Space is only freed when you empty it.`}
                </p>
                <button
                  type="button"
                  className="db-dialog-link"
                  onClick={() => void invoke("open_recycle_bin").catch(() => undefined)}
                >
                  <CheckIcon size={12} /> Open {BIN_NAME}
                </button>
                <div className="db-dialog-actions">
                  <button type="button" className="db-outline auto" onClick={() => setConfirming(null)}>
                    Cancel
                  </button>
                  <button
                    type="button"
                    className="db-ink-button auto danger"
                    disabled={committing}
                    onClick={() => void doCommit()}
                  >
                    <Trash2Icon size={14} />
                    {committing ? "Moving…" : `Move to ${BIN_NAME}`}
                  </button>
                </div>
              </>
            ) : (
              <>
                <h3 className="db-dialog-danger-title">
                  <TriangleAlertIcon size={16} />
                  Permanently delete {items.length.toLocaleString()} item{items.length === 1 ? "" : "s"}?
                </h3>
                <p>
                  {items.length.toLocaleString()} item{items.length === 1 ? "" : "s"} · {bytes(total)} of data.{" "}
                  {IS_MAC
                    ? "These items are deleted immediately. Nothing goes to the Trash, and this can't be undone."
                    : "These items are deleted immediately. Nothing goes to the Recycle Bin, and this can't be undone."}
                </p>
                <div className="db-dialog-actions">
                  <button type="button" className="db-outline auto" onClick={() => setConfirming(null)}>
                    Cancel
                  </button>
                  <button
                    type="button"
                    className="db-ink-button auto danger"
                    disabled={committing}
                    onClick={() => void doCommit()}
                  >
                    <TriangleAlertIcon size={14} />
                    {committing ? "Deleting…" : "Delete permanently"}
                  </button>
                </div>
              </>
            )}
          </div>
        </div>
      )}
    </>
  );
}
