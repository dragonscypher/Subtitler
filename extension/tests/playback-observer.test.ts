import { afterEach, describe, expect, it, vi } from "vitest";
import {
  PlaybackObserver,
  PLAYBACK_SNAPSHOT_INTERVAL_MS,
  type ObservableMediaElement
} from "../src/content/playback-observer";
import type { PlaybackUpdateSnapshot } from "../src/shared/protocol";

class FakeMediaElement implements ObservableMediaElement {
  currentTime = 0;
  playbackRate = 1;
  paused = true;
  ended = false;
  private readonly listeners = new Map<string, Set<EventListener>>();

  addEventListener(type: string, listener: EventListener): void {
    const listeners = this.listeners.get(type) ?? new Set<EventListener>();
    listeners.add(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type: string, listener: EventListener): void {
    this.listeners.get(type)?.delete(listener);
  }

  emit(type: string): void {
    for (const listener of this.listeners.get(type) ?? []) {
      listener({ type } as Event);
    }
  }
}

describe("PlaybackObserver", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("emits an initial snapshot, then low-rate snapshots only while playing", () => {
    vi.useFakeTimers();
    const media = new FakeMediaElement();
    const snapshots: PlaybackUpdateSnapshot[] = [];
    const observer = new PlaybackObserver(media, (snapshot) => snapshots.push(snapshot));

    observer.start();
    expect(snapshots).toEqual([{ positionMs: 0, playbackRateMilli: 1_000, isPaused: true, seekGeneration: 0 }]);
    vi.advanceTimersByTime(PLAYBACK_SNAPSHOT_INTERVAL_MS * 2);
    expect(snapshots).toHaveLength(1);

    media.currentTime = 3.125;
    media.paused = false;
    media.emit("play");
    expect(snapshots.at(-1)).toEqual({ positionMs: 3_125, playbackRateMilli: 1_000, isPaused: false, seekGeneration: 0 });
    media.currentTime = 5.75;
    vi.advanceTimersByTime(PLAYBACK_SNAPSHOT_INTERVAL_MS);
    expect(snapshots.at(-1)).toEqual({ positionMs: 5_750, playbackRateMilli: 1_000, isPaused: false, seekGeneration: 0 });

    media.paused = true;
    media.emit("pause");
    const countAfterPause = snapshots.length;
    vi.advanceTimersByTime(PLAYBACK_SNAPSHOT_INTERVAL_MS * 2);
    expect(snapshots).toHaveLength(countAfterPause);
    observer.stop();
  });

  it("reports seek, playback-rate, and final state changes immediately without retaining events", () => {
    const media = new FakeMediaElement();
    media.paused = false;
    const snapshots: PlaybackUpdateSnapshot[] = [];
    const observer = new PlaybackObserver(media, (snapshot) => snapshots.push(snapshot));
    observer.start();

    media.playbackRate = 6;
    media.emit("ratechange");
    expect(snapshots.at(-1)).toMatchObject({ playbackRateMilli: 4_000, seekGeneration: 0 });

    media.currentTime = 48.9;
    media.emit("seeking");
    expect(snapshots.at(-1)).toEqual({ positionMs: 48_900, playbackRateMilli: 4_000, isPaused: false, seekGeneration: 1 });
    media.emit("seeked");
    expect(snapshots.at(-1)).toEqual({ positionMs: 48_900, playbackRateMilli: 4_000, isPaused: false, seekGeneration: 1 });

    observer.stop();
    media.currentTime = 60;
    media.emit("seeking");
    expect(snapshots.at(-1)).toEqual({ positionMs: 48_900, playbackRateMilli: 4_000, isPaused: false, seekGeneration: 1 });
  });
});
