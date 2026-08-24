import { chooseUsableCaptionTrack, normalizeCaptionCues } from "../shared/captions";
import { failure, success, type MediaDetectionResult, type Result, type SubtitleCue } from "../shared/domain";
import {
  isContentRequest,
  sanitizeDirectMediaUrl,
  type ContentPlaybackUpdate,
  type ContentRequest,
  type ContentResponse,
  type PlaybackUpdateSnapshot
} from "../shared/protocol";
import { SubtitleOverlayController } from "../overlay/controller";
import { MediaRegistry } from "./detector";
import { PlaybackObserver } from "./playback-observer";

interface ExistingTrackBinding {
  track: TextTrack;
  originalMode: TextTrackMode;
  onCueChange: () => void;
  onMediaReady: () => void;
}

interface VisibleCaptionBinding {
  /** Watches only the caption surface once YouTube creates it. */
  captionObserver: MutationObserver;
  /** Temporary, low-work watcher while YouTube has not rendered captions yet. */
  availabilityObserver: MutationObserver;
  media: HTMLMediaElement;
  onMediaEnd: () => void;
  onSeeking: () => void;
  lastText: string;
  lastCue: SubtitleCue | undefined;
  nextCueId: number;
  hiddenNodes: Map<HTMLElement, string>;
  captionHost: HTMLElement | undefined;
}

interface PlaybackObservation {
  jobId: string;
  mediaId: string;
  observer: PlaybackObserver;
}

/** Owns all page-side state. No recording, cookie, or session data is persisted here. */
export class ContentController {
  private readonly registry = new MediaRegistry();
  private readonly overlay = new SubtitleOverlayController();
  private overlayMediaId: string | undefined;
  private overlayMode: "existing" | "generated" | undefined;
  private existingTrackBinding: ExistingTrackBinding | undefined;
  private visibleCaptionBinding: VisibleCaptionBinding | undefined;
  private playbackObservation: PlaybackObservation | undefined;

  handleMessage(value: unknown): ContentResponse {
    if (!isContentRequest(value)) {
      return failure("PAGE_UNAVAILABLE", "Subtitler received an invalid page request.");
    }
    return this.handleRequest(value);
  }

  private handleRequest(request: ContentRequest): ContentResponse {
    switch (request.type) {
      case "content.get-media":
        return success(this.registry.detect());
      case "content.get-network-media-candidates":
        return success({ mediaUrls: this.collectNetworkMediaCandidates() });
      case "content.start-overlay":
        return this.startOverlay(request.mediaId, request.mode, request.cues ?? [], request.visibleCaptionFallback === true);
      case "content.set-overlay-cues":
        return this.setOverlayCues(request.mediaId, request.cues);
      case "content.append-overlay-cues":
        return this.appendOverlayCues(request.mediaId, request.cues);
      case "content.start-playback-observation":
        return this.startPlaybackObservation(request.mediaId, request.jobId);
      case "content.stop-playback-observation":
        this.stopPlaybackObservation(request.mediaId, request.jobId);
        return success({ observing: false });
      case "content.stop-overlay":
        this.stopOverlay(request.mediaId);
        return success({ stopped: true });
    }
  }

  /**
   * Returns only a small list of direct file-looking requests already made by
   * this page. This does not inspect cookies, headers, request bodies, or
   * browser cache and does not initiate any network request.
   */
  private collectNetworkMediaCandidates(): string[] {
    const candidates: string[] = [];
    const seen = new Set<string>();
    const add = (value: string): void => {
      const safeUrl = sanitizeDirectMediaUrl(value);
      if (!safeUrl || seen.has(safeUrl) || !looksLikeDirectMediaUrl(safeUrl)) {
        return;
      }
      seen.add(safeUrl);
      candidates.push(safeUrl);
    };
    for (const entry of performance.getEntriesByType("resource")) {
      if (!(entry instanceof PerformanceResourceTiming) || !looksLikeDirectMediaResource(entry)) {
        continue;
      }
      add(entry.name);
      if (candidates.length === 12) {
        return candidates;
      }
    }
    // Some recording players initialise a signed descriptor in an inert JSON
    // script before creating a media request. Inspect only bounded inline page
    // data; this deliberately excludes cookies, JavaScript globals, request
    // headers, browser storage, and all response bodies.
    for (const text of recordingDescriptorText()) {
      for (const url of extractHttpsUrls(text)) {
        add(url);
        if (candidates.length === 12) {
          return candidates;
        }
      }
    }
    return candidates;
  }

  private startOverlay(
    mediaId: string,
    mode: "existing" | "generated",
    suppliedCues: readonly SubtitleCue[],
    visibleCaptionFallback: boolean
  ): ContentResponse {
    const media = this.registry.findMedia(mediaId);
    if (!media) {
      return failure("NO_MEDIA", "The selected media is no longer available on this page.");
    }
    this.stopPlaybackObservation();
    this.clearExistingCaptionBinding();
    this.clearVisibleCaptionBinding();
    this.overlayMediaId = mediaId;
    this.overlayMode = mode;

    if (mode === "existing") {
      const snapshot = this.registry.snapshot(media);
      const track = chooseUsableCaptionTrack(snapshot.captionTracks);
      const textTrack = track ? this.registry.findTextTrack(mediaId, track.id) : undefined;
      if (!textTrack) {
        this.overlayMediaId = undefined;
        this.overlayMode = undefined;
        return failure("NO_MEDIA", "No usable existing captions are currently available for this media.");
      }
      this.bindExistingCaptions(media, textTrack);
      this.overlay.attach(media, readCaptionCues(textTrack));
      return success({ started: true });
    }

    this.overlay.attach(media, suppliedCues);
    if (visibleCaptionFallback) {
      this.bindVisibleYoutubeCaptions(media);
    }
    return success({ started: true });
  }

  /**
   * Generated subtitle jobs need only playback metadata for the native
   * scheduler. Existing captions never activate this channel.
   */
  private startPlaybackObservation(mediaId: string, jobId: string): ContentResponse {
    if (this.overlayMediaId !== mediaId || this.overlayMode !== "generated") {
      return failure("NO_MEDIA", "Subtitler cannot observe playback because the generated subtitle overlay is no longer active.");
    }
    const media = this.registry.findMedia(mediaId);
    if (!media) {
      return failure("NO_MEDIA", "The selected media is no longer available on this page.");
    }

    this.stopPlaybackObservation();
    const observer = new PlaybackObserver(media, (snapshot) => reportPlaybackSnapshot(mediaId, jobId, snapshot));
    this.playbackObservation = { mediaId, jobId, observer };
    observer.start();
    return success({ observing: true });
  }

  private setOverlayCues(mediaId: string, cues: readonly SubtitleCue[]): ContentResponse {
    if (this.overlayMediaId !== mediaId || !this.registry.findMedia(mediaId)) {
      return failure("NO_MEDIA", "The selected media is no longer available on this page.");
    }
    this.overlay.setCues(cues);
    return success({ started: true });
  }

  /**
   * Generated cues arrive in bounded native pages. Merge by stable cue ID so
   * duplicate or out-of-order delivery cannot erase an earlier page.
   */
  private appendOverlayCues(mediaId: string, cues: readonly SubtitleCue[]): ContentResponse {
    if (this.overlayMediaId !== mediaId || !this.registry.findMedia(mediaId)) {
      return failure("NO_MEDIA", "The selected media is no longer available on this page.");
    }
    if (!this.overlay.appendCues(cues)) {
      return failure("PAGE_UNAVAILABLE", "This recording has too many subtitle cues for the browser overlay.");
    }
    return success({ started: true });
  }

  private stopOverlay(mediaId?: string): void {
    if (mediaId && this.overlayMediaId && mediaId !== this.overlayMediaId) {
      return;
    }
    this.stopPlaybackObservation(mediaId);
    this.clearExistingCaptionBinding();
    this.clearVisibleCaptionBinding();
    this.overlay.destroy();
    this.overlayMediaId = undefined;
    this.overlayMode = undefined;
  }

  private stopPlaybackObservation(mediaId?: string, jobId?: string): void {
    const observation = this.playbackObservation;
    if (!observation || (mediaId && observation.mediaId !== mediaId) || (jobId && observation.jobId !== jobId)) {
      return;
    }
    observation.observer.stop();
    this.playbackObservation = undefined;
  }

  private bindExistingCaptions(media: HTMLMediaElement, track: TextTrack): void {
    const originalMode = track.mode;
    // `hidden` loads cues but avoids duplicate native captions behind our overlay.
    track.mode = "hidden";
    const syncCues = (): void => this.overlay.setCues(readCaptionCues(track));
    const onMediaReady = (): void => syncCues();
    track.addEventListener("cuechange", syncCues);
    media.addEventListener("loadeddata", onMediaReady);
    media.addEventListener("loadedmetadata", onMediaReady);
    this.existingTrackBinding = { track, originalMode, onCueChange: syncCues, onMediaReady };
  }

  private clearExistingCaptionBinding(): void {
    const binding = this.existingTrackBinding;
    if (!binding) {
      return;
    }
    binding.track.removeEventListener("cuechange", binding.onCueChange);
    const media = this.overlayMediaId ? this.registry.findMedia(this.overlayMediaId) : undefined;
    media?.removeEventListener("loadeddata", binding.onMediaReady);
    media?.removeEventListener("loadedmetadata", binding.onMediaReady);
    binding.track.mode = binding.originalMode;
    this.existingTrackBinding = undefined;
  }

  /**
   * Some YouTube pages render already-enabled captions in their player DOM but
   * do not expose a readable TextTrack or usable timedtext payload. This
   * bounded fallback observes only the visible caption text on that page and
   * maps it to the current media clock. It never fetches a URL, accesses a
   * player response, sends caption content through native messaging, or stores
   * it outside this page overlay.
  */
  private bindVisibleYoutubeCaptions(media: HTMLMediaElement): void {
    const segmentSelector = ".ytp-caption-segment";
    const binding: VisibleCaptionBinding = {
      captionObserver: new MutationObserver(() => sync()),
      availabilityObserver: new MutationObserver(() => bindCaptionHost()),
      media,
      onMediaEnd: () => closeLastCue(),
      onSeeking: () => {
        closeLastCue();
        binding.lastText = "";
      },
      lastText: "",
      lastCue: undefined,
      nextCueId: 1,
      hiddenNodes: new Map(),
      captionHost: undefined
    };
    const readVisibleCaption = (): string => {
      const host = binding.captionHost;
      if (!host) {
        return "";
      }
      const nodes = Array.from(host.querySelectorAll<HTMLElement>(segmentSelector));
      for (const node of nodes) {
        if (!binding.hiddenNodes.has(node)) {
          binding.hiddenNodes.set(node, node.style.visibility);
        }
        node.style.visibility = "hidden";
      }
      return nodes
        .map((node) => node.textContent ?? "")
        .join(" ")
        .replace(/\s+/gu, " ")
        .trim()
        .slice(0, 2_000);
    };
    const closeLastCue = (): void => {
      const lastCue = binding.lastCue;
      if (!lastCue) {
        return;
      }
      const endSeconds = Math.max(lastCue.startSeconds + 0.05, media.currentTime);
      binding.lastCue = { ...lastCue, endSeconds };
      this.overlay.appendCues([binding.lastCue]);
      binding.lastCue = undefined;
    };
    const sync = (): void => {
      if (this.visibleCaptionBinding !== binding || media.ended) {
        return;
      }
      const text = readVisibleCaption();
      if (!text) {
        closeLastCue();
        binding.lastText = "";
        return;
      }
      if (text === binding.lastText) {
        return;
      }
      closeLastCue();
      const startSeconds = Math.max(0, media.currentTime);
      const cue: SubtitleCue = {
        id: `youtube-visible-${binding.nextCueId}`,
        startSeconds,
        // Until caption surface changes, keep a short safe window visible.
        endSeconds: startSeconds + 6,
        text
      };
      binding.nextCueId += 1;
      binding.lastText = text;
      binding.lastCue = cue;
      this.overlay.appendCues([cue]);
    };
    const bindCaptionHost = (): void => {
      if (this.visibleCaptionBinding !== binding || binding.captionHost) {
        return;
      }
      const host =
        document.querySelector<HTMLElement>("#ytp-caption-window-container") ??
        document.querySelector<HTMLElement>(".caption-window");
      if (!host) {
        return;
      }
      binding.captionHost = host;
      binding.availabilityObserver.disconnect();
      binding.captionObserver.observe(host, { childList: true, characterData: true, subtree: true });
      sync();
    };
    // YouTube often creates its caption surface after a video/ad transition.
    // Until then, do no caption work for unrelated page mutations.
    binding.availabilityObserver.observe(document.documentElement, { childList: true, subtree: true });
    media.addEventListener("ended", binding.onMediaEnd);
    media.addEventListener("seeking", binding.onSeeking);
    this.visibleCaptionBinding = binding;
    bindCaptionHost();
  }

  private clearVisibleCaptionBinding(): void {
    const binding = this.visibleCaptionBinding;
    if (!binding) {
      return;
    }
    binding.captionObserver.disconnect();
    binding.availabilityObserver.disconnect();
    binding.media.removeEventListener("ended", binding.onMediaEnd);
    binding.media.removeEventListener("seeking", binding.onSeeking);
    for (const [node, originalVisibility] of binding.hiddenNodes) {
      if (node.isConnected) {
        node.style.visibility = originalVisibility;
      }
    }
    this.visibleCaptionBinding = undefined;
  }
}

function looksLikeDirectMediaResource(entry: PerformanceResourceTiming): boolean {
  if (entry.initiatorType === "audio" || entry.initiatorType === "video") {
    return true;
  }
  return looksLikeDirectMediaUrl(entry.name);
}

function looksLikeDirectMediaUrl(value: string): boolean {
  try {
    const url = new URL(value);
    const path = url.pathname.toLowerCase();
    if (/\.(mp4|m4a|webm|mp3|aac|ogg|wav)$/.test(path)) {
      return true;
    }
    const mediaType = `${url.searchParams.get("mime") ?? ""} ${url.searchParams.get("type") ?? ""}`.toLowerCase();
    if (/\b(audio|video)\b/.test(mediaType)) {
      return true;
    }
    // Webex/Zoom descriptors are commonly signed routes rather than URLs
    // with a filename extension. Require both a media-ish route segment and
    // a bounded opaque token/query signal so ordinary API URLs are excluded.
    const route = `${url.hostname}${path}`.toLowerCase();
    const hasMediaRoute = /(?:recording|playback|media|stream|audio|video|download)/.test(route);
    const hasOpaqueAccess = ["token", "sig", "signature", "auth", "ticket", "expires", "jwt"]
      .some((key) => url.searchParams.has(key));
    return hasMediaRoute && hasOpaqueAccess;
  } catch {
    return false;
  }
}

function recordingDescriptorText(): string[] {
  const texts: string[] = [];
  let remaining = 512 * 1024;
  for (const script of document.querySelectorAll<HTMLScriptElement>('script[type="application/json"], script[type="application/ld+json"]')) {
    const text = script.textContent ?? "";
    if (!/recording|playback|media|stream|audio|video/i.test(text)) {
      continue;
    }
    const limited = text.slice(0, remaining);
    texts.push(limited);
    remaining -= limited.length;
    if (remaining <= 0) {
      break;
    }
  }
  return texts;
}

function extractHttpsUrls(text: string): string[] {
  const normalized = text.replaceAll("\\/", "/");
  const matches = normalized.match(/https:\/\/[^\s"'<>\\]+/g) ?? [];
  return matches.slice(0, 64);
}

/**
 * Content-script notifications are best-effort by design. A service-worker
 * restart or tab navigation simply drops a stale snapshot; no queue or media
 * content is retained in the page or extension storage.
 */
function reportPlaybackSnapshot(mediaId: string, jobId: string, snapshot: PlaybackUpdateSnapshot): void {
  const message: ContentPlaybackUpdate = {
    type: "content.playback-update",
    jobId,
    mediaId,
    ...snapshot
  };
  try {
    chrome.runtime.sendMessage(message, () => {
      // Reading lastError suppresses Chrome's expected no-receiver warning.
      void chrome.runtime.lastError;
    });
  } catch {
    // The service worker may be unavailable during a browser shutdown.
  }
}

function readCaptionCues(track: TextTrack): SubtitleCue[] {
  return normalizeCaptionCues(
    Array.from(track.cues ?? []).map((cue) => ({
      id: cue.id,
      startSeconds: cue.startTime,
      endSeconds: cue.endTime,
      text: typeof (cue as unknown as { text?: unknown }).text === "string" ? (cue as unknown as { text: string }).text : ""
    }))
  );
}
