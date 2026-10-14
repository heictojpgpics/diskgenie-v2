/**
 * User-facing error text (production-readiness sweep).
 *
 * Store/view catch paths display command failures verbatim; two classes
 * of text should never reach a raw banner:
 *
 * 1. License-gate markers ("ACTIVATION_REQUIRED — …" / "LICENSE_STALE —
 *    …"): ipc.ts already intercepts these and the App shell opens the
 *    activation flow — a banner repeating the marker is noise on top of
 *    the dialog. The banner shows the human sentence without the
 *    ALL_CAPS protocol prefix.
 * 2. Tauri's serialized error shapes (Error instances / objects): use
 *    `.message` when present, `String()` otherwise.
 */

const GATE_PREFIX = /^(ACTIVATION_REQUIRED|LICENSE_STALE)[\s—\-.]*/;

/** Normalize any thrown value to honest, readable, banner-ready text. */
export function userFacingError(e: unknown): string {
  const raw = e instanceof Error ? e.message : typeof e === "string" ? e : String(e);
  const stripped = raw.replace(GATE_PREFIX, "").trim();
  return stripped.length > 0 ? stripped : "Something went wrong.";
}
