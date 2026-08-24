import type { SubtitleCue } from "../shared/domain";
import { cueDisplayText, findActiveCue, mergeOverlayCues, normalizeOverlayCues } from "./timeline";

type VideoFrameCapable = HTMLVideoElement & {
  requestVideoFrameCallback?: (callback: () => void) => number;
  cancelVideoFrameCallback?: (id: number) => void;
};

export const SUBTITLER_OVERLAY_ROOT_ATTRIBUTE = "data-subtitler-overlay-root";
export const SUBTITLER_OVERLAY_OWNER_ATTRIBUTE = "data-subtitler-overlay-owner";
export const SUBTITLER_ACTIVE_OVERLAY_OWNER_ATTRIBUTE = "data-subtitler-active-overlay-owner";
// Builds before the ownership invariant used this marker. Keep recognizing it
// so an unpacked-extension reload cannot leave that old, closed-shadow root
// visible beside the new owner.
export const SUBTITLER_LEGACY_OVERLAY_ATTRIBUTE = "data-subtitler-overlay";

let overlayOwnerSequence = 0;

/**
 * A content script can be re-created while an older isolated-world instance
 * still has page event handlers. This identifier is intentionally unique
 * across those instances; a simple module counter would collide after reload.
 */
export function createOverlayOwnerId(): string {
  overlayOwnerSequence += 1;
  const random = globalThis.crypto?.randomUUID?.() ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
  return `subtitler-overlay-${random}-${overlayOwnerSequence}`;
}

/**
 * Pure ownership rule shared by DOM reconciliation and regression tests.
 * One controller-owned host is retained; every other Subtitler root is stale,
 * even if a malformed/reloaded instance reused an owner value.
 */
export function staleOverlayRoots<T>(roots: readonly T[], retainedRoot: T | undefined): T[] {
  return roots.filter((root) => root !== retainedRoot);
}

/**
 * A page-local overlay that is driven by the HTMLMediaElement clock. It never
 * captures or reads audio; it only renders timestamped cues supplied by native
 * processing or an already-authorized TextTrack.
 */
export class SubtitleOverlayController {
  private readonly ownerId = createOverlayOwnerId();
  private media: HTMLMediaElement | undefined;
  private cues: SubtitleCue[] = [];
  private host: HTMLDivElement | undefined;
  private textNode: HTMLDivElement | undefined;
  private cleanupCallbacks: Array<() => void> = [];
  private layoutObserver: ResizeObserver | undefined;
  private ownershipObserver: MutationObserver | undefined;
  private animationFrameId: number | undefined;
  private videoFrameCallbackId: number | undefined;
  private lastText = "";
  private visible = true;

  attach(media: HTMLMediaElement, initialCues: readonly SubtitleCue[] = []): void {
    // Claim before attaching listeners. A later content-script instance wins
    // and removes stale page roots; an older instance then retires itself on
    // its next media/layout callback instead of re-adding its old host.
    this.claimDocumentOwnership();
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
    if (!this.ensureCurrentOwnership()) {
      return;
    }
    this.cues = normalizeOverlayCues(cues);
    if (this.media) {
      this.renderAt(this.media.currentTime);
    }
  }

  /** Adds a bounded generated-cue page without dropping already-rendered pages. */
  appendCues(cues: readonly SubtitleCue[]): boolean {
    if (!this.ensureCurrentOwnership()) {
      return false;
    }
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
    if (!this.ensureCurrentOwnership()) {
      return;
    }
    this.visible = visible;
    if (this.host) {
      this.host.style.display = visible ? "block" : "none";
    }
  }

  destroy(): void {
    this.stopClock();
    this.detachMediaListeners();
    this.stopOwnershipObserver();
    this.media = undefined;
    this.cues = [];
    this.lastText = "";
    this.host?.remove();
    this.host = undefined;
    this.textNode = undefined;
    if (document.documentElement.getAttribute(SUBTITLER_ACTIVE_OVERLAY_OWNER_ATTRIBUTE) === this.ownerId) {
      document.documentElement.removeAttribute(SUBTITLER_ACTIVE_OVERLAY_OWNER_ATTRIBUTE);
    }
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
    if (!this.isCurrentOwner()) {
      return;
    }
    if (this.host && this.textNode) {
      return;
    }
    const host = document.createElement("div");
    host.setAttribute(SUBTITLER_OVERLAY_ROOT_ATTRIBUTE, "true");
    host.setAttribute(SUBTITLER_OVERLAY_OWNER_ATTRIBUTE, this.ownerId);
    host.setAttribute(SUBTITLER_LEGACY_OVERLAY_ATTRIBUTE, "true");
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
    if (!this.ensureCurrentOwnership() || !this.host || !this.media) {
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
        // Some YouTube fullscreen wrappers retain a much wider document
        // layout than the visible player. Absolute 50% positioning would put
        // a cue at that off-screen layout midpoint. Fixed viewport geometry
        // keeps the one host centered in the actual fullscreen surface.
        position: "fixed",
        inset: "auto",
        left: "0px",
        top: "0px",
        width: "100vw",
        height: "100vh"
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
    if (!this.ensureCurrentOwnership()) {
      return;
    }
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
    if (!this.ensureCurrentOwnership()) {
      return;
    }
    this.stopClock();
    const media = this.media;
    if (!media || media.paused || media.ended) {
      return;
    }
    if (media instanceof HTMLVideoElement && typeof (media as VideoFrameCapable).requestVideoFrameCallback === "function") {
      const video = media as VideoFrameCapable;
      const tick = (): void => {
        if (!this.ensureCurrentOwnership() || !this.media || this.media !== media || media.paused || media.ended) {
          return;
        }
        this.renderAt(media.currentTime);
        this.videoFrameCallbackId = video.requestVideoFrameCallback?.(tick);
      };
      this.videoFrameCallbackId = video.requestVideoFrameCallback(tick);
      return;
    }

    const tick = (): void => {
      if (!this.ensureCurrentOwnership() || !this.media || this.media !== media || media.paused || media.ended) {
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

  /** Establish this controller as the sole page-visible Subtitler owner. */
  private claimDocumentOwnership(): void {
    document.documentElement.setAttribute(SUBTITLER_ACTIVE_OVERLAY_OWNER_ATTRIBUTE, this.ownerId);
    this.observeDocumentOwnership();
    this.removeStaleOverlayRoots();
  }

  /**
   * A pre-ownership build can keep a detached host in its controller and
   * append it again on a fullscreen or SPA-layout event. One observer owned by
   * the current controller makes that revival harmless without touching page
   * elements that are not explicitly marked as Subtitler-owned.
   */
  private observeDocumentOwnership(): void {
    if (this.ownershipObserver || typeof MutationObserver === "undefined") {
      return;
    }
    this.ownershipObserver = new MutationObserver(() => {
      if (!this.isCurrentOwner()) {
        this.stopOwnershipObserver();
        return;
      }
      this.removeStaleOverlayRoots();
    });
    this.ownershipObserver.observe(document.documentElement, {
      subtree: true,
      childList: true,
      attributes: true,
      attributeFilter: [SUBTITLER_ACTIVE_OVERLAY_OWNER_ATTRIBUTE]
    });
  }

  private stopOwnershipObserver(): void {
    this.ownershipObserver?.disconnect();
    this.ownershipObserver = undefined;
  }

  private removeStaleOverlayRoots(): void {
    const roots = Array.from(
      document.querySelectorAll<HTMLElement>(
        `[${SUBTITLER_OVERLAY_ROOT_ATTRIBUTE}="true"], [${SUBTITLER_LEGACY_OVERLAY_ATTRIBUTE}="true"]`
      )
    );
    for (const root of staleOverlayRoots(roots, this.host)) {
      // A controller can safely retain only its own current host. Every
      // orphaned/reloaded host is a Subtitler-owned root and is removed.
      root.remove();
    }
  }

  private isCurrentOwner(): boolean {
    return document.documentElement.getAttribute(SUBTITLER_ACTIVE_OVERLAY_OWNER_ATTRIBUTE) === this.ownerId;
  }

  /** Old handlers become inert after a newer controller claims the document. */
  private ensureCurrentOwnership(): boolean {
    if (this.isCurrentOwner()) {
      return true;
    }
    this.detachMediaListeners();
    this.stopOwnershipObserver();
    this.media = undefined;
    this.cues = [];
    this.lastText = "";
    this.host?.remove();
    this.host = undefined;
    this.textNode = undefined;
    return false;
  }
}
