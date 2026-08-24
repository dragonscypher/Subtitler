import { describe, expect, it } from "vitest";
import { cueDisplayText, findActiveCue, mergeOverlayCues, normalizeOverlayCues } from "../src/overlay/timeline";

const cues = normalizeOverlayCues([
  { id: "two", startSeconds: 4, endSeconds: 6, text: "Second cue" },
  { id: "one", startSeconds: 1, endSeconds: 3, text: " First   cue ", speaker: "Ada" },
  { id: "invalid", startSeconds: 5, endSeconds: 5, text: "bad" }
]);

describe("subtitle timeline", () => {
  it("normalizes, orders, and chooses a cue at exact media time", () => {
    expect(cues.map((cue) => cue.id)).toEqual(["one", "two"]);
    expect(cueDisplayText(findActiveCue(cues, 1.5))).toBe("Ada: First cue");
    expect(cueDisplayText(findActiveCue(cues, 3.2))).toBe("");
    expect(cueDisplayText(findActiveCue(cues, 4))).toBe("Second cue");
  });

  it("uses the latest-started cue when source cues overlap", () => {
    const overlapping = normalizeOverlayCues([
      { id: "wide", startSeconds: 0, endSeconds: 10, text: "Wide" },
      { id: "nested", startSeconds: 2, endSeconds: 4, text: "Nested" }
    ]);
    expect(findActiveCue(overlapping, 3)?.id).toBe("nested");
    expect(findActiveCue(overlapping, 8)?.id).toBe("wide");
  });

  it("clears immediately outside a cue after a seek", () => {
    expect(findActiveCue(cues, 100)).toBeUndefined();
    expect(findActiveCue(cues, -1)).toBeUndefined();
  });

  it("merges generated cue pages by stable ID without losing line breaks", () => {
    const firstPage = [{ id: "native:0", startSeconds: 0, endSeconds: 1, text: "First\nline" }];
    const merged = mergeOverlayCues(firstPage, [
      { id: "native:0", startSeconds: 0, endSeconds: 1, text: "First\nline" },
      { id: "native:1", startSeconds: 1, endSeconds: 2, text: "Second cue" }
    ]);
    expect(merged).toEqual([
      { id: "native:0", startSeconds: 0, endSeconds: 1, text: "First\nline" },
      { id: "native:1", startSeconds: 1, endSeconds: 2, text: "Second cue" }
    ]);
  });
});
