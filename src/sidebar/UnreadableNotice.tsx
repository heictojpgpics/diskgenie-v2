/**
 * Sidebar §7 (spec §6.9 + §7 unreadable notice): the access-denied
 * notice with sample-path tooltip, Restart-as-administrator (hidden
 * when already elevated), and dismiss ✕. On macOS the copy references
 * Full Disk Access (Mac BuildPrompt §6).
 */
import { useEffect, useState } from "react";
import { LockKeyholeIcon, SettingsIcon, UacShieldIcon, XIcon } from "../components/Icon";
import { invoke } from "../lib/ipc";
import { IS_MAC } from "../lib/platform";
import { useScanStore } from "../state/scan";

interface DeniedInfo {
  count: number;
  samples: string[];
}

export function UnreadableNotice() {
  const status = useScanStore((s) => s.status);
  const scanTarget = useScanStore((s) => s.scanTarget);
  const [info, setInfo] = useState<DeniedInfo | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const [elevated, setElevated] = useState<boolean | null>(null);

  useEffect(() => {
    if (status !== "done") {
      setInfo(null);
      setDismissed(false);
      return;
    }
    try {
      const raw = window.localStorage.getItem("diskgenie.last-denied");
      if (raw) {
        const parsed = JSON.parse(raw) as DeniedInfo;
        setInfo(parsed.count > 0 ? parsed : null);
      }
    } catch {
      /* ignore */
    }
    void invoke<boolean>("is_elevated").then(setElevated).catch(() => setElevated(null));
  }, [status]);

  if (status !== "done" || !info || dismissed) return null;

  const restartAdmin = () => {
    void invoke("restart_as_admin", { scanTarget, turbo: false }).catch(() => undefined);
  };

  return (
    <div className="db-notice" role="status">
      <LockKeyholeIcon size={17} />
      <div title={info.samples.join("\n")}>
        <strong>{info.count.toLocaleString()} folders couldn’t be read. Sizes may be incomplete.</strong>
        <span>
          {IS_MAC
            ? "macOS denied access. Granting Full Disk Access includes privacy-protected locations."
            : elevated === true
              ? "These folders are protected by the system."
              : "Some folders need administrator rights to read."}
        </span>
      </div>
      {IS_MAC ? (
        <button
          type="button"
          className="db-outline compact"
          onClick={() => void invoke("open_url", { url: "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles" }).catch(() => undefined)}
        >
          <SettingsIcon size={14} /> Open Privacy Settings
        </button>
      ) : elevated === false ? (
        <button type="button" className="db-outline compact" onClick={restartAdmin}>
          <UacShieldIcon size={14} /> Restart as administrator
        </button>
      ) : null}
      <button
        type="button"
        className="db-icon-button"
        aria-label="Dismiss notice"
        title="Dismiss notice"
        onClick={() => setDismissed(true)}
      >
        <XIcon size={14} />
      </button>
    </div>
  );
}
