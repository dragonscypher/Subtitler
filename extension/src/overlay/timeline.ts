import type { SubtitleCue } from "../shared/domain";

const TIME_EPSILON_SECONDS = 0.001;
export const MAX_OVERLAY_CUES = 100_000;

export function normalizeOverlayCues(cues: readonly SubtitleCue[]): SubtitleCue[] {
  return cues
    .filter(
      (cue) =>
        Boolean(cue.id) &&
        Boolean(cue.text.trim()) &&
        Number.isFinite(cue.startSeconds) &&
        Number.isFinite(cue.endSeconds) &&
        cue.startSeconds >= 0 &&
        cue.endSeconds > cue.startSeconds
    )
    .map((cue) => {
      const normalized: SubtitleCue = {
        id: cue.id,
        startSeconds: cue.startSeconds,
        endSeconds: cue.endSeconds,
        text: normalizeCueText(cue.text)
      };
      const speaker = cue.speaker?.trim();
      if (speaker) {
        normalized.speaker = speaker;
      }
      return normalized;
    })
    .sort(compareCues);
}

/**
 * Merge a bounded native page into overlay state. The native cursor creates
 * stable IDs, so retries and late duplicate pages replace the same cue rather
 * than duplicating it. A hard in-page cap avoids unbounded DOM-side memory.
 */
export function mergeOverlayCues(existing: readonly SubtitleCue[], incoming: readonly SubtitleCue[]): SubtitleCue[] | null {
  const merged = new Map<string, SubtitleCue>();
  for (const cue of normalizeOverlayCues(existing)) {
    merged.set(cue.id, cue);
  }
  for (const cue of normalizeOverlayCues(incoming)) {
    merged.set(cue.id, cue);
  }
  if (merged.size > MAX_OVERLAY_CUES) {
    return null;
  }
  return [...merged.values()].sort(compareCues);
}

/** Returns the most recently started cue when source cues overlap. */
export function findActiveCue(cues: readonly SubtitleCue[], timeSeconds: number): SubtitleCue | undefined {
  if (!Number.isFinite(timeSeconds) || timeSeconds < 0) {
    return undefined;
  }

  let low = 0;
  let high = cues.length - 1;
  let firstAfterTime = cues.length;
  while (low <= high) {
    const middle = Math.floor((low + high) / 2);
    const cue = cues[middle];
    if (!cue) {
      break;
    }
    if (cue.startSeconds <= timeSeconds + TIME_EPSILON_SECONDS) {
      low = middle + 1;
    } else {
      firstAfterTime = middle;
      high = middle - 1;
    }
  }

  for (let index = firstAfterTime - 1; index >= 0; index -= 1) {
    const cue = cues[index];
    if (!cue) {
      continue;
    }
    if (cue.endSeconds > timeSeconds + TIME_EPSILON_SECONDS) {
      return cue;
    }
  }
  return undefined;
}

export function cueDisplayText(cue: SubtitleCue | undefined): string {
  if (!cue) {
    return "";
  }
  return cue.speaker ? `${cue.speaker}: ${cue.text}` : cue.text;
}

function normalizeCueText(text: string): string {
  return text
    .split(/\r?\n/)
    .map((line) => line.replace(/[^\S\r\n]+/g, " ").trim())
    .filter(Boolean)
    .join("\n");
}

function compareCues(left: SubtitleCue, right: SubtitleCue): number {
  return left.startSeconds - right.startSeconds || left.endSeconds - right.endSeconds || left.id.localeCompare(right.id);
}
