/**
 * The in-tab Activation Gate (docs/LICENSING-ARCHITECTURE.md §7):
 * rendered instead of Explore / Duplicates / Applications / Snapshots
 * while unlicensed or degraded. The Monitor tab stays live (owner
 * decision). Every locked surface funnels to the same two actions —
 * Activate (opens the dialog) and Purchase Licence.
 *
 * v2 (owner feedback): the actions row uses PROPORTIONAL buttons —
 * v1 inherited `.db-ink-button`'s sidebar contract (`width: 100%`),
 * so both buttons claimed full width and the row stacked ("Purchase"
 * over "Licence"). The gate now scopes its own button sizing: equal
 * heights, auto widths, centered, wrapping as a pair at narrow
 * widths — and the card itself carries the settle-in fade so a
 * license-state flip never hard-cuts the view.
 */
import { CheckIcon, LockKeyholeIcon } from "./Icon";
import { BrandMark } from "./BrandMark";
import { useLicenseStore } from "../state/license";
import { invoke } from "../lib/ipc";
import { track, EVENTS } from "../lib/analytics";
import type { TabId } from "../state/view";

const PITCH: Record<TabId, { title: string; lines: string[] }> = {
  explore: {
    title: "Explore every byte of your disk",
    lines: [
      "Full-disk map in 9 visual modes",
      "Quick Wins: the biggest safe space savings",
      "Inspector, previews, and one-click cleanup",
    ],
  },
  duplicates: {
    title: "Find and clear duplicate files",
    lines: [
      "Content-hash scan (bit-identical only)",
      "Hardlink-aware, never a false positive",
      "One-click review and cleanup",
    ],
  },
  applications: {
    title: "Uninstall apps completely",
    lines: [
      "Every installed app, Store and classic",
      "Leftover detection after uninstall",
      "Safe removal with your confirmation",
    ],
  },
  monitor: { title: "", lines: [] },
  snapshots: {
    title: "Track what changed over time",
    lines: [
      "Point-in-time disk snapshots",
      "Diff two snapshots: what grew, what shrank",
      "Keep your cleanup honest",
    ],
  },
};

export function ActivationGate({ tab }: { tab: TabId }) {
  const status = useLicenseStore((s) => s.status);
  const purchaseUrl = status?.purchaseUrl || "https://diskgenie.app/pricing";
  const posture = status?.posture ?? "unlicensed";
  const pitch = PITCH[tab] ?? PITCH.explore;

  const openPurchase = () => {
    track(EVENTS.checkoutOpened, { source: "activation-gate" });
    void invoke("open_url", { url: purchaseUrl }).catch(() => undefined);
  };

  const openDialog = () => {
    window.dispatchEvent(new CustomEvent("db-open-license"));
  };

  return (
    <div className="db-activation-gate" role="note" aria-label="Activation required">
      <div className="db-activation-gate-card">
        {posture === "degraded" ? (
          <div className="db-activation-gate-icon" aria-hidden="true">
            <LockKeyholeIcon size={22} />
          </div>
        ) : (
          /* The product mark at gate scale — the photoreal disk+broom
           * over the live theme gradient (the same var(--ink-grad)
           * circle as the topbar). The product IS the pitch here. */
          <BrandMark size={44} />
        )}
        <h2>
          {posture === "degraded" ? "Pro features are paused" : pitch.title}
        </h2>
        {posture === "degraded" ? (
          <p>
            DiskGenie couldn't verify your license for over 14 days. Reconnect
            and validate from the License panel to pick up exactly where you
            left off.
          </p>
        ) : (
          <>
            <ul>
              {pitch.lines.map((line) => (
                <li key={line}>
                  <CheckIcon size={13} />
                  <span>{line}</span>
                </li>
              ))}
            </ul>
            <p className="db-activation-gate-sub">
              One license covers one Windows PC and one Mac.
            </p>
          </>
        )}
        <div className="db-activation-gate-actions">
          <button type="button" className="db-activation-gate-primary" onClick={openDialog}>
            <LockKeyholeIcon size={14} />
            {posture === "degraded" ? "Reactivate" : "Activate DiskGenie"}
          </button>
          {posture !== "degraded" && (
            <button type="button" className="db-activation-gate-secondary" onClick={openPurchase}>
              Purchase License
            </button>
          )}
        </div>
        <p className="db-activation-gate-monitor">
          The <strong>Monitor</strong> tab stays live, always.
        </p>
      </div>
    </div>
  );
}
