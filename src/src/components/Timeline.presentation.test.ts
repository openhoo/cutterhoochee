import { describe, expect, it } from "vitest";

import { normalizeTimelineRange } from "./Timeline";

describe("timeline presentation range", () => {
  it("accepts a nonempty native frame interval", () => {
    expect(normalizeTimelineRange({ startFrame: 20, endFrame: 45 })).toEqual({ startFrame: 20, endFrame: 45 });
  });

  it("rejects empty, inverted, negative, and unsafe ranges instead of highlighting them", () => {
    expect(normalizeTimelineRange(undefined)).toBeUndefined();
    expect(normalizeTimelineRange({ startFrame: 20, endFrame: 20 })).toBeUndefined();
    expect(normalizeTimelineRange({ startFrame: 45, endFrame: 20 })).toBeUndefined();
    expect(normalizeTimelineRange({ startFrame: -1, endFrame: 20 })).toBeUndefined();
    expect(normalizeTimelineRange({ startFrame: Number.MAX_SAFE_INTEGER + 1, endFrame: Number.MAX_SAFE_INTEGER + 2 })).toBeUndefined();
  });
});
