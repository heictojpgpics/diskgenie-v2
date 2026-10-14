/**
 * Preview overlay (spec §8): modal for images / video / audio / PDF
 * (iframe on the asset URL) / text (first 64 KB via a Rust command) and
 * an icon + "Open with default app" for everything else. NEVER previews
 * cloud placeholders. Esc closes.
 */
import { useEffect, useRef, useState } from "react";
import { AppWindowIcon, ExternalLinkIcon, FileIcon, XIcon, CloudIcon } from "./Icon";
import { getNodeDetails, previewText, type NodeDetailsData } from "../viz/exploreIpc";
import { bytes } from "../lib/format";
import { IS_MAC } from "../lib/platform";
import { useFocusTrap } from "../lib/useFocusTrap";
import { Spinner } from "./buttons";

export function PreviewOverlay({
  generation,
  id,
  onClose,
  onOpenDefault,
}: {
  generation: number;
  id: number;
  onClose: () => void;
  onOpenDefault: (id: number) => void;
}) {
  const [details, setDetails] = useState<NodeDetailsData | null>(null);
  const [text, setText] = useState<{ text: string; truncated: boolean } | null>(null);
  const [kind, setKind] = useState<"loading" | "image" | "video" | "audio" | "pdf" | "text" | "other" | "cloud">("loading");
  const escRef = useRef<(e: KeyboardEvent) => void>(() => undefined);
  const boxRef = useRef<HTMLDivElement>(null);
  useFocusTrap(boxRef, true);

  useEffect(() => {
    let disposed = false;
    void (async () => {
      const d = await getNodeDetails(generation, id).catch(() => null);
      if (disposed || !d) {
        if (!disposed) setKind("other");
        return;
      }
      setDetails(d);
      if (d.isCloud) {
        setKind("cloud");
        return;
      }
      const cat = d.kind.toLowerCase();
      if (["documents", "other", "developer"].includes(cat)) {
        // Text-able kinds: resolve preview_text first and switch to the
        // text view on success (a stale-closure on the `kind` state once
        // left these stuck on the placeholder icon — kind was still
        // "loading" in this closure when the fetch settled).
        const isPdf = cat === "documents" && d.name.toLowerCase().endsWith(".pdf");
        if (isPdf) {
          setKind("pdf");
        } else {
          const t = await previewText(generation, id).catch(() => null);
          if (disposed) return;
          if (t) {
            setText(t);
            setKind("text");
          } else {
            setKind("other");
          }
        }
      } else if (cat === "images") setKind("image");
      else if (cat === "video") setKind("video");
      else if (cat === "audio") setKind("audio");
      else setKind("other");
    })();
    return () => {
      disposed = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [generation, id]);

  useEffect(() => {
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    escRef.current = esc;
    window.addEventListener("keydown", esc);
    return () => window.removeEventListener("keydown", esc);
  }, [onClose]);

  const name = details?.name ?? "";
  const size = details?.size ?? 0;
  const catColor = details ? `#${details.kindColor.toString(16).padStart(6, "0")}` : undefined;

  // Asset URL from the node's FULL path. The old stub built
  // `asset://C:/${name}` — filename only — so any file not directly in
  // C:\ pointed at a nonexistent URL (a C:\Users\dev\Pictures\IMG.jpg
  // preview tried to load C:/IMG.jpg). Use Tauri's own converter when
  // running under the runtime; mirror its scheme form for browser dev.
  const assetUrl = (): string => {
    const p = details?.path ?? "";
    if (!p) return "";
    const internals = (
      window as unknown as {
        __TAURI_INTERNALS__?: { convertFileSrc?: (path: string, protocol: string) => string };
      }
    ).__TAURI_INTERNALS__;
    if (typeof internals?.convertFileSrc === "function") {
      return internals.convertFileSrc(p, "asset");
    }
    // Browser-dev approximation (assets don't load outside the runtime
    // anyway): Tauri 2 forms — http://asset.localhost on WebView2,
    // asset://localhost on WKWebView.
    const enc = encodeURIComponent(p);
    return IS_MAC ? `asset://localhost/${enc}` : `http://asset.localhost/${enc}`;
  };

  return (
    <div className="db-scrim" role="dialog" aria-modal="true" aria-label={`Preview ${name}`}>
      <div className="db-preview" ref={boxRef}>
        <div className="db-preview-head">
          <FileIcon size={16} style={{ color: catColor }} />
          <strong title={name}>{name || "…"}</strong>
          <small className="tnum">{bytes(size)}</small>
          <button type="button" className="db-preview-close" onClick={onClose} aria-label="Close preview" title="Esc">
            <XIcon size={15} />
          </button>
        </div>
        <div className="db-preview-body db-scroll">
          {kind === "loading" && (
            <div className="db-preview-other">
              <Spinner />
            </div>
          )}
          {kind === "cloud" && (
            <div className="db-preview-other">
              <CloudIcon size={38} />
              <p>Stored in the cloud. Not downloaded.</p>
              <p style={{ fontSize: 11, marginTop: -4 }}>Previews are disabled so nothing gets downloaded.</p>
            </div>
          )}
          {kind === "image" && <img src={assetUrl()} alt={name} />}
          {kind === "video" && <video src={assetUrl()} controls />}
          {kind === "audio" && <audio src={assetUrl()} controls style={{ width: "80%" }} />}
          {kind === "pdf" && <iframe src={assetUrl()} title={name} />}
          {kind === "text" && (
            <pre>{text?.text ?? "…"}</pre>
          )}
          {kind === "other" && (
            <div className="db-preview-other">
              <span className="db-preview-fileicon">
                <AppWindowIcon size={34} />
              </span>
              <p>
                {details?.kind ?? "File"} · {bytes(size)}
              </p>
              <button
                type="button"
                className="db-outline"
                style={{ minHeight: 34, padding: "0 16px" }}
                onClick={() => onOpenDefault(id)}
              >
                <ExternalLinkIcon size={14} /> Open with default app
              </button>
            </div>
          )}
        </div>
        {kind !== "other" && kind !== "loading" && kind !== "cloud" && (
          <footer className="db-preview-foot">
            <span>{details?.kind ?? name.split(".").pop()?.toUpperCase() ?? "File"}</span>
            <button
              type="button"
              className="db-outline"
              style={{ minHeight: 30, padding: "0 13px", fontSize: 11.5 }}
              onClick={() => onOpenDefault(id)}
            >
              <ExternalLinkIcon size={13} /> Open with default app
            </button>
          </footer>
        )}
      </div>
    </div>
  );
}
