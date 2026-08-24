import type { SubtitleCue } from "../shared/domain";
import { cueDisplayText, findActiveCue, mergeOverlayCues, normalizeOverlayCues } from "./timeline";

type VideoFrameCapable = HTMLVideoElement & {
  requestVideoFrameCallback?: (callback: () => void) => number;
  cancelVideoFrameCallback?: (id: number) => void;
};

/**
 * A page-local overlay that is driven by the HTMLMediaElement clock. It never
 * captures or reads audio; it only renders timestamped cues supplied by native
 * processing or an already-authorized TextTrack.
 */
export class SubtitleOverlayController {
  private media: HTMLMediaElement | undefined;
  private cues: SubtitleCue[] = [];
  private host: HTMLDivElement | undefined;
  private textNode: HTMLDivElement | undefined;
  private cleanupCallbacks: Array<() => void> = [];
  private layoutObserver: ResizeObserver | undefined;
  private animationFrameId: number | undefined;
  private videoFrameCallbackId: number | undefined;
  private lastText = "";
  private visible = true;

  attach(media: HTMLMediaElement, initialCues: readonly SubtitleCue[] = []): void {
    if (this.media !== media) {
      this.detachMediaListeners();
      this.media = media;
      this.addMediaListeners(media);
    }
    this.cues = normalizeOverlayCues(initialCues);
    this.ensureHost();
    this.repositionForFullscreen();
    this.renderAt(media.currentTime);
    if (!media.paused && !media.ended) {
      this.startClock();
    }
  }

  setCues(cues: readonly SubtitleCue[]): void {
    this.cues = normalizeOverlayCues(cues);
    if (this.media) {
      this.renderAt(this.media.currentTime);
    }
  }

  /** Adds a bounded generated-cue page without dropping already-rendered pages. */
  appendCues(cues: readonly SubtitleCue[]): boolean {
    const merged = mergeOverlayCues(this.cues, cues);
    if (!merged) {
      return false;
    }
    this.cues = merged;
    if (this.media) {
      this.renderAt(this.media.currentTime);
    }
    return true;
  }

  setVisible(visible: boolean): void {
    this.visible = visible;
    if (this.host) {
      this.host.style.display = visible ? "block" : "none";
    }
  }

  destroy(): void {
    this.stopClock();
    this.detachMediaListeners();
    this.media = undefined;
    this.cues = [];
    this.lastText = "";
    this.host?.remove();
    this.host = undefined;
    this.textNode = undefined;
  }

  private addMediaListeners(media: HTMLMediaElement): void {
    const onSync = (): void => this.renderAt(media.currentTime);
    const onLayout = (): void => {
      this.repositionForFullscreen();
      this.renderAt(media.currentTime);
    };
    const onPlay = (): void => {
      this.renderAt(media.currentTime);
      this.startClock();
    };
    const onPause = (): void => {
      this.stopClock();
      this.renderAt(media.currentTime);
    };
    const onEnded = (): void => {
      this.stopClock();
      this.renderAt(media.currentTime);
    };
    const onFullscreenChange = onLayout;

    for (const eventName of ["timeupdate", "seeking", "seeked", "loadedmetadata", "ratechange", "emptied"]) {
      media.addEventListener(eventName, onSync);
      this.cleanupCallbacks.push(() => media.removeEventListener(eventName, onSync));
    }
    media.addEventListener("play", onPlay);
    media.addEventListener("pause", onPause);
    media.addEventListener("ended", onEnded);
    document.addEventListener("fullscreenchange", onFullscreenChange);
    window.addEventListener("resize", onLayout);
    // Capture scrolls from player containers as well as the page. This keeps a
    // fixed overlay exactly over media instead of lagging at viewport bottom.
    window.addEventListener("scroll", onLayout, true);
    this.layoutObserver = new ResizeObserver(onLayout);
    this.layoutObserver.observe(media);
    this.cleanupCallbacks.push(
      () => media.removeEventListener("play", onPlay),
      () => media.removeEventListener("pause", onPause),
      () => media.removeEventListener("ended", onEnded),
      () => document.removeEventListener("fullscreenchange", onFullscreenChange),
      () => window.removeEventListener("resize", onLayout),
      () => window.removeEventListener("scroll", onLayout, true)
    );
  }

  private detachMediaListeners(): void {
    for (const cleanup of this.cleanupCallbacks.splice(0)) {
      cleanup();
    }
    this.layoutObserver?.disconnect();
    this.layoutObserver = undefined;
    this.stopClock();
  }

  private ensureHost(): void {
    if (this.host && this.textNode) {
      return;
    }
    const host = document.createElement("div");
    host.dataset.subtitlerOverlay = "true";
    host.setAttribute("aria-hidden", "false");
    Object.assign(host.style, {
      position: "fixed",
      inset: "auto",
      zIndex: "2147483647",
      pointerEvents: "none",
      display: this.visible ? "block" : "none"
    });

    const shadow = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = `
      :host { all: initial; }
      .subtitler-cue {
        position: absolute;
        left: 50%;
        bottom: 11%;
        transform: translateX(-50%);
        max-width: min(88vw, 980px);
        padding: 0.28em 0.6em;
        border-radius: 0.28em;
        color: #fff;
        background: rgba(0, 0, 0, 0.76);
        font: 600 clamp(18px, 2.5vw, 34px)/1.28 system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
        text-align: center;
        text-wrap: balance;
        white-space: pre-line;
        text-shadow: 0 1px 2px rgba(0, 0, 0, 0.9);
      }
      .subtitler-cue:empty { display: none; }
    `;
    const cue = document.createElement("div");
    cue.className = "subtitler-cue";
    cue.setAttribute("role", "status");
    cue.setAttribute("aria-live", "polite");
    cue.setAttribute("aria-atomic", "true");
    shadow.append(style, cue);
    this.host = host;
    this.textNode = cue;
  }

  private repositionForFullscreen(): void {
    if (!this.host || !this.media) {
      return;
    }
    const fullscreenElement = document.fullscreenElement;
    const isMediaFullscreen = Boolean(fullscreenElement && (fullscreenElement === this.media || fullscreenElement.contains(this.media)));
    const parent = isMediaFullscreen ? fullscreenElement : document.body ?? document.documentElement;
    if (!parent) {
      return;
    }
    if (this.host.parentElement !== parent) {
      parent.append(this.host);
    }
    if (isMediaFullscreen) {
      Object.assign(this.host.style, {
        position: "absolute",
        inset: "0",
        left: "",
        top: "",
        width: "",
        height: ""
      });
      return;
    }
    const bounds = this.media.getBoundingClientRect();
    Object.assign(this.host.style, {
      position: "fixed",
      inset: "auto",
      left: `${Math.round(bounds.left)}px`,
      top: `${Math.round(bounds.top)}px`,
      width: `${Math.round(bounds.width)}px`,
      height: `${Math.round(bounds.height)}px`
    });
  }

  private renderAt(timeSeconds: number): void {
    const text = cueDisplayText(findActiveCue(this.cues, timeSeconds));
    if (text === this.lastText) {
      return;
    }
    this.lastText = text;
    if (this.textNode) {
      this.textNode.textContent = text;
    }
  }

  private startClock(): void {
    this.stopClock();
    const media = this.media;
    if (!media || media.paused || media.ended) {
      return;
    }
    if (media instanceof HTMLVideoElement && typeof (media as VideoFrameCapable).requestVideoFrameCallback === "function") {
      const video = media as VideoFrameCapable;
      const tick = (): void => {
        if (!this.media || this.media !== media || media.paused || media.ended) {
          return;
        }
        this.renderAt(media.currentTime);
        this.videoFrameCallbackId = video.requestVideoFrameCallback?.(tick);
      };
      this.videoFrameCallbackId = video.requestVideoFrameCallback(tick);
      return;
    }

    const tick = (): void => {
      if (!this.media || this.media !== media || media.paused || media.ended) {
        return;
      }
      this.renderAt(media.currentTime);
      this.animationFrameId = requestAnimationFrame(tick);
    };
    this.animationFrameId = requestAnimationFrame(tick);
  }

  private stopClock(): void {
    if (this.animationFrameId !== undefined) {
      cancelAnimationFrame(this.animationFrameId);
      this.animationFrameId = undefined;
    }
    const media = this.media as VideoFrameCapable | undefined;
    if (this.videoFrameCallbackId !== undefined && media?.cancelVideoFrameCallback) {
      media.cancelVideoFrameCallback(this.videoFrameCallbackId);
    }
    this.videoFrameCallbackId = undefined;
  }
}
