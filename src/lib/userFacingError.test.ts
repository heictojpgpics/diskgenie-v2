import { describe, expect, it } from "vitest";
import { userFacingError } from "./userFacingError";

describe("userFacingError", () => {
  it("passes plain Rust string errors through", () => {
    expect(userFacingError("Couldn't write the snapshot: boom")).toBe("Couldn't write the snapshot: boom");
  });

  it("unwraps Error instances to .message", () => {
    expect(userFacingError(new Error("io failed"))).toBe("io failed");
  });

  it("stringifies non-error objects", () => {
    expect(userFacingError(42)).toBe("42");
  });

  it("strips the ACTIVATION_REQUIRED protocol prefix but keeps the sentence", () => {
    expect(userFacingError("ACTIVATION_REQUIRED — Activate DiskGenie Pro to use this.")).toBe(
      "Activate DiskGenie Pro to use this.",
    );
  });

  it("strips the LICENSE_STALE protocol prefix", () => {
    expect(userFacingError("LICENSE_STALE — Your license needs revalidation.")).toBe(
      "Your license needs revalidation.",
    );
  });

  it("strips the new period-separated gate format too", () => {
    expect(userFacingError("ACTIVATION_REQUIRED. Activate DiskGenie Pro to use this.")).toBe(
      "Activate DiskGenie Pro to use this.",
    );
    expect(userFacingError("LICENSE_STALE. DiskGenie couldn't verify your license.")).toBe(
      "DiskGenie couldn't verify your license.",
    );
  });

  it("handles a bare marker with no sentence (never an empty banner)", () => {
    expect(userFacingError("ACTIVATION_REQUIRED")).toBe("Something went wrong.");
  });

  it("never returns empty text", () => {
    expect(userFacingError("").length).toBeGreaterThan(0);
    expect(userFacingError("   ").length).toBeGreaterThan(0);
  });
});
