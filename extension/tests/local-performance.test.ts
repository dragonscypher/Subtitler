import { describe, expect, it } from "vitest";
import { describeLocalPerformance } from "../src/popup/local-performance";

describe("local performance popup note", () => {
  const baseAdvisory = {
    selectionSource: "automatic" as const,
    model: "small" as const,
    quantization: "q5_k_m" as const,
    backend: "cpu" as const
  };

  it.each([
    ["excellent", true, "Local processing: excellent performance expected."],
    ["good", true, "Local processing: good performance expected."],
    ["may_be_slow", true, "Local processing: this device may be slow."],
    ["cloud_helpful", true, "Local processing: this device may be slow."],
    ["good", false, "Local plan when the engine is available: good performance expected."]
  ] as const)("renders %s without technical or cloud controls", (localPerformance, localProcessingAvailable, expected) => {
    expect(describeLocalPerformance({ ...baseAdvisory, localPerformance }, localProcessingAvailable)).toBe(expected);
    expect(expected).not.toContain("cloud");
    expect(expected).not.toContain("CPU");
  });
});
