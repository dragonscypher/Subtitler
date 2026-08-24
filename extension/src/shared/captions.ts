import type { CaptionTrackDescriptor, SubtitleCue } from "./domain";

export interface CaptionTrackInput {
  id: string;
  kind: string;
  label?: string | null;
  language?: string | null;
  mode: string;
  cueCount?: number | null | undefined;
}

export interface CaptionCueInput {
  id?: string | null;
  startSeconds: number;
  endSeconds: number;
  text: string;
}

export function isCaptionKind(kind: string): kind is "subtitles" | "captions" {
  return kind === "subtitles" || kind === "captions";
}

export function normalizeCaptionTracks(inputs: readonly CaptionTrackInput[]): CaptionTrackDescriptor[] {
  const seen = new Set<string>();
  const tracks: CaptionTrackDescriptor[] = [];

  for (const input of inputs) {
    if (!isCaptionKind(input.kind) || !input.id || seen.has(input.id)) {
      continue;
    }

    const mode = normalizeTrackMode(input.mode);
    const track: CaptionTrackDescriptor = {
      id: input.id,
      kind: input.kind,
      mode
    };
    const label = cleanOptionalText(input.label);
    const language = cleanOptionalText(input.language);
    const cueCount = normalizeCueCount(input.cueCount);
    if (label) {
      track.label = label;
    }
    if (language) {
      track.language = language;
    }
    if (cueCount !== undefined) {
      track.cueCount = cueCount;
    }
    tracks.push(track);
    seen.add(input.id);
  }

  return tracks.sort(compareCaptionTracks);
}

export function chooseUsableCaptionTrack(
  tracks: readonly CaptionTrackDescriptor[]
): CaptionTrackDescriptor | undefined {
  return [...tracks].sort(compareCaptionTracks)[0];
}

export function normalizeCaptionCues(inputs: readonly CaptionCueInput[]): SubtitleCue[] {
  const cues: SubtitleCue[] = [];

  for (let index = 0; index < inputs.length; index += 1) {
    const input = inputs[index];
    if (!input || !Number.isFinite(input.startSeconds) || !Number.isFinite(input.endSeconds)) {
      continue;
    }
    if (input.startSeconds < 0 || input.endSeconds <= input.startSeconds) {
      continue;
    }
    const text = collapseWhitespace(input.text);
    if (!text) {
      continue;
    }
    cues.push({
      id: input.id?.trim() || `caption-${index + 1}`,
      startSeconds: input.startSeconds,
      endSeconds: input.endSeconds,
      text
    });
  }

  return cues.sort((left, right) => left.startSeconds - right.startSeconds || left.endSeconds - right.endSeconds);
}

function normalizeTrackMode(mode: string): CaptionTrackDescriptor["mode"] {
  if (mode === "showing" || mode === "hidden") {
    return mode;
  }
  return "disabled";
}

function normalizeCueCount(value: number | null | undefined): number | undefined {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? Math.floor(value) : undefined;
}

function cleanOptionalText(value: string | null | undefined): string | undefined {
  const normalized = value?.trim();
  return normalized ? normalized : undefined;
}

function collapseWhitespace(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function compareCaptionTracks(left: CaptionTrackDescriptor, right: CaptionTrackDescriptor): number {
  const modeScore = (track: CaptionTrackDescriptor): number => {
    if (track.mode === "showing") {
      return 0;
    }
    if (track.mode === "hidden") {
      return 1;
    }
    return 2;
  };
  return modeScore(left) - modeScore(right) || left.id.localeCompare(right.id);
}
