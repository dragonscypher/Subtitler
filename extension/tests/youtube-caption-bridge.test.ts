import { afterEach, describe, expect, it, vi } from "vitest";
import {
  chooseEnglishYoutubeCaptionTrack,
  fetchYoutubeTimedTextJson3FromExtension,
  parseYoutubeTimedTextPayload,
  toCaptionTrackDescriptor,
  youtubeTrackIdFromDescriptor
} from "../src/background/youtube-caption-bridge";

describe("YouTube caption bridge mapping", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("keeps only a safe provider marker and track identifier in page detection metadata", () => {
    const descriptor = toCaptionTrackDescriptor({
      id: ".en",
      label: "English",
      language: "en",
      kind: "manual",
      captionBaseUrl: "https://www.youtube.com/api/timedtext?v=example&lang=en&signature=ephemeral"
    });

    expect(descriptor).toEqual({
      id: "youtube:.en",
      kind: "captions",
      label: "English",
      language: "en",
      mode: "disabled",
      provider: "youtube"
    });
    expect(JSON.stringify(descriptor)).not.toContain("timedtext");
    expect(youtubeTrackIdFromDescriptor(descriptor)).toBe(".en");
  });

  it("does not treat a lookalike or generic caption descriptor as a YouTube track", () => {
    expect(
      youtubeTrackIdFromDescriptor({
        id: "youtube:.en",
        kind: "captions",
        mode: "disabled"
      })
    ).toBeUndefined();
    expect(
      youtubeTrackIdFromDescriptor({
        id: "youtube:../../not-a-track",
        kind: "captions",
        mode: "disabled",
        provider: "youtube"
      })
    ).toBeUndefined();
  });

  it("selects only an English existing-caption track for the V1 fast path", () => {
    const selected = chooseEnglishYoutubeCaptionTrack([
      {
        id: ".es",
        label: "Spanish",
        language: "es",
        kind: "manual",
        captionBaseUrl: "https://www.youtube.com/api/timedtext?v=example&lang=es"
      },
      {
        id: ".en-auto",
        label: "English (automatic)",
        language: "en",
        kind: "asr",
        captionBaseUrl: "https://www.youtube.com/api/timedtext?v=example&lang=en&kind=asr"
      },
      {
        id: ".en-GB",
        label: "English (United Kingdom)",
        language: "en-GB",
        kind: "manual",
        captionBaseUrl: "https://www.youtube.com/api/timedtext?v=example&lang=en-GB"
      }
    ]);

    expect(selected?.id).toBe(".en-auto");
    expect(chooseEnglishYoutubeCaptionTrack([{
      id: ".fr",
      label: "French",
      language: "fr",
      kind: "manual",
      captionBaseUrl: "https://www.youtube.com/api/timedtext?v=example&lang=fr"
    }])).toBeUndefined();
  });

  it("uses Chrome's existing YouTube session only for a validated caption endpoint", async () => {
    const url = "https://www.youtube.com/api/timedtext?v=example&lang=en&fmt=json3";
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({
          events: [{ tStartMs: 0, dDurationMs: 1_000, segs: [{ utf8: "hello" }] }]
        }),
        { status: 200, headers: { "content-type": "application/json" } }
      )
    );
    vi.stubGlobal("fetch", fetchMock);

    await expect(fetchYoutubeTimedTextJson3FromExtension(url)).resolves.toMatchObject({ status: "ok" });
    expect(fetchMock).toHaveBeenCalledWith(url, {
      credentials: "include",
      redirect: "error",
      cache: "no-store"
    });
  });

  it("accepts YouTube's anti-XSSI and legacy timedtext transport wrappers", () => {
    expect(
      parseYoutubeTimedTextPayload(")]}'\n{\"events\":[{\"tStartMs\":0,\"dDurationMs\":1000,\"segs\":[{\"utf8\":\"hello\"}]}]}")
    ).toMatchObject({ events: [{ tStartMs: 0, dDurationMs: 1_000 }] });
    expect(
      parseYoutubeTimedTextPayload('<transcript><text start="1.25" dur="2.5">hello &amp; goodbye</text></transcript>')
    ).toEqual({
      events: [{ tStartMs: 1_250, dDurationMs: 2_500, segs: [{ utf8: "hello &amp; goodbye" }] }]
    });
    expect(
      parseYoutubeTimedTextPayload("WEBVTT\n\n00:00:01.250 --> 00:00:03.750\nhello")
    ).toEqual({
      events: [{ tStartMs: 1_250, dDurationMs: 2_500, segs: [{ utf8: "hello" }] }]
    });
  });
});
