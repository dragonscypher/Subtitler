import type { SubtitleCue, TranscriptSegment } from "../shared/domain";

/**
 * Transcript content is deliberately process-memory only. These limits bound a
 * compromised or malfunctioning local host without putting user content in
 * chrome.storage. They are sized for long recordings while still preventing a
 * service worker from retaining unbounded text.
 */
export const MAX_TRANSCRIPT_SEGMENTS = 100_000;
export const MAX_TRANSCRIPT_CUES = 100_000;
export const MAX_TRANSCRIPT_CHARACTERS = 32 * 1024 * 1024;
export const TRANSCRIPT_POPUP_PAGE_LIMIT = 100;

export interface TranscriptPage {
  jobId: string;
  segments: TranscriptSegment[];
  nextCursor?: number;
}

/** Complete, transient source data for an explicitly requested export. */
export interface CompletedTranscriptResult {
  readonly segments: readonly TranscriptSegment[];
  readonly cues: readonly SubtitleCue[];
}

export type TranscriptAppendResult =
  | { ok: true }
  | { ok: false; reason: "inactive" | "capacity" | "invalid" };

interface TranscriptEntry {
  segments: TranscriptSegment[];
  cues: SubtitleCue[];
  /** Combined transcript/cue text bound for this service-worker lifetime. */
  characterCount: number;
  complete: boolean;
}

/**
 * An intentionally non-persistent result cache for a single extension service
 * worker lifetime. Callers can retrieve bounded pages only after the native
 * client has drained a completed transcript job.
 */
export class TranscriptResultStore {
  private readonly entries = new Map<string, TranscriptEntry>();

  begin(jobId: string): void {
    this.entries.set(jobId, { segments: [], cues: [], characterCount: 0, complete: false });
  }

  append(jobId: string, segments: readonly TranscriptSegment[]): TranscriptAppendResult {
    const entry = this.entries.get(jobId);
    if (!entry || entry.complete) {
      return { ok: false, reason: "inactive" };
    }

    const additionalCharacters = segments.reduce((total, segment) => total + transcriptSegmentCharacterCount(segment), 0);
    if (
      entry.segments.length + segments.length > MAX_TRANSCRIPT_SEGMENTS ||
      entry.characterCount + additionalCharacters > MAX_TRANSCRIPT_CHARACTERS
    ) {
      return { ok: false, reason: "capacity" };
    }
    entry.segments.push(...segments.map(cloneTranscriptSegment));
    entry.characterCount += additionalCharacters;
    return { ok: true };
  }

  appendCues(jobId: string, cues: readonly SubtitleCue[]): TranscriptAppendResult {
    const entry = this.entries.get(jobId);
    if (!entry || entry.complete) {
      return { ok: false, reason: "inactive" };
    }
    if (!isValidCuePage(entry.cues, cues)) {
      return { ok: false, reason: "invalid" };
    }
    const additionalCharacters = cues.reduce((total, cue) => total + subtitleCueCharacterCount(cue), 0);
    if (
      entry.cues.length + cues.length > MAX_TRANSCRIPT_CUES ||
      entry.characterCount + additionalCharacters > MAX_TRANSCRIPT_CHARACTERS
    ) {
      return { ok: false, reason: "capacity" };
    }
    entry.cues.push(...cues.map(cloneSubtitleCue));
    entry.characterCount += additionalCharacters;
    return { ok: true };
  }

  complete(jobId: string): void {
    const entry = this.entries.get(jobId);
    if (entry) {
      entry.complete = true;
    }
  }

  discard(jobId: string): void {
    this.entries.delete(jobId);
  }

  /** Drop only incomplete pages after a native-host disconnect. */
  discardIncomplete(): void {
    for (const [jobId, entry] of this.entries) {
      if (!entry.complete) {
        this.entries.delete(jobId);
      }
    }
  }

  /**
   * Returns `undefined` until a job's complete result has been retained, or
   * when the requested cursor is outside that result. The caller owns all
   * user-facing wording so this class never formats transcript contents.
   */
  getPage(jobId: string, cursor: number, limit: number): TranscriptPage | undefined {
    const entry = this.entries.get(jobId);
    if (
      !entry ||
      !entry.complete ||
      !Number.isSafeInteger(cursor) ||
      cursor < 0 ||
      !Number.isSafeInteger(limit) ||
      limit < 1 ||
      limit > TRANSCRIPT_POPUP_PAGE_LIMIT ||
      cursor > entry.segments.length
    ) {
      return undefined;
    }
    const pageEnd = Math.min(entry.segments.length, cursor + limit);
    const page: TranscriptPage = {
      jobId,
      segments: entry.segments.slice(cursor, pageEnd).map(cloneTranscriptSegment)
    };
    if (pageEnd < entry.segments.length) {
      page.nextCursor = pageEnd;
    }
    return page;
  }

  /**
   * Returns internal, readonly export input only after both page streams drain.
   * This is background-only and intentionally avoids duplicating a long
   * transcript merely to format one explicitly requested download.
   */
  getCompletedResult(jobId: string): CompletedTranscriptResult | undefined {
    const entry = this.entries.get(jobId);
    if (!entry || !entry.complete) {
      return undefined;
    }
    return {
      segments: entry.segments,
      cues: entry.cues
    };
  }
}

function transcriptSegmentCharacterCount(segment: TranscriptSegment): number {
  return segment.text.length + (segment.speaker?.length ?? 0);
}

function cloneTranscriptSegment(segment: TranscriptSegment): TranscriptSegment {
  const clone: TranscriptSegment = {
    startSeconds: segment.startSeconds,
    endSeconds: segment.endSeconds,
    text: segment.text
  };
  if (segment.speaker !== undefined) {
    clone.speaker = segment.speaker;
  }
  return clone;
}

function subtitleCueCharacterCount(cue: SubtitleCue): number {
  return cue.text.length + (cue.speaker?.length ?? 0);
}

function cloneSubtitleCue(cue: SubtitleCue): SubtitleCue {
  const clone: SubtitleCue = {
    id: cue.id,
    startSeconds: cue.startSeconds,
    endSeconds: cue.endSeconds,
    text: cue.text
  };
  if (cue.speaker !== undefined) {
    clone.speaker = cue.speaker;
  }
  return clone;
}

/**
 * Native pagination is expected to be stable and timeline ordered. Validate
 * that invariant across page boundaries so export never has to silently sort,
 * overlap, or invent subtitle timing in the browser.
 */
function isValidCuePage(previousCues: readonly SubtitleCue[], incomingCues: readonly SubtitleCue[]): boolean {
  let previousEnd = previousCues.length > 0 ? previousCues[previousCues.length - 1]!.endSeconds : 0;
  for (const cue of incomingCues) {
    if (
      !Number.isFinite(cue.startSeconds) ||
      !Number.isFinite(cue.endSeconds) ||
      cue.startSeconds < previousEnd ||
      cue.endSeconds <= cue.startSeconds ||
      !hasSafeCueText(cue.text)
    ) {
      return false;
    }
    previousEnd = cue.endSeconds;
  }
  return true;
}

function hasSafeCueText(value: string): boolean {
  return value
    .replace(/\r\n?/g, "\n")
    .split("\n")
    .every((line) => line.trim().length > 0);
}
