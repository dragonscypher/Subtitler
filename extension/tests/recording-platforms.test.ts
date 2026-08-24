import { describe, expect, it } from "vitest";
import {
  classifyRecordingPageUrl,
  opaqueSourceGuidanceFor,
  recordingPlatformMetadataFor
} from "../src/platforms/recording-platforms";

describe("recording platform URL classification", () => {
  it("recognizes only known YouTube video page routes", () => {
    expect(classifyRecordingPageUrl("https://www.youtube.com/watch?v=abc123")).toMatchObject({
      id: "youtube",
      displayName: "YouTube",
      pageKind: "youtube-watch",
      knownRecordingPath: true,
      captionAvailability: "youtube-existing-captions"
    });
    expect(classifyRecordingPageUrl("https://m.youtube.com/embed/abc123").pageKind).toBe("youtube-embed");
    expect(classifyRecordingPageUrl("https://music.youtube.com/shorts/abc123").pageKind).toBe("youtube-shorts");
    expect(classifyRecordingPageUrl("https://youtu.be/abc123").pageKind).toBe("youtube-short-link");
  });

  it("recognizes bounded Webex and Zoom recording routes", () => {
    expect(
      classifyRecordingPageUrl("https://acme.webex.com/recordingservice/sites/acme/playback/recording-id")
    ).toMatchObject({ id: "webex", displayName: "Webex", pageKind: "webex-recording", knownRecordingPath: true });
    expect(
      classifyRecordingPageUrl("https://acme.webex.com/recordingservice/sites/acme/recording/recording-id/playback")
    ).toMatchObject({ id: "webex", pageKind: "webex-recording", knownRecordingPath: true });
    expect(
      classifyRecordingPageUrl("https://acme.webex.com/webappng/sites/acme/recording/recording-id/playback")
    ).toMatchObject({ id: "webex", pageKind: "webex-recording", knownRecordingPath: true });
    expect(classifyRecordingPageUrl("https://acme.zoom.us/rec/play/recording-id")).toMatchObject({
      id: "zoom",
      displayName: "Zoom",
      pageKind: "zoom-recording",
      knownRecordingPath: true
    });
    expect(classifyRecordingPageUrl("https://acme.zoom.us/rec/share/recording-id").pageKind).toBe("zoom-recording");
  });

  it("requires exact platform domain boundaries, HTTPS, no credentials, and a recording path", () => {
    const genericCases = [
      "https://youtube.com.evil.test/watch?v=abc123",
      "https://notyoutube.com/watch?v=abc123",
      "http://www.youtube.com/watch?v=abc123",
      "https://user:secret@www.youtube.com/watch?v=abc123",
      "https://www.youtube.com/channel/example",
      "https://acme.webex.com/meetings",
      "https://acme.webex.com/recordingservice/sites/acme/recording/playback",
      "https://acme.webex.com/webappng/sites/acme/recording/recording-id/preview",
      "https://webex.com.evil.test/recordingservice/sites/acme/playback/recording-id",
      "https://acme.zoom.us/rec/play",
      "https://zoom.us.evil.test/rec/play/recording-id",
      "https://example.test/recording"
    ];

    for (const value of genericCases) {
      expect(classifyRecordingPageUrl(value)).toMatchObject({
        id: "generic",
        pageKind: "generic",
        knownRecordingPath: false,
        displayName: "This page"
      });
    }
  });

  it("returns safe platform metadata and opaque-source guidance without retaining page secrets", () => {
    const classification = classifyRecordingPageUrl(
      "https://acme.zoom.us/rec/play/recording-id?access_token=do-not-retain&cookie=do-not-retain"
    );
    const serialized = JSON.stringify(classification);

    expect(serialized).not.toContain("access_token");
    expect(serialized).not.toContain("do-not-retain");
    expect(classification.opaqueSourceGuidance).toContain("will not copy credentials");
    expect(classification.opaqueSourceGuidance).toContain("will not bypass recording protections");
    expect(recordingPlatformMetadataFor("youtube")).toEqual({
      id: "youtube",
      displayName: "YouTube",
      captionAvailability: "youtube-existing-captions"
    });
  });

  it("gives every supported opaque source a clear no-credentials/no-bypass message", () => {
    for (const platform of ["youtube", "webex", "zoom"] as const) {
      const guidance = opaqueSourceGuidanceFor(platform);
      expect(guidance).toContain("will not copy credentials");
      expect(guidance).toMatch(/will not bypass .*protections/u);
    }
    expect(opaqueSourceGuidanceFor("generic")).toContain("will not copy credentials");
  });
});
