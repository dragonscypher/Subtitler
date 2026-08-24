import type { SubtitleCue, TranscriptSegment } from "../shared/domain";
import type { TranscriptExportFormat } from "../shared/protocol";
import { MAX_TRANSCRIPT_EXPORT_BYTES, type ExportMimeType } from "../shared/export-download-protocol";

/** Keep one explicit export payload safely below common extension-message limits. */
export { MAX_TRANSCRIPT_EXPORT_BYTES };

export interface CompletedTranscriptForExport {
  segments: readonly TranscriptSegment[];
  cues: readonly SubtitleCue[];
}

export interface PreparedTranscriptExport {
  format: TranscriptExportFormat;
  filename: "Transcript.txt" | "Transcript-timestamped.txt" | "Subtitles.srt" | "Subtitles.vtt" | "Transcript.json";
  mimeType: ExportMimeType;
  content: string;
}

export type TranscriptExportPreparation =
  | { ok: true; value: PreparedTranscriptExport }
  | { ok: false; reason: "subtitle_cues_unavailable" | "subtitle_cues_invalid" | "too_large" };

/**
 * Produces only user-requested, in-memory output. All filenames are fixed
 * product names, never derived from a page URL, media title, or local path.
 */
export function prepareTranscriptExport(
  format: TranscriptExportFormat,
  transcript: CompletedTranscriptForExport
): TranscriptExportPreparation {
  try {
    switch (format) {
      case "txt":
        return success(format, "Transcript.txt", "text/plain;charset=utf-8", renderPlainText(transcript.segments));
      case "timestamped_txt":
        return success(
          format,
          "Transcript-timestamped.txt",
          "text/plain;charset=utf-8",
          renderTimestampedText(transcript.segments)
        );
      case "json":
        return success(format, "Transcript.json", "application/json;charset=utf-8", renderJson(transcript.segments));
      case "srt":
        if (transcript.cues.length === 0) return { ok: false, reason: "subtitle_cues_unavailable" };
        if (!areExportCuesValid(transcript.cues)) return { ok: false, reason: "subtitle_cues_invalid" };
        return success(format, "Subtitles.srt", "application/x-subrip;charset=utf-8", renderSrt(transcript.cues));
      case "vtt":
        if (transcript.cues.length === 0) return { ok: false, reason: "subtitle_cues_unavailable" };
        if (!areExportCuesValid(transcript.cues)) return { ok: false, reason: "subtitle_cues_invalid" };
        return success(format, "Subtitles.vtt", "text/vtt;charset=utf-8", renderVtt(transcript.cues));
    }
  } catch (error) {
    if (error instanceof ExportTooLargeError) {
      return { ok: false, reason: "too_large" };
    }
    throw error;
  }
}

function success(
  format: PreparedTranscriptExport["format"],
  filename: PreparedTranscriptExport["filename"],
  mimeType: PreparedTranscriptExport["mimeType"],
  content: string
): TranscriptExportPreparation {
  return { ok: true, value: { format, filename, mimeType, content } };
}

function renderPlainText(segments: readonly TranscriptSegment[]): string {
  const output = new BoundedTextBuilder();
  for (let index = 0; index < segments.length; index += 1) {
    if (index > 0) output.append("\n");
    output.append(displayText(segments[index]!));
  }
  return output.finish();
}

function renderTimestampedText(segments: readonly TranscriptSegment[]): string {
  const output = new BoundedTextBuilder();
  for (let index = 0; index < segments.length; index += 1) {
    if (index > 0) output.append("\n");
    const segment = segments[index]!;
    output.append(`[${formatVttTimestamp(segment.startSeconds)}] ${displayText(segment)}`);
  }
  return output.finish();
}

function renderSrt(cues: readonly SubtitleCue[]): string {
  const output = new BoundedTextBuilder();
  for (let index = 0; index < cues.length; index += 1) {
    const cue = cues[index]!;
    output.append(`${index + 1}\n`);
    output.append(`${formatSrtTimestamp(cue.startSeconds)} --> ${formatSrtTimestamp(cue.endSeconds)}\n`);
    output.append(nativeCompatibleCueLines(cue));
    output.append("\n\n");
  }
  return output.finish();
}

function renderVtt(cues: readonly SubtitleCue[]): string {
  const output = new BoundedTextBuilder();
  output.append("WEBVTT\n\n");
  for (const cue of cues) {
    output.append(`${formatVttTimestamp(cue.startSeconds)} --> ${formatVttTimestamp(cue.endSeconds)}\n`);
    output.append(nativeCompatibleCueLines(cue));
    output.append("\n\n");
  }
  return output.finish();
}

function renderJson(segments: readonly TranscriptSegment[]): string {
  const output = new BoundedTextBuilder();
  output.append('{\n  "format": "subtitler-transcript-v1",\n  "segments": [');
  for (let index = 0; index < segments.length; index += 1) {
    const segment = segments[index]!;
    const item: {
      start_ms: number;
      end_ms: number;
      text: string;
      speaker?: string;
    } = {
      start_ms: Math.round(segment.startSeconds * 1_000),
      end_ms: Math.round(segment.endSeconds * 1_000),
      text: segment.text
    };
    if (segment.speaker !== undefined) item.speaker = segment.speaker;
    output.append(index === 0 ? "\n    " : ",\n    ");
    output.append(JSON.stringify(item));
  }
  if (segments.length > 0) output.append("\n  ");
  output.append("]\n}\n");
  return output.finish();
}

function displayText(segment: TranscriptSegment): string {
  const speaker = normalizeSpeaker(segment.speaker);
  return `${speaker ? `${speaker}: ` : ""}${normalizeLineEndings(segment.text).trim()}`;
}

/** Native subtitle exports preserve cue lines and do not inject speaker labels. */
function nativeCompatibleCueLines(cue: SubtitleCue): string {
  return normalizeLineEndings(cue.text)
    .split("\n")
    .map((line) => line.trim())
    .join("\n");
}

function normalizeSpeaker(value: string | undefined): string | undefined {
  if (value === undefined || value.length === 0) return undefined;
  return normalizeLineEndings(value).replaceAll("\n", " ");
}

function normalizeLineEndings(value: string): string {
  return value.replace(/\r\n?/g, "\n");
}

/**
 * The final native cue pages must already be timeline-ordered. Recheck that
 * invariant before creating a browser subtitle file rather than silently
 * sorting or manufacturing timing in the extension.
 */
function areExportCuesValid(cues: readonly SubtitleCue[]): boolean {
  let previousEnd = 0;
  for (const cue of cues) {
    if (
      !Number.isFinite(cue.startSeconds) ||
      !Number.isFinite(cue.endSeconds) ||
      cue.startSeconds < previousEnd ||
      cue.endSeconds <= cue.startSeconds ||
      nativeCompatibleCueLines(cue).split("\n").some((line) => line.length === 0)
    ) {
      return false;
    }
    previousEnd = cue.endSeconds;
  }
  return true;
}

function formatSrtTimestamp(seconds: number): string {
  const milliseconds = timestampMilliseconds(seconds);
  const hours = Math.floor(milliseconds / 3_600_000);
  const minutes = Math.floor((milliseconds % 3_600_000) / 60_000);
  const remainingSeconds = Math.floor((milliseconds % 60_000) / 1_000);
  return `${pad2(hours)}:${pad2(minutes)}:${pad2(remainingSeconds)},${pad3(milliseconds % 1_000)}`;
}

function formatVttTimestamp(seconds: number): string {
  const milliseconds = timestampMilliseconds(seconds);
  const hours = Math.floor(milliseconds / 3_600_000);
  const minutes = Math.floor((milliseconds % 3_600_000) / 60_000);
  const remainingSeconds = Math.floor((milliseconds % 60_000) / 1_000);
  return `${pad2(hours)}:${pad2(minutes)}:${pad2(remainingSeconds)}.${pad3(milliseconds % 1_000)}`;
}

function timestampMilliseconds(seconds: number): number {
  if (!Number.isFinite(seconds) || seconds < 0) {
    throw new TypeError("Transcript export received an invalid timestamp.");
  }
  return Math.round(seconds * 1_000);
}

function pad2(value: number): string {
  return Math.floor(value).toString().padStart(2, "0");
}

function pad3(value: number): string {
  return Math.floor(value).toString().padStart(3, "0");
}

class ExportTooLargeError extends Error {}

class BoundedTextBuilder {
  private readonly chunks: string[] = [];
  private byteLength = 0;
  private readonly encoder = new TextEncoder();

  append(value: string): void {
    const bytes = this.encoder.encode(value).byteLength;
    if (this.byteLength + bytes > MAX_TRANSCRIPT_EXPORT_BYTES) {
      throw new ExportTooLargeError();
    }
    this.chunks.push(value);
    this.byteLength += bytes;
  }

  finish(): string {
    return this.chunks.join("");
  }
}
