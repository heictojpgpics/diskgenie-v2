/**
 * License dialog (docs/LICENSING-ARCHITECTURE.md §7): the activation
 * entry + the Pro status card. Owner decisions, session 10 + v2:
 *  * Until the COMPLETE key is in, the buttons are "Later" and
 *    "Purchase Licence" (no Dodo checkout, no free tier) — the primary
 *    becomes "Activate" only for a complete 22-char key.
 *  * On activation: a brief success state ("activated — thank you"),
 *    then the dialog closes itself.
 *  * Pro view: Status Active chip, name, email, Active until, and the
 *    thank-you line (the simulated variant is marked for CI).
 *  * v2 (owner feedback): NO Deactivate button — a user never needs
 *    to deactivate ("Validate now" + "Done" only); moving a license
 *    between machines is support's slot-reset flow.
 *  * v2.1 (session 16): the appearance settings — the ThemePicker
 *    (five palettes incl. the three new ones) lives in BOTH the
 *    entry and the Pro status bodies. Theming is an app-level
 *    cosmetic, never license-gated (the Monitor page is free, and
 *    so is making the app feel like yours); the entry-state picker
 *    is also the preview surface that sells the premium feel.
 *
 * Motion (the app's settle-in contract — same family as the toast +
 * popover + preview overlay, never framer for these paths):
 *  * ENTER: scrim keyframe fade (140 ms) + panel keyframe fade-up
 *    (160 ms) — unchanged shared .db-dialog base.
 *  * EXIT: `data-closing` phase — the DOM lives one 130 ms
 *    transition longer than the logic (opacity + gentle scale-down),
 *    then the parent unmounts. No instant pop.
 *  * STATE SWAPS (key entry → success → status card): each body is a
 *    keyed child running `db-settle-in` (130 ms opacity) — the dialog
 *    never hard-cuts its content mid-conversation.
 *  * The key input auto-formats as you type (DB-XXXXX-…), matching
 *    the purchase email's exact grouping — the placeholder promise is
 *    the input's lived experience.
 */
import { useEffect, useRef, useState } from "react";
import { CheckIcon, KeyIcon, SparklesIcon } from "./Icon";
import { BrandMark } from "./BrandMark";
import { ThemePicker } from "./ThemePicker";
import { useLicenseStore } from "../state/license";
import { useFocusTrap } from "../lib/useFocusTrap";
import { formatKey, isCompleteKey, normalizeKey } from "../lib/licenseKey";
import { invoke } from "../lib/ipc";
import { track, EVENTS } from "../lib/analytics";
import { bundledAppVersion, getInstalledVersion } from "../lib/appVersion";

/** Exit fade duration — must match the CSS `data-closing` transition. */
const CLOSE_MS = 130;

function activeUntilLabel(expiresAt: number, tier: string): string {
  if (tier === "lifetime" || expiresAt === 0) return "Lifetime";
  return new Date(expiresAt * 1000).toLocaleDateString(undefined, {
    year: "numeric",
    month: "long",
    day: "numeric",
  });
}

/** Normalize + reformat user input to the display grouping, capping
 * at the 22-char key (typing/pasting beyond it is dropped). */
function reformat(raw: string): string {
  const normalized = normalizeKey(raw);
  if (normalized.length === 0) return "";
  return formatKey(normalized.slice(0, 22));
}

function Appearance({ version }: { version: string }) {
  return (
    <div className="db-license-appearance">
      <ThemePicker />
      <div className="db-license-version" aria-label={`Installed version ${version}`}>
        Installed version <span className="tnum">{version}</span>
      </div>
    </div>
  );
}

export function LicenseDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const status = useLicenseStore((s) => s.status);
  const activate = useLicenseStore((s) => s.activate);
  const validateNow = useLicenseStore((s) => s.validateNow);
  const busy = useLicenseStore((s) => s.busy);
  const error = useLicenseStore((s) => s.error);
  const activated = useLicenseStore((s) => s.activated);
  const consumeActivated = useLicenseStore((s) => s.consumeActivated);
  const [key, setKey] = useState("");
  const [version, setVersion] = useState(bundledAppVersion);
  const [closing, setClosing] = useState(false);
  const dialogRef = useRef<HTMLDivElement>(null);
  const closeTimer = useRef<number | null>(null);
  useFocusTrap(dialogRef, open && !closing);

  useEffect(() => {
    if (!open) return;
    void getInstalledVersion().then(setVersion);
  }, [open]);

  // One graceful close path: fade out (data-closing) → THEN unmount.
  // A reopened dialog remounts fresh (enter animations replay once —
  // the "never replay entrances" rule applies WITHIN one mount).
  const requestClose = () => {
    if (closing) return;
    setClosing(true);
    if (closeTimer.current !== null) window.clearTimeout(closeTimer.current);
    closeTimer.current = window.setTimeout(() => {
      closeTimer.current = null;
      consumeActivated();
      onClose();
      // Reset after unmount (a reopen starts clean).
      window.setTimeout(() => {
        setClosing(false);
        setKey("");
      }, 30);
    }, CLOSE_MS);
  };

  // Success → auto-close (premium: show the win, then leave quietly —
  // through the same fade path as every other close).
  useEffect(() => {
    if (!open || !activated || closing) return;
    const t = window.setTimeout(() => requestClose(), 1600);
    return () => window.clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- requestClose is stable-in-open
  }, [open, activated, closing]);

  useEffect(() => {
    if (!open) return;
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") requestClose();
    };
    window.addEventListener("keydown", esc);
    return () => {
      window.removeEventListener("keydown", esc);
      if (closeTimer.current !== null) window.clearTimeout(closeTimer.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- mount-scoped listeners
  }, [open]);

  // CI tour hook: a scripted key fill (captures the "Activate" button
  // state in the screenshots workflow).
  useEffect(() => {
    if (!open) return;
    const onKey = (e: Event) => {
      const detail = (e as CustomEvent<{ key?: string }>).detail;
      if (detail?.key) setKey(reformat(detail.key));
    };
    window.addEventListener("db-tour-license-key", onKey);
    return () => window.removeEventListener("db-tour-license-key", onKey);
  }, [open]);

  if (!open) return null;

  const posture = status?.posture ?? "unlicensed";
  const isPro = posture === "pro" || posture === "grace";
  const complete = isCompleteKey(key);
  const purchaseUrl = status?.purchaseUrl || "https://diskgenie.app/pricing";

  const openPurchase = () => {
    track(EVENTS.checkoutOpened, { source: "license-dialog" });
    void invoke("open_url", { url: purchaseUrl }).catch(() => undefined);
  };

  // The three dialog bodies, keyed so each swap crossfades.
  let bodyKey: string;
  let body: React.ReactNode;
  if (activated) {
    bodyKey = "success";
    body = (
      <div className="db-license-success">
        <div className="db-license-success-badge" aria-hidden="true">
          <CheckIcon size={26} />
        </div>
        <h3>
          <CheckIcon size={16} /> DiskGenie Pro activated
        </h3>
        <p>
          Thank you for purchasing DiskGenie. Every tool is unlocked on this
          device — enjoy the clean disk.
        </p>
      </div>
    );
  } else if (isPro) {
    bodyKey = "status";
    body = (
      <div className="db-license-status">
        <div className="db-license-status-head">
          <span className={`db-license-chip ${posture === "grace" ? "offline" : "active"}`}>
            <i className="db-dot" />
            {posture === "grace"
              ? `Offline · ${status?.graceDaysLeft ?? 0} days left`
              : "Status: Active"}
          </span>
          {status?.simulated && <span className="db-license-sim">CI SIM</span>}
        </div>
        <dl>
          <div>
            <dt>Name</dt>
            <dd>{status?.customerName || "—"}</dd>
          </div>
          <div>
            <dt>Email</dt>
            <dd>{status?.customerEmail || "—"}</dd>
          </div>
          <div>
            <dt>Plan</dt>
            <dd>{status?.tier === "yearly" ? "Yearly" : "Lifetime"}</dd>
          </div>
          <div>
            <dt>Active until</dt>
            <dd>{activeUntilLabel(status?.licenseExpiresAt ?? 0, status?.tier ?? "")}</dd>
          </div>
        </dl>
        {posture === "grace" && (
          <p className="db-license-note">
            Reconnecting validates automatically — everything keeps working offline for{" "}
            {status?.graceDaysLeft ?? 0} more day{(status?.graceDaysLeft ?? 0) === 1 ? "" : "s"}.
          </p>
        )}
        <p className="db-license-thanks">Thank you for purchasing DiskGenie.</p>
        <Appearance version={version} />
      </div>
    );
  } else if (posture === "degraded") {
    bodyKey = "degraded";
    body = (
      <div className="db-license-status">
        <div className="db-license-status-head">
          <span className="db-license-chip offline">
            <i className="db-dot" />
            Offline · license check needed
          </span>
        </div>
        <p className="db-license-note">
          DiskGenie couldn't reach the license server for over 14 days, so Pro
          features are paused. Reconnect to the internet and check now — your
          license and data are safe.
        </p>
        {error && (
          <div className="db-license-msg error" role="alert">
            {error}
          </div>
        )}
        <Appearance version={version} />
      </div>
    );
  } else {
    bodyKey = "entry";
    body = (
      <div className="db-license-body">
        <p className="db-license-lead">
          Paste the license key from your purchase email. One key covers one
          Windows PC and one Mac.
        </p>
        <label>
          LICENSE KEY
          {/* Auto-formatted display form (DB-XXXXX-XXXXX-XXXXX-XXXXX);
           * completeness drives the button swap; final normalization
           * happens Rust-side. */}
          <input
            value={key}
            onChange={(e) => setKey(reformat(e.target.value))}
            placeholder="DB-XXXXX-XXXXX-XXXXX-XXXXX"
            spellCheck={false}
            autoComplete="off"
            autoFocus
            aria-label="License key"
            data-complete={complete || undefined}
          />
        </label>
        {error && (
          <div className="db-license-msg error" role="alert">
            {error}
          </div>
        )}
        {complete && !error && (
          <div className="db-license-msg ok">Key looks complete. You’re ready to activate.</div>
        )}
        <div className="db-license-perks">
          <div>
            <SparklesIcon size={13} /> Full-disk map, cleanup, duplicates & snapshots
          </div>
          <div>
            <CheckIcon size={13} /> Live monitor, always free
          </div>
          <div>
            <CheckIcon size={13} /> 14-day offline grace
          </div>
        </div>
        <Appearance version={version} />
      </div>
    );
  }

  // The heading's brand moment: the live gradient mark (the same
  // var(--ink-grad) circle the topbar carries) — the dialog IS the
  // product's premium surface, so it leads with the mark, and the
  // theme picker one section down re-colors it live.
  const heading =
    activated ? null
    : isPro ? (
      <h3>
        <BrandMark size={20} /> DiskGenie Pro
      </h3>
    )
    : posture === "degraded" ? (
      <h3>
        <BrandMark size={20} /> License check needed
      </h3>
    )
    : (
      <h3>
        <BrandMark size={20} /> Activate DiskGenie
      </h3>
    );

  return (
    <div className="db-scrim" role="dialog" aria-modal="true" aria-label="License" data-closing={closing || undefined}>
      <div className="db-dialog db-license-dialog" ref={dialogRef} data-closing={closing || undefined}>
        {heading}
        <div className="db-license-body-swap" key={bodyKey}>
          {body}
        </div>
        <div className="db-dialog-actions">
          {bodyKey === "entry" && !complete && (
            <button type="button" className="db-outline auto" onClick={requestClose}>
              Later
            </button>
          )}
          {bodyKey === "entry" &&
            (complete ? (
              <button
                type="button"
                className="db-ink-button auto db-license-primary"
                disabled={busy}
                onClick={() => void activate(key.trim())}
              >
                <KeyIcon size={14} />
                {busy ? "Activating…" : "Activate"}
              </button>
            ) : (
              <button type="button" className="db-ink-button auto db-license-primary" onClick={openPurchase}>
                Purchase Licence
              </button>
            ))}
          {bodyKey === "degraded" && (
            <>
              <button type="button" className="db-outline auto" onClick={requestClose}>
                Later
              </button>
              <button
                type="button"
                className="db-ink-button auto db-license-primary"
                disabled={busy}
                onClick={() => void validateNow()}
              >
                {busy ? "Checking…" : "Reconnect now"}
              </button>
            </>
          )}
          {bodyKey === "status" && (
            <>
              <button
                type="button"
                className="db-outline auto"
                disabled={busy}
                onClick={() => void validateNow()}
              >
                {busy ? "Checking…" : "Validate now"}
              </button>
              <button type="button" className="db-ink-button auto db-license-primary" onClick={requestClose}>
                Done
              </button>
            </>
          )}
          {/* Success state: no actions — the dialog closes itself
           * (actions under a transient state read as an error affordance). */}
        </div>
      </div>
    </div>
  );
}
