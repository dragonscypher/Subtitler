import { describe, expect, it } from "vitest";
import { findActiveGeneratedSubtitleJob } from "../src/background/playback-routing";
import type { JobRecord } from "../src/shared/domain";

const target = {
  tabId: 12,
  jobId: "11111111-1111-4111-8111-111111111111",
  mediaId: "media-1"
};

function job(overrides: Partial<JobRecord> = {}): JobRecord {
  return {
    id: target.jobId,
    nativeJobId: "22222222-2222-4222-8222-222222222222",
    kind: "subtitle",
    status: "processing",
    createdAt: "2026-01-01T00:00:00.000Z",
    updatedAt: "2026-01-01T00:00:00.000Z",
    tabId: target.tabId,
    mediaId: target.mediaId,
    ...overrides
  };
}

describe("generated subtitle playback routing", () => {
  it("routes only the exact active generated subtitle job", () => {
    const active = job();
    expect(findActiveGeneratedSubtitleJob([active], target)).toEqual(active);
    expect(findActiveGeneratedSubtitleJob([job({ status: "buffering" })], target)).toBeDefined();
  });

  it("rejects transcript, existing-caption, terminal, and cross-context jobs", () => {
    const missingNativeJobId = job();
    delete missingNativeJobId.nativeJobId;
    const candidates = [
      job({ kind: "transcript" }),
      job({ usesExistingCaptions: true }),
      job({ status: "completed" }),
      job({ tabId: 99 }),
      job({ mediaId: "media-elsewhere" }),
      job({ id: "33333333-3333-4333-8333-333333333333" }),
      missingNativeJobId
    ];
    expect(findActiveGeneratedSubtitleJob(candidates, target)).toBeUndefined();
  });
});
