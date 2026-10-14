/**
 * Sidebar §4 (spec §6.6): current view — folder name + scan duration,
 * full path in monospace (middle-truncated), Reveal + Copy Path
 * (Copied ✓ transient). Shows a live progress strip while scanning.
 *
 * Session 15: the name/path come from the explore store's view
 * location (the ONE resolver in the App shell) — this section used to
 * re-fetch get_breadcrumb + node_details for the same node on every
 * navigation, a second ad-hoc resolution of the same fact.
 */
import { CopyIcon, EyeIcon } from "../components/Icon";
import { OutlineButton, SectionCaption, Spinner } from "../components/buttons";
import { TailPath } from "../components/TailPath";
import { invoke } from "../lib/ipc";
import { bytes, duration } from "../lib/format";
import { REVEAL_NAME } from "../lib/platform";
import { useExploreStore } from "../state/explore";
import { useScanStore } from "../state/scan";

export function CurrentViewSection() {
  const status = useScanStore((s) => s.status);
  const progress = useScanStore((s) => s.progress);
  const generation = useScanStore((s) => s.generation);
  const scanDurationMs = useScanStore((s) => s.scanDurationMs);
  const currentFolder = useExploreStore((s) => s.currentFolder);
  const viewName = useExploreStore((s) => s.viewName);
  const viewPath = useExploreStore((s) => s.viewPath);
  const show = status === "done" && viewName !== null;

  const reveal = () => {
    void invoke("reveal_in_explorer", { generation, id: currentFolder }).catch(() => undefined);
  };

  const copy = () => {
    void invoke("copy_path", { generation, id: currentFolder }).catch(() => undefined);
    // Browser-dev fallback: put the path on the clipboard ourselves.
    if (viewPath) void navigator.clipboard?.writeText(viewPath).catch(() => undefined);
  };

  return (
    <>
      <SectionCaption right={show && scanDurationMs ? `${duration(scanDurationMs)} scan` : undefined}>
        Current view
      </SectionCaption>
      {status === "scanning" && progress && (
        <div className="db-scan-strip" data-testid="scan-strip">
          <div className="db-scan-row">
            <Spinner size={16} />
            <span className="tnum">
              {progress.files.toLocaleString()} files · {bytes(progress.bytes)}
            </span>
          </div>
          <div className="db-scan-row db-scan-path" title={progress.currentPath}>
            {progress.currentPath}
          </div>
          <div className="db-scan-bar">
            <i />
          </div>
        </div>
      )}
      {show && viewName && (
        <div className="db-current">
          <strong>{viewName}</strong>
          {viewPath ? (
            <TailPath path={viewPath} className="db-current-path" />
          ) : (
            <span className="db-current-path">No folder selected</span>
          )}
          <div className="db-sidebar-actions" style={{ marginTop: 9 }}>
            <OutlineButton onClick={reveal} title={REVEAL_NAME}>
              <EyeIcon size={14} /> Reveal
            </OutlineButton>
            <OutlineButton confirmText="Copied ✓" onConfirm={copy}>
              <CopyIcon size={14} /> Copy Path
            </OutlineButton>
          </div>
        </div>
      )}
    </>
  );
}
