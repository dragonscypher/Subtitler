import { describe, expect, it } from "vitest";
import { MAX_TRANSCRIPT_EXPORT_BYTES, prepareTranscriptExport } from "../src/background/transcript-export";

const transcript = {
  segments: [
    { startSeconds: 0, endSeconds: 1.25, text: "Hello\r\nworld", speaker: "Ada" },
    { startSeconds: 61.5, endSeconds: 63, text: "Second segment" }
  ],
  cues: [
    { id: "cue-1", startSeconds: 0, endSeconds: 1.25, text: "Hello\nworld", speaker: "Ada" },
    { id: "cue-2", startSeconds: 61.5, endSeconds: 63, text: "Second cue" }
  ]
} as const;

describe("prepareTranscriptExport", () => {
  it("renders all five V1 formats using fixed safe names", () => {
    expect(prepareTranscriptExport("txt", transcript)).toEqual({
      ok: true,
      value: {
        format: "txt",
        filename: "Transcript.txt",
        mimeType: "text/plain;charset=utf-8",
        content: "Ada: Hello\nworld\nSecond segment"
      }
    });
    expect(prepareTranscriptExport("timestamped_txt", transcript)).toMatchObject({
      ok: true,
      value: {
        filename: "Transcript-timestamped.txt",
        content: "[00:00:00.000] Ada: Hello\nworld\n[00:01:01.500] Second segment"
      }
    });
    expect(prepareTranscriptExport("srt", transcript)).toMatchObject({
      ok: true,
      value: {
        filename: "Subtitles.srt",
        mimeType: "application/x-subrip;charset=utf-8",
        content: "1\n00:00:00,000 --> 00:00:01,250\nHello\nworld\n\n2\n00:01:01,500 --> 00:01:03,000\nSecond cue\n\n"
      }
    });
    expect(prepareTranscriptExport("vtt", transcript)).toMatchObject({
      ok: true,
      value: {
        filename: "Subtitles.vtt",
        mimeType: "text/vtt;charset=utf-8",
        content: "WEBVTT\n\n00:00:00.000 --> 00:00:01.250\nHello\nworld\n\n00:01:01.500 --> 00:01:03.000\nSecond cue\n\n"
      }
    });
    expect(prepareTranscriptExport("json", transcript)).toMatchObject({
      ok: true,
      value: {
        filename: "Transcript.json",
        mimeType: "application/json;charset=utf-8",
        content: expect.stringContaining('"start_ms":0')
      }
    });
  });

  it("does not synthesize subtitle formats when final cues are unavailable", () => {
    expect(prepareTranscriptExport("srt", { ...transcript, cues: [] })).toEqual({
      ok: false,
      reason: "subtitle_cues_unavailable"
    });
    expect(prepareTranscriptExport("vtt", { ...transcript, cues: [] })).toEqual({
      ok: false,
      reason: "subtitle_cues_unavailable"
    });
  });

  it("fails closed rather than exporting out-of-order or overlapping final cues", () => {
    expect(
      prepareTranscriptExport("srt", {
        ...transcript,
        cues: [
          { id: "later", startSeconds: 2, endSeconds: 3, text: "Later" },
          { id: "earlier", startSeconds: 1, endSeconds: 2, text: "Earlier" }
        ]
      })
    ).toEqual({ ok: false, reason: "subtitle_cues_invalid" });
  });

  it("refuses to assemble output beyond the global in-memory export cap", () => {
    expect(
      prepareTranscriptExport("txt", {
        segments: [{ startSeconds: 0, endSeconds: 1, text: "x".repeat(MAX_TRANSCRIPT_EXPORT_BYTES + 1) }],
        cues: []
      })
    ).toEqual({ ok: false, reason: "too_large" });
  });
});
