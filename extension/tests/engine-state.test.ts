import { describe, expect, it } from "vitest";
import { EngineConnectionStateStore } from "../src/background/engine-state";

describe("EngineConnectionStateStore", () => {
  it("keeps a validated advisory in memory only until the engine disconnects", () => {
    const state = new EngineConnectionStateStore();
    expect(state.snapshot()).toEqual({ connected: false, localProcessingAvailable: false });

    const ready = state.markReady(true, {
      selectionSource: "automatic",
      model: "small",
      quantization: "q5_k_m",
      backend: "cpu",
      localPerformance: "good"
    });
    expect(ready).toEqual({
      connected: true,
      localProcessingAvailable: true,
      localProcessingAdvisory: {
        selectionSource: "automatic",
        model: "small",
        quantization: "q5_k_m",
        backend: "cpu",
        localPerformance: "good"
      }
    });

    // Snapshots cannot mutate the background's transient recommendation.
    ready.localProcessingAdvisory!.model = "tiny";
    expect(state.snapshot().localProcessingAdvisory?.model).toBe("small");
    expect(state.markDisconnected()).toEqual({ connected: false, localProcessingAvailable: false });
  });
});
