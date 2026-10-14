/**
 * Byte / percent / age / duration formatting — the TS twin of
 * `core/src/format.rs` (BuildPrompt §14: "Rust and TS versions must
 * agree, both unit-tested").
 *
 * Windows flavor: **binary** base (1024) with KB/MB/GB/TB labels and
 * 3 significant digits, matching Explorer's Properties dialog and
 * `StrFormatByteSize` ("118 GB", "5.12 GB", "33.7 MB").
 */

const UNITS = ["B", "KB", "MB", "GB", "TB", "PB", "EB"] as const;

/** Format a byte count with binary units and 3 significant digits. */
export function bytes(value: number | bigint): string {
  let v = Number(value);
  if (!Number.isFinite(v)) return "n/a";
  if (v < 0) return `-${bytes(-v)}`;
  if (v < 1024) return `${Math.round(v)} B`;
  // Largest k such that v >= 1024^k, k >= 1 — mirrors the Rust twin's
  // loop exactly (k starts at 1, unit at 1024^1).
  let k = 1;
  let unit = 1024;
  while (v >= unit * 1024 && k + 1 < UNITS.length) {
    unit *= 1024;
    k += 1;
  }
  let scaled = v / unit;
  // Rounding may cross the unit boundary (e.g. 1023.99 MB → 1024): bump
  // once — exactly the Rust twin's rule, so both sides always agree.
  if (scaled >= 1023.5 && k + 1 < UNITS.length) {
    k += 1;
    scaled /= 1024;
  }
  // 3 significant digits: >=100 → integer, >=10 → 1 decimal, else 2 decimals.
  if (scaled >= 99.995) return `${scaled.toFixed(0)} ${UNITS[k]}`;
  if (scaled >= 9.9995) return `${scaled.toFixed(1)} ${UNITS[k]}`;
  return `${scaled.toFixed(2)} ${UNITS[k]}`;
}

/** Percentage of a whole; below 0.1% renders as "<0.1%". */
export function percent(fraction: number): string {
  const pct = fraction * 100;
  if (pct < 0.1) return "<0.1%";
  const tenths = Math.round(pct * 10);
  // One decimal, dropped when it rounds to an integer ("12.3%", "50%").
  return tenths % 10 === 0 ? `${pct.toFixed(0)}%` : `${pct.toFixed(1)}%`;
}

/**
 * Relative age from a Unix-seconds timestamp to `now` (seconds).
 * `"Just now"`, `"42 seconds ago"`, `"3 days ago"`, `"7 months ago"`, …
 * `modified <= 0` means unknown → `"—"`.
 */
const ago = (n: number, unit: string) => `${n} ${unit}${n === 1 ? "" : "s"} ago`;

export function relativeAge(modified: number, now: number): string {
  if (modified <= 0) return "n/a";
  const delta = now - modified;
  if (delta < 0) return "Just now"; // clock skew — display-only choice
  if (delta < 10) return "Just now";
  if (delta < 60) return ago(Math.floor(delta), "second");
  if (delta < 3600) return ago(Math.floor(delta / 60), "minute");
  if (delta < 86400) return ago(Math.floor(delta / 3600), "hour");
  if (delta < 7 * 86400) return ago(Math.floor(delta / 86400), "day");
  if (delta < 30 * 86400) return ago(Math.floor(delta / (7 * 86400)), "week");
  if (delta < 365 * 86400) return ago(Math.floor(delta / (30 * 86400)), "month");
  return ago(Math.floor(delta / (365 * 86400)), "year");
}

/** Format an elapsed duration: `"5.5s"`, `"12s"`, `"1m 03s"`, `"2h 04m"`. */
export function duration(millis: number): string {
  if (millis < 1000) return `${Math.round(millis)}ms`;
  if (millis < 10000) return `${(millis / 1000).toFixed(1)}s`;
  if (millis < 60000) return `${Math.round(millis / 1000)}s`;
  if (millis < 3600000) {
    const m = Math.floor(millis / 60000);
    const s = Math.floor((millis % 60000) / 1000);
    return `${m}m ${String(s).padStart(2, "0")}s`;
  }
  const h = Math.floor(millis / 3600000);
  const m = Math.floor((millis % 3600000) / 60000);
  return `${h}h ${String(m).padStart(2, "0")}m`;
}
