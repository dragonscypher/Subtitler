/** Shared, serializable types. Do not add media URLs, cookies, or transcript text to persisted job records. */

export type JobKind = "subtitle" | "transcript";

export type JobStatus =
  | "queued"
  | "connecting"
  | "processing"
  | "buffering"
  | "using-existing-captions"
  | "completed"
  | "stopped"
  | "failed"
  | "recovering"
  | "stale";

export type JobPhase =
  | "resolving"
  | "acquiring"
  | "decoding"
  | "transcribing"
  | "segmenting"
  | "finalizing"
  | "complete"
  | "failed"
  | "cancelled"
  | "stale";

export type WorkerStatus = "not_started" | "active" | "waiting" | "finished" | "unavailable";

export type MediaKind = "video" | "audio";

export interface SubtitleCue {
  id: string;
  startSeconds: number;
  endSeconds: number;
  text: string;
  speaker?: string;
}

/**
 * A readable timestamped transcript segment. Unlike a subtitle cue it has no
 * rendering identity: transcript content stays in transient extension/page
 * memory and is never added to a persisted JobRecord.
 */
export interface TranscriptSegment {
  startSeconds: number;
  endSeconds: number;
  text: string;
  speaker?: string;
}

export interface CaptionTrackDescriptor {
  id: string;
  kind: "subtitles" | "captions";
  label?: string;
  language?: string;
  mode: "disabled" | "hidden" | "showing";
  cueCount?: number;
  /**
   * Omitted for a browser `TextTrack`. A platform adapter may identify an
   * existing caption source without exposing a media stream to the native
   * engine. This is page-local detection metadata and is never persisted in a
   * job record.
   */
  provider?: "youtube";
}

/**
 * Source locations are ephemeral job input only. Neither direct URLs nor local
 * paths may be put in JobRecord, extension storage, analytics, or logs.
 */
export type MediaSource =
  | {
      kind: "direct";
      url: string;
      mimeType?: string;
    }
  | {
      /** A validated path derived from a local `file:` media URL. */
      kind: "local_file";
      path: string;
      mimeType?: string;
    }
  | {
      kind: "opaque";
      reason: "missing-source" | "blob-url" | "media-source" | "unsupported-protocol";
    };

export interface MediaSnapshot {
  id: string;
  mediaKind: MediaKind;
  durationSeconds: number | null;
  currentTimeSeconds: number;
  playing: boolean;
  ended: boolean;
  protectedMedia: boolean;
  source: MediaSource;
  captionTracks: CaptionTrackDescriptor[];
  dimensions?: {
    width: number;
    height: number;
  };
}

export type MediaDetectionResult =
  | {
      state: "detected";
      media: MediaSnapshot;
      detectedCount: number;
    }
  | {
      state: "none";
      detectedCount: 0;
      reason: "no-html5-media";
    };

export interface JobProgress {
  processedSeconds?: number;
  durationSeconds?: number;
  percent?: number;
  subtitleBufferSeconds?: number;
  /** Safe, non-content engine state such as buffer health or recovery advice. */
  statusMessage?: string;
  phase?: JobPhase;
  /** ISO UTC time converted from the host's local-activity heartbeat. */
  lastProgressAt?: string;
  mediaBytesProcessed?: number;
  audioSecondsDecoded?: number;
  audioSecondsTranscribed?: number;
  completedIntervals?: number;
  workerPid?: number;
  workerStatus?: WorkerStatus;
}

export interface JobFailure {
  code:
    | "COMPANION_UNAVAILABLE"
    | "UNSUPPORTED_MEDIA"
    | "PROTECTED_MEDIA"
    | "NATIVE_ERROR"
    | "USER_STOPPED"
    | "UNKNOWN";
  message: string;
  retryable: boolean;
}

/**
 * This is deliberately safe to persist in chrome.storage.local. It contains no
 * source URL, authentication material, media bytes, caption text, or transcript.
 */
export interface JobRecord {
  id: string;
  /** Native-generated UUID; safe operational metadata, never a media URL or credential. */
  nativeJobId?: string;
  kind: JobKind;
  status: JobStatus;
  createdAt: string;
  updatedAt: string;
  tabId?: number;
  mediaId?: string;
  mediaDurationSeconds?: number;
  progress?: JobProgress;
  error?: JobFailure;
  usesExistingCaptions?: boolean;
}

export interface UserFacingError {
  code: JobFailure["code"] | "NO_MEDIA" | "INVALID_URL" | "PAGE_UNAVAILABLE";
  message: string;
}

export type Result<T> =
  | {
      ok: true;
      data: T;
    }
  | {
      ok: false;
      error: UserFacingError;
    };

export function success<T>(data: T): Result<T> {
  return { ok: true, data };
}

export function failure<T = never>(code: UserFacingError["code"], message: string): Result<T> {
  return { ok: false, error: { code, message } };
}
