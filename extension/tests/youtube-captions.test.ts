import { describe, expect, it } from "vitest";
import {
  chooseYoutubeCaptionTrack,
  createYoutubeTimedTextJson3Url,
  extractYoutubeCaptionTracks,
  isYoutubeCaptionEndpoint,
  isYoutubeVideoPageUrl,
  parseYoutubeTimedTextJson3,
  sanitizeYoutubeCaptionBaseUrl
} from "../src/platforms/youtube-captions";

const englishCaptionUrl =
  "https://www.youtube.com/api/timedtext?v=video-id&lang=en&kind=asr&expire=9999999999&signature=temporary-request-signature";

describe("YouTube caption-only adapter", () => {
  it("recognizes only safe YouTube watch, embed, shorts, and shortened-video pages", () => {
    expect(isYoutubeVideoPageUrl("https://www.youtube.com/watch?v=abc123")).toBe(true);
    expect(isYoutubeVideoPageUrl("https://m.youtube.com/embed/abc123")).toBe(true);
    expect(isYoutubeVideoPageUrl("https://music.youtube.com/shorts/abc123")).toBe(true);
    expect(isYoutubeVideoPageUrl("https://youtu.be/abc123")).toBe(true);

    expect(isYoutubeVideoPageUrl("http://www.youtube.com/watch?v=abc123")).toBe(false);
    expect(isYoutubeVideoPageUrl("https://www.youtube.com/channel/example")).toBe(false);
    expect(isYoutubeVideoPageUrl("https://youtube.com.evil.test/watch?v=abc123")).toBe(false);
    expect(isYoutubeVideoPageUrl("https://user:secret@www.youtube.com/watch?v=abc123")).toBe(false);
    expect(isYoutubeVideoPageUrl("https://youtu.be/abc123/extra")).toBe(false);
  });

  it("extracts only existing validated caption tracks from the fixed caption path", () => {
    const playerResponse = {
      streamingData: {
        adaptiveFormats: [{ url: "https://r1---sn.example.googlevideo.com/videoplayback?audio=secret" }]
      },
      cookies: "must-not-be-exposed",
      accessToken: "must-not-be-exposed",
      captions: {
        playerCaptionsTracklistRenderer: {
          captionTracks: [
            {
              baseUrl: englishCaptionUrl,
              vssId: ".en",
              languageCode: "en",
              kind: "asr",
              name: { runs: [{ text: "English" }, { text: " (auto-generated)" }] },
              authToken: "must-not-be-exposed"
            },
            {
              baseUrl: "https://www.youtube.com/api/timedtext?v=video-id&lang=es",
              vssId: ".es",
              languageCode: "es",
              name: { simpleText: "<b>Spanish</b>" }
            },
            {
              baseUrl: "https://r1---sn.example.googlevideo.com/videoplayback?audio=secret",
              vssId: ".not-caption",
              languageCode: "en",
              name: { simpleText: "Must be ignored" }
            },
            {
              baseUrl: "https://www.youtube.com/api/timedtext?v=video-id&access_token=secret",
              vssId: ".token",
              languageCode: "en",
              name: { simpleText: "Must be ignored" }
            }
          ]
        }
      }
    };

    expect(extractYoutubeCaptionTracks(playerResponse)).toEqual([
      {
        id: ".en",
        label: "English (auto-generated)",
        language: "en",
        kind: "asr",
        captionBaseUrl: englishCaptionUrl
      },
      {
        id: ".es",
        label: "Spanish",
        language: "es",
        kind: "manual",
        captionBaseUrl: "https://www.youtube.com/api/timedtext?v=video-id&lang=es"
      }
    ]);
    expect(Object.keys(extractYoutubeCaptionTracks(playerResponse)[0] ?? {})).toEqual([
      "id",
      "label",
      "language",
      "kind",
      "captionBaseUrl"
    ]);
  });

  it("selects an existing preferred-language caption and favors manual tracks", () => {
    const tracks = extractYoutubeCaptionTracks({
      captions: {
        playerCaptionsTracklistRenderer: {
          captionTracks: [
            {
              baseUrl: "https://www.youtube.com/api/timedtext?v=video-id&lang=en&kind=asr",
              vssId: ".en-asr",
              languageCode: "en",
              kind: "asr",
              name: { simpleText: "English (auto)" }
            },
            {
              baseUrl: "https://www.youtube.com/api/timedtext?v=video-id&lang=en-US",
              vssId: ".en-US",
              languageCode: "en-US",
              name: { simpleText: "English (United States)" }
            },
            {
              baseUrl: "https://www.youtube.com/api/timedtext?v=video-id&lang=fr",
              vssId: ".fr",
              languageCode: "fr",
              name: { simpleText: "French" }
            }
          ]
        }
      }
    });

    expect(chooseYoutubeCaptionTrack(tracks, { preferredLanguage: "en-US" })?.id).toBe(".en-US");
    expect(chooseYoutubeCaptionTrack(tracks, { preferredLanguage: "en-GB" })?.id).toBe(".en-US");
    expect(chooseYoutubeCaptionTrack(tracks)?.id).toBe(".en-US");
    expect(
      chooseYoutubeCaptionTrack([
        {
          ...tracks[0]!,
          captionBaseUrl: "https://r1---sn.example.googlevideo.com/videoplayback"
        }
      ])
    ).toBeUndefined();
  });

  it("accepts only HTTPS YouTube timedtext endpoints and produces a json3 request URL", () => {
    expect(isYoutubeCaptionEndpoint(englishCaptionUrl)).toBe(true);
    expect(isYoutubeCaptionEndpoint("https://www.youtube-nocookie.com/api/timedtext?v=video-id")).toBe(true);
    expect(isYoutubeCaptionEndpoint("https://m.youtube.com/api/timedtext?v=video-id")).toBe(true);
    expect(isYoutubeCaptionEndpoint("http://www.youtube.com/api/timedtext?v=video-id")).toBe(false);
    expect(isYoutubeCaptionEndpoint("https://user:secret@www.youtube.com/api/timedtext?v=video-id")).toBe(false);
    expect(isYoutubeCaptionEndpoint("https://www.youtube.com.evil.test/api/timedtext?v=video-id")).toBe(false);
    expect(isYoutubeCaptionEndpoint("https://www.youtube.com/watch?v=video-id")).toBe(false);
    expect(isYoutubeCaptionEndpoint("https://www.youtube.com/api/timedtext?v=video-id&cookie=secret")).toBe(false);
    expect(sanitizeYoutubeCaptionBaseUrl("https://www.youtube.com/api/timedtext?v=video-id#fragment")).toBe(
      "https://www.youtube.com/api/timedtext?v=video-id"
    );
    expect(createYoutubeTimedTextJson3Url("https://www.youtube.com/api/timedtext?v=video-id&fmt=srv3")).toBe(
      "https://www.youtube.com/api/timedtext?v=video-id&fmt=json3"
    );
  });

  it("parses json3 events into timestamped clean UTF-8 cue inputs and drops invalid events", () => {
    const malformedUtf16 = String.fromCharCode(0xd800);
    expect(
      parseYoutubeTimedTextJson3({
        events: [
          {
            tStartMs: 1_600,
            dDurationMs: 900,
            segs: [{ utf8: "\nSecond\tline" }]
          },
          {
            tStartMs: 0,
            dDurationMs: 1_530,
            segs: [{ utf8: "<b>Hello</b> " }, { utf8: "world&nbsp; &amp; <i>team</i>" }]
          },
          { tStartMs: -1, dDurationMs: 500, segs: [{ utf8: "invalid start" }] },
          { tStartMs: 2_000, dDurationMs: 0, segs: [{ utf8: "invalid duration" }] },
          { tStartMs: 2_500, dDurationMs: 500, segs: [] },
          { tStartMs: 3_000, dDurationMs: 500, segs: [{ utf8: malformedUtf16 }] },
          { tStartMs: 3_500, dDurationMs: 500 }
        ]
      })
    ).toEqual([
      { tStartMs: 0, dDurationMs: 1_530, segs: [{ utf8: "Hello world & team" }] },
      { tStartMs: 1_600, dDurationMs: 900, segs: [{ utf8: "Second line" }] }
    ]);
  });
});
