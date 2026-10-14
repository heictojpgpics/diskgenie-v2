/**
 * Boundary tests for the TS format twin — the SAME fixtures as
 * `core/src/format.rs` (both twins must agree; spec §14/§17).
 */
import { describe, expect, it } from "vitest";

import { bytes, duration, percent, relativeAge } from "./format";

describe("bytes (binary, 3 significant digits — Windows Explorer flavor)", () => {
  it("bytes below the first bump render as plain bytes", () => {
    expect(bytes(0)).toBe("0 B");
    expect(bytes(1)).toBe("1 B");
    expect(bytes(999)).toBe("999 B");
    expect(bytes(1023)).toBe("1023 B");
  });

  it("crosses each binary boundary exactly", () => {
    expect(bytes(1024)).toBe("1.00 KB");
    expect(bytes(1024 * 1024)).toBe("1.00 MB");
    expect(bytes(1024 ** 4)).toBe("1.00 TB");
  });

  it("rounds across the bump, not down into it", () => {
    expect(bytes(1048575)).toBe("1.00 MB"); // 1023.99 KB rounds across
  });

  it("matches the spec examples", () => {
    expect(bytes(33700000)).toBe("32.1 MB");
    expect(bytes(35338675)).toBe("33.7 MB");
    expect(bytes(5497563032)).toBe("5.12 GB");
    expect(bytes(126663778304)).toBe("118 GB");
  });

  it("uses 3 significant digits at each magnitude", () => {
    expect(bytes(999 * 1024 ** 3)).toBe("999 GB");
    expect(bytes(512 * 1024 ** 2)).toBe("512 MB");
  });

  it("accepts bigint (sizes come from u64 over IPC)", () => {
    expect(bytes(126663778304n)).toBe("118 GB");
    expect(bytes(0n)).toBe("0 B");
  });
});

describe("percent", () => {
  it("renders below-tenth values as <0.1%", () => {
    expect(percent(0)).toBe("<0.1%");
    expect(percent(0.0009)).toBe("<0.1%"); // 0.09%
  });
  it("keeps one decimal where it matters", () => {
    expect(percent(0.001)).toBe("0.1%");
    expect(percent(0.1234)).toBe("12.3%");
  });
  it("drops a zero decimal", () => {
    expect(percent(0.5)).toBe("50%");
    expect(percent(1.0)).toBe("100%");
  });
});

describe("relativeAge", () => {
  const now = 1_800_000_000;
  it("unknown timestamps render as an em dash", () => {
    expect(relativeAge(0, now)).toBe("n/a");
  });
  it("bucketizes the same as the Rust twin", () => {
    expect(relativeAge(now - 5, now)).toBe("Just now");
    expect(relativeAge(now - 42, now)).toBe("42 seconds ago");
    expect(relativeAge(now - 5 * 60, now)).toBe("5 minutes ago");
    expect(relativeAge(now - 3 * 3600, now)).toBe("3 hours ago");
    expect(relativeAge(now - 3 * 86400, now)).toBe("3 days ago");
    expect(relativeAge(now - 14 * 86400, now)).toBe("2 weeks ago");
    expect(relativeAge(now - 90 * 86400, now)).toBe("3 months ago");
    expect(relativeAge(now - 800 * 86400, now)).toBe("2 years ago");
  });
  it("singular units drop the s (premium copy: never '1 days ago')", () => {
    expect(relativeAge(now - 30, now)).toBe("30 seconds ago");
    expect(relativeAge(now - 60, now)).toBe("1 minute ago");
    expect(relativeAge(now - 3600, now)).toBe("1 hour ago");
    expect(relativeAge(now - 86400, now)).toBe("1 day ago");
    expect(relativeAge(now - 7 * 86400, now)).toBe("1 week ago");
    expect(relativeAge(now - 30 * 86400, now)).toBe("1 month ago");
    expect(relativeAge(now - 365 * 86400, now)).toBe("1 year ago");
  });
});

describe("duration", () => {
  it("sub-second renders in milliseconds", () => {
    expect(duration(120)).toBe("120ms");
  });
  it("seconds keep one decimal under 10s", () => {
    expect(duration(5500)).toBe("5.5s");
    expect(duration(12345)).toBe("12s");
  });
  it("minutes pad seconds", () => {
    expect(duration(63000)).toBe("1m 03s");
  });
  it("hours pad minutes", () => {
    expect(duration(2 * 3600000 + 4 * 60000)).toBe("2h 04m");
  });
});
