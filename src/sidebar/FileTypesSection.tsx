/**
 * Sidebar §6 (spec §6.8): File Types — thin stacked bar across the 9
 * categories + legend rows (color dot, label, size).
 */
import { useEffect, useState } from "react";
import { SectionCaption } from "../components/buttons";
import { invoke } from "../lib/ipc";
import { bytes } from "../lib/format";
import { useScanStore } from "../state/scan";
import { useViewStore } from "../state/view";

export interface TypeSegment {
  label: string;
  color: number;
  size: number;
}

export function FileTypesSection() {
  const status = useScanStore((s) => s.status);
  const generation = useScanStore((s) => s.generation);
  const setTab = useViewStore((s) => s.setTab);
  const [segs, setSegs] = useState<TypeSegment[] | null>(null);

  useEffect(() => {
    if (status !== "done") {
      setSegs(null);
      return;
    }
    let disposed = false;
    void (async () => {
      const res = await invoke<TypeSegment[]>("file_types", { generation }).catch(() => null);
      if (!disposed) setSegs(res);
    })();
    return () => {
      disposed = true;
    };
  }, [status, generation]);

  if (!segs || segs.length === 0) return null;
  const total = segs.reduce((a, s) => a + s.size, 0);
  const css = (c: number) => `#${c.toString(16).padStart(6, "0")}`;

  return (
    <>
      <SectionCaption right={bytes(total)}>File Types</SectionCaption>
      <div className="db-types-bar" role="img" aria-label="File types by size">
        {segs.map((s) => (
          <i key={s.label} style={{ width: `${(s.size / total) * 100}%`, background: css(s.color) }} />
        ))}
      </div>
      <div className="db-types-legend">
        {segs.map((s) => (
          <button key={s.label} type="button" onClick={() => setTab("explore")} title={`${s.label} · ${bytes(s.size)}`}>
            <i style={{ background: css(s.color) }} />
            <span>{s.label}</span>
            <b className="tnum">{bytes(s.size)}</b>
          </button>
        ))}
      </div>
    </>
  );
}
