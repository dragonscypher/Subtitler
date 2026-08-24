import { describe, expect, it } from "vitest";
import {
  MAX_TRANSCRIPT_CHARACTERS,
  MAX_TRANSCRIPT_SEGMENTS,
  TranscriptResultStore
} from "../src/background/transcript-store";

const segment = (text: string, startSeconds = 0) => ({ startSeconds, endSeconds: startSeconds + 1, text });

describe("TranscriptResultStore", () => {
  it("keeps transcript pages transient until the native result is complete", () => {
    const store = new TranscriptResultStore();
    store.begin("extension-job");
    expect(store.append("extension-job", [segment("First", 0), { ...segment("Second", 1), speaker: "Ada" }])).toEqual({ ok: true });
    expect(store.appendCues("extension-job", [{ id: "cue-1", startSeconds: 0, endSeconds: 1, text: "First cue" }])).toEqual({
      ok: true
    });
    expect(store.getPage("extension-job", 0, 1)).toBeUndefined();

    store.complete("extension-job");
    expect(store.getPage("extension-job", 0, 1)).toEqual({
      jobId: "extension-job",
      segments: [segment("First", 0)],
      nextCursor: 1
    });
    expect(store.getPage("extension-job", 1, 100)).toEqual({
      jobId: "extension-job",
      segments: [{ ...segment("Second", 1), speaker: "Ada" }]
    });
    expect(store.getCompletedResult("extension-job")).toEqual({
      segments: [segment("First", 0), { ...segment("Second", 1), speaker: "Ada" }],
      cues: [{ id: "cue-1", startSeconds: 0, endSeconds: 1, text: "First cue" }]
    });
  });

  it("clones returned data and clears incomplete transcript data on disconnect", () => {
    const store = new TranscriptResultStore();
    store.begin("completed");
    store.append("completed", [segment("Private text")]);
    store.complete("completed");
    const returned = store.getPage("completed", 0, 1);
    if (!returned) throw new Error("Expected the completed transcript page.");
    returned.segments[0]!.text = "mutated";
    expect(store.getPage("completed", 0, 1)?.segments[0]?.text).toBe("Private text");

    store.begin("partial");
    store.append("partial", [segment("Do not retain")]);
    store.discardIncomplete();
    expect(store.getPage("partial", 0, 1)).toBeUndefined();
    expect(store.getPage("completed", 0, 1)).toBeDefined();
  });

  it("fails closed when content would exceed its process-memory bounds", () => {
    const store = new TranscriptResultStore();
    store.begin("bounded");
    expect(store.append("bounded", Array.from({ length: MAX_TRANSCRIPT_SEGMENTS }, (_, index) => segment("x", index)))).toEqual({
      ok: true
    });
    expect(store.append("bounded", [segment("one more")])).toEqual({ ok: false, reason: "capacity" });

    const characters = new TranscriptResultStore();
    characters.begin("characters");
    expect(characters.append("characters", [segment("x".repeat(MAX_TRANSCRIPT_CHARACTERS + 1))])).toEqual({
      ok: false,
      reason: "capacity"
    });
  });

  it("rejects final cue pages that are out of timeline order or contain blank cue lines", () => {
    const store = new TranscriptResultStore();
    store.begin("cue-order");
    expect(
      store.appendCues("cue-order", [{ id: "first", startSeconds: 1, endSeconds: 2, text: "First" }])
    ).toEqual({ ok: true });
    expect(
      store.appendCues("cue-order", [{ id: "overlap", startSeconds: 1.5, endSeconds: 2.5, text: "Overlapping" }])
    ).toEqual({ ok: false, reason: "invalid" });
    expect(
      store.appendCues("cue-order", [{ id: "blank", startSeconds: 2, endSeconds: 3, text: "Line one\n   " }])
    ).toEqual({ ok: false, reason: "invalid" });
  });
});
