import { normalizeCaptionTracks } from "../shared/captions";
import type { CaptionTrackDescriptor, MediaDetectionResult, MediaSnapshot, MediaSource } from "../shared/domain";
import { sanitizeLocalFileUrl } from "../shared/protocol";

export interface MediaCandidate {
  id: string;
  mediaKind: "video" | "audio";
  durationSeconds: number | null;
  playing: boolean;
  ended: boolean;
  width?: number;
  height?: number;
  hasDirectSource: boolean;
  protectedMedia: boolean;
}

/**
 * This deliberately ranks an actively playing visible video over merely-present
 * players, while keeping a stable DOM-order tiebreaker in chooseBestMediaCandidate.
 */
export function scoreMediaCandidate(candidate: MediaCandidate): number {
  let score = 0;
  if (candidate.playing) {
    score += 10_000;
  }
  if (!candidate.ended) {
    score += 500;
  }
  if (candidate.mediaKind === "video") {
    score += 200;
    score += Math.min((candidate.width ?? 0) * (candidate.height ?? 0), 4_000_000) / 10_000;
  }
  if (candidate.durationSeconds !== null && candidate.durationSeconds > 0) {
    score += 100;
  }
  if (candidate.hasDirectSource) {
    score += 20;
  }
  if (candidate.protectedMedia) {
    score -= 50;
  }
  return score;
}

export function chooseBestMediaCandidate<T extends MediaCandidate>(candidates: readonly T[]): T | undefined {
  let best: T | undefined;
  let bestScore = Number.NEGATIVE_INFINITY;
  for (const candidate of candidates) {
    const score = scoreMediaCandidate(candidate);
    if (score > bestScore) {
      best = candidate;
      bestScore = score;
    }
  }
  return best;
}

export class MediaRegistry {
  private readonly elementToId = new WeakMap<HTMLMediaElement, string>();
  private readonly idToElement = new Map<string, HTMLMediaElement>();
  private nextMediaId = 1;

  detect(documentRoot: Document = document): MediaDetectionResult {
    const mediaElements = Array.from(documentRoot.querySelectorAll<HTMLMediaElement>("video, audio"));
    const snapshots = mediaElements.map((element) => this.snapshot(element));
    const candidateById = new Map(snapshots.map((snapshot) => [snapshot.id, toCandidate(snapshot)]));
    const selected = chooseBestMediaCandidate(snapshots.map((snapshot) => candidateById.get(snapshot.id)!));

    if (!selected) {
      return { state: "none", detectedCount: 0, reason: "no-html5-media" };
    }
    const snapshot = snapshots.find((item) => item.id === selected.id);
    if (!snapshot) {
      return { state: "none", detectedCount: 0, reason: "no-html5-media" };
    }
    return { state: "detected", media: snapshot, detectedCount: snapshots.length };
  }

  snapshot(element: HTMLMediaElement): MediaSnapshot {
    const id = this.idFor(element);
    const durationSeconds = finiteNonNegative(element.duration);
    const currentTimeSeconds = finiteNonNegative(element.currentTime) ?? 0;
    const video = element instanceof HTMLVideoElement ? element : undefined;
    const snapshot: MediaSnapshot = {
      id,
      mediaKind: video ? "video" : "audio",
      durationSeconds,
      currentTimeSeconds,
      playing: !element.paused && !element.ended && element.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA,
      ended: element.ended,
      protectedMedia: hasAttachedMediaKeys(element),
      source: describeSource(element),
      captionTracks: this.captionTracksFor(element)
    };
    if (video) {
      snapshot.dimensions = { width: video.videoWidth, height: video.videoHeight };
    }
    return snapshot;
  }

  findMedia(mediaId: string): HTMLMediaElement | undefined {
    const media = this.idToElement.get(mediaId);
    if (!media || !media.isConnected) {
      this.idToElement.delete(mediaId);
      return undefined;
    }
    return media;
  }

  findTextTrack(mediaId: string, trackId: string): TextTrack | undefined {
    const media = this.findMedia(mediaId);
    if (!media) {
      return undefined;
    }
    return Array.from(media.textTracks).find((_, index) => captionTrackId(mediaId, index) === trackId);
  }

  captionTracksFor(element: HTMLMediaElement): CaptionTrackDescriptor[] {
    const mediaId = this.idFor(element);
    return normalizeCaptionTracks(
      Array.from(element.textTracks).map((track, index) => ({
        id: captionTrackId(mediaId, index),
        kind: track.kind,
        label: track.label,
        language: track.language,
        mode: track.mode,
        cueCount: track.cues?.length
      }))
    );
  }

  private idFor(element: HTMLMediaElement): string {
    const existing = this.elementToId.get(element);
    if (existing) {
      return existing;
    }
    const id = `media-${this.nextMediaId}`;
    this.nextMediaId += 1;
    this.elementToId.set(element, id);
    this.idToElement.set(id, element);
    return id;
  }
}

export function describeSource(media: HTMLMediaElement): MediaSource {
  const source = media.currentSrc || media.src || media.querySelector<HTMLSourceElement>("source")?.src;
  if (!source) {
    return { kind: "opaque", reason: "missing-source" };
  }
  const mimeType = media.getAttribute("type") || media.querySelector<HTMLSourceElement>("source")?.type;
  return describeSourceUrl(source, document.baseURI, mimeType ?? undefined);
}

/**
 * Pure source classification used by page detection. A `file:` source yields
 * only a validated absolute path; the original URL never leaves this step.
 */
export function describeSourceUrl(source: string, baseUrl: string, mimeType?: string): MediaSource {
  try {
    const url = new URL(source, baseUrl);
    if (url.protocol === "blob:") {
      return { kind: "opaque", reason: "blob-url" };
    }
    if (url.protocol === "file:") {
      const path = sanitizeLocalFileUrl(url.href);
      if (!path) {
        return { kind: "opaque", reason: "unsupported-protocol" };
      }
      const sourceInfo: MediaSource = { kind: "local_file", path };
      if (mimeType) {
        sourceInfo.mimeType = mimeType;
      }
      return sourceInfo;
    }
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      return { kind: "opaque", reason: "unsupported-protocol" };
    }
    if (url.username || url.password) {
      return { kind: "opaque", reason: "unsupported-protocol" };
    }
    url.hash = "";
    const sourceInfo: MediaSource = { kind: "direct", url: url.href };
    if (mimeType) {
      sourceInfo.mimeType = mimeType;
    }
    return sourceInfo;
  } catch {
    return { kind: "opaque", reason: "media-source" };
  }
}

function toCandidate(snapshot: MediaSnapshot): MediaCandidate {
  const candidate: MediaCandidate = {
    id: snapshot.id,
    mediaKind: snapshot.mediaKind,
    durationSeconds: snapshot.durationSeconds,
    playing: snapshot.playing,
    ended: snapshot.ended,
    hasDirectSource: snapshot.source.kind === "direct" || snapshot.source.kind === "local_file",
    protectedMedia: snapshot.protectedMedia
  };
  if (snapshot.dimensions) {
    candidate.width = snapshot.dimensions.width;
    candidate.height = snapshot.dimensions.height;
  }
  return candidate;
}

function captionTrackId(mediaId: string, index: number): string {
  return `${mediaId}:track:${index}`;
}

function finiteNonNegative(value: number): number | null {
  return Number.isFinite(value) && value >= 0 ? value : null;
}

function hasAttachedMediaKeys(media: HTMLMediaElement): boolean {
  try {
    return media.mediaKeys !== null;
  } catch {
    return false;
  }
}
