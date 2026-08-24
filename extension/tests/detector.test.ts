import { describe, expect, it } from "vitest";
import {
  chooseBestMediaCandidate,
  describeSourceUrl,
  scoreMediaCandidate,
  type MediaCandidate
} from "../src/content/detector";

const candidate = (overrides: Partial<MediaCandidate> = {}): MediaCandidate => ({
  id: "media-1",
  mediaKind: "video",
  durationSeconds: 3600,
  playing: false,
  ended: false,
  width: 1280,
  height: 720,
  hasDirectSource: true,
  protectedMedia: false,
  ...overrides
});

describe("media candidate ranking", () => {
  it("prioritizes currently playing media over a larger idle player", () => {
    const idle = candidate({ id: "idle", width: 3840, height: 2160 });
    const playing = candidate({ id: "playing", playing: true, width: 640, height: 360 });

    expect(chooseBestMediaCandidate([idle, playing])?.id).toBe("playing");
    expect(scoreMediaCandidate(playing)).toBeGreaterThan(scoreMediaCandidate(idle));
  });

  it("keeps DOM order when candidates have an equal score", () => {
    const first = candidate({ id: "first", width: 0, height: 0 });
    const second = candidate({ id: "second", width: 0, height: 0 });

    expect(chooseBestMediaCandidate([first, second])?.id).toBe("first");
  });

  it("still recognizes protected media so existing captions remain discoverable", () => {
    const protectedCandidate = candidate({ protectedMedia: true, hasDirectSource: false });
    expect(scoreMediaCandidate(protectedCandidate)).toBeGreaterThan(0);
  });
});

describe("media source classification", () => {
  it("turns a safe local media URL into a distinct transient local source", () => {
    expect(describeSourceUrl("file:///C:/Users/Alice/Videos/meeting%20one.mp4", "file:///C:/Users/Alice/index.html")).toEqual({
      kind: "local_file",
      path: "C:\\Users\\Alice\\Videos\\meeting one.mp4"
    });
    expect(describeSourceUrl("file:///var/tmp/meeting.mp4", "file:///var/tmp/player.html")).toEqual({
      kind: "local_file",
      path: "/var/tmp/meeting.mp4"
    });
  });

  it("does not classify UNC or query-bearing file URLs as local media", () => {
    expect(describeSourceUrl("file://recording-server/share/meeting.mp4", "file:///C:/Users/Alice/index.html")).toEqual({
      kind: "opaque",
      reason: "unsupported-protocol"
    });
    expect(describeSourceUrl("file:///C:/Users/Alice/meeting.mp4?version=2", "file:///C:/Users/Alice/index.html")).toEqual({
      kind: "opaque",
      reason: "unsupported-protocol"
    });
  });
});
