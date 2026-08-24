import { describe, expect, it } from "vitest";
import type { JobRecord } from "../src/shared/domain";
import { completedTranscriptJobsToHydrate } from "../src/background/recovery";

const completedTranscript: JobRecord = {
  id: "recovered-transcript",
  kind: "transcript",
  status: "completed",
  nativeJobId: "11111111-1111-4111-8111-111111111111",
  tabId: 7,
  createdAt: "2026-01-01T00:00:00.000Z",
  updatedAt: "2026-01-01T00:00:00.000Z"
};

describe("completedTranscriptJobsToHydrate", () => {
  it("claims a completed result once so popup polling cannot return it to recovering", () => {
    const hydrated = new Set<string>();
    const first = completedTranscriptJobsToHydrate([completedTranscript], 7, hydrated);

    expect(first).toEqual([completedTranscript]);
    first.forEach((job) => hydrated.add(job.id));

    expect(completedTranscriptJobsToHydrate([completedTranscript], 7, hydrated)).toEqual([]);
  });

  it("does not rehydrate a transcript from another tab or an existing-caption path", () => {
    const existingCaption = { ...completedTranscript, id: "existing", usesExistingCaptions: true };
    expect(completedTranscriptJobsToHydrate([completedTranscript, existingCaption], 8, new Set())).toEqual([]);
    expect(completedTranscriptJobsToHydrate([existingCaption], 7, new Set())).toEqual([]);
  });
});
