import type { PlaybackUpdateSnapshot } from "../shared/protocol";

/** Two seconds is frequent enough for scheduler awareness without tracking every media frame. */
export const PLAYBACK_SNAPSHOT_INTERVAL_MS = 2_000;
const MIN_PLAYBACK_RATE_MILLI = 250;
const MAX_PLAYBACK_RATE_MILLI = 4_000;
const MAX_SEEK_GENERATION = 0xffff_ffff;

/**
 * Structural subset of HTMLMediaElement so event behavior can be tested without
 * a browser DOM. It never exposes source URLs or media bytes.
 */
export interface ObservableMediaElement {
  currentTime: number;
  playbackRate: number;
  paused: boolean;
  ended: boolean;
  addEventListener(type: string, listener: EventListener): void;
  removeEventListener(type: string, listener: EventListener): void;
}

export type PlaybackSnapshotListener = (snapshot: PlaybackUpdateSnapshot) => void;

/**
 * Produces a deliberately lossy stream of playback metadata. Regular snapshots
 * run only while the media plays; control-state and seek events emit promptly.
 * The native client applies a second rate limit and retains only the newest
 * unsent snapshot, so this observer cannot accumulate a message backlog.
 */
export class PlaybackObserver {
  private started = false;
  private seekGeneration = 0;
  private intervalId: ReturnType<typeof setInterval> | undefined;

  private readonly onPlay = (): void => {
    this.emitSnapshot();
    this.startInterval();
  };

  private readonly onPause = (): void => {
    this.stopInterval();
    this.emitSnapshot();
  };

  private readonly onRateChange = (): void => this.emitSnapshot();

  private readonly onSeeking = (): void => {
    this.seekGeneration = Math.min(MAX_SEEK_GENERATION, this.seekGeneration + 1);
    this.emitSnapshot();
  };

  private readonly onSeeked = (): void => this.emitSnapshot();

  private readonly onLoadedMetadata = (): void => this.emitSnapshot();

  private readonly onEnded = (): void => {
    this.stopInterval();
    this.emitSnapshot();
  };

  constructor(
    private readonly media: ObservableMediaElement,
    private readonly report: PlaybackSnapshotListener,
    private readonly intervalMs = PLAYBACK_SNAPSHOT_INTERVAL_MS
  ) {}

  start(): void {
    if (this.started) {
      return;
    }
    this.started = true;
    this.media.addEventListener("play", this.onPlay);
    this.media.addEventListener("pause", this.onPause);
    this.media.addEventListener("ratechange", this.onRateChange);
    this.media.addEventListener("seeking", this.onSeeking);
    this.media.addEventListener("seeked", this.onSeeked);
    this.media.addEventListener("loadedmetadata", this.onLoadedMetadata);
    this.media.addEventListener("ended", this.onEnded);
    this.emitSnapshot();
    this.startInterval();
  }

  stop(): void {
    if (!this.started) {
      return;
    }
    this.started = false;
    this.stopInterval();
    this.media.removeEventListener("play", this.onPlay);
    this.media.removeEventListener("pause", this.onPause);
    this.media.removeEventListener("ratechange", this.onRateChange);
    this.media.removeEventListener("seeking", this.onSeeking);
    this.media.removeEventListener("seeked", this.onSeeked);
    this.media.removeEventListener("loadedmetadata", this.onLoadedMetadata);
    this.media.removeEventListener("ended", this.onEnded);
  }

  private startInterval(): void {
    if (!this.started || this.media.paused || this.media.ended || this.intervalId !== undefined) {
      return;
    }
    this.intervalId = setInterval(() => {
      if (!this.started || this.media.paused || this.media.ended) {
        this.stopInterval();
        return;
      }
      this.emitSnapshot();
    }, this.intervalMs);
  }

  private stopInterval(): void {
    if (this.intervalId !== undefined) {
      clearInterval(this.intervalId);
      this.intervalId = undefined;
    }
  }

  private emitSnapshot(): void {
    if (!this.started) {
      return;
    }
    this.report({
      positionMs: toPositionMs(this.media.currentTime),
      playbackRateMilli: toPlaybackRateMilli(this.media.playbackRate),
      isPaused: this.media.paused || this.media.ended,
      seekGeneration: this.seekGeneration
    });
  }
}

function toPositionMs(currentTime: number): number {
  if (!Number.isFinite(currentTime) || currentTime <= 0) {
    return 0;
  }
  return Math.min(Number.MAX_SAFE_INTEGER, Math.round(currentTime * 1_000));
}

function toPlaybackRateMilli(playbackRate: number): number {
  const rate = Number.isFinite(playbackRate) && playbackRate > 0 ? playbackRate : 1;
  return Math.min(MAX_PLAYBACK_RATE_MILLI, Math.max(MIN_PLAYBACK_RATE_MILLI, Math.round(rate * 1_000)));
}
