import {
  failure,
  success,
  type CaptionTrackDescriptor,
  type JobFailure,
  type JobRecord,
  type MediaDetectionResult,
  type MediaSnapshot,
  type Result,
  type SubtitleCue,
  type TranscriptSegment
} from "../shared/domain";
import { chooseUsableCaptionTrack } from "../shared/captions";
import {
  isContentPlaybackUpdate,
  isPopupRequest,
  isSafeLocalFilePageUrl,
  sanitizeDirectMediaUrl,
  sanitizeLocalFilePath,
  type ContentPlaybackUpdate,
  type ContentRequest,
  type ContentResponse,
  type EngineConnectionStateUpdate,
  type NativeInboundMessage,
  type NativeStartJobPayload,
  type PopupRequest,
  type PopupResponse,
  type PopupTranscriptPage
} from "../shared/protocol";
import {
  OFFSCREEN_EXPORT_DOCUMENT_PATH,
  isOffscreenExportBlobExpiredEvent
} from "../shared/export-download-protocol";
import { EngineConnectionStateStore } from "./engine-state";
import { createChromeExportDownloadApi, ExportDownloadCoordinator } from "./export-downloads";
import { JobStore, jobsForTab, type NewJob } from "./job-store";
import { NativeClient } from "./native-client";
import { findActiveGeneratedSubtitleJob } from "./playback-routing";
import { prepareTranscriptExport } from "./transcript-export";
import { discoverYoutubeCaptionTracks, loadYoutubeCaptionCues } from "./youtube-caption-bridge";
import { classifyRecordingPageUrl } from "../platforms/recording-platforms";
import {
  TRANSCRIPT_POPUP_PAGE_LIMIT,
  TranscriptResultStore,
  type TranscriptPage
} from "./transcript-store";

const jobStore = new JobStore();
const nativeClient = new NativeClient();
const engineConnectionState = new EngineConnectionStateStore();
/** Transcript content remains only in this extension service-worker memory. */
const transcriptResults = new TranscriptResultStore();
const exportDownloadApi = createChromeExportDownloadApi();
/** No export state or Blob URL is persisted across a service-worker restart. */
const exportDownloads = exportDownloadApi ? new ExportDownloadCoordinator(exportDownloadApi) : undefined;
const initialized = jobStore.initialize();
let nativeMessageChain: Promise<void> = Promise.resolve();

nativeClient.onMessage((message) => {
  // A transcript page must reach the transient result store before its
  // matching terminal event makes the result available to the popup.
  nativeMessageChain = nativeMessageChain
    .then(() => handleNativeMessage(message), () => handleNativeMessage(message))
    .catch(() => undefined);
});
nativeClient.onDisconnect(() => {
  transcriptResults.discardIncomplete();
  engineConnectionState.markDisconnected();
  publishEngineConnectionState();
  void markDisconnectedJobsRecovering();
});

// A service-worker restart deliberately retains no transcript text, but it can
// safely reconnect a persisted opaque native job ID to the host's own private
// checkpoint/export bundle. This never resends a media URL or browser session.
void initialized.then(() => reconcilePersistedJobs()).catch(() => undefined);

chrome.runtime.onInstalled.addListener(() => {
  void initialized.catch(() => undefined);
});

chrome.runtime.onStartup.addListener(() => {
  void initialized.catch(() => undefined);
});

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (sender.id !== chrome.runtime.id) {
    return false;
  }
  if (isOffscreenExportBlobExpiredEvent(message)) {
    // A popup or content context cannot release an export Blob: only our
    // bundled offscreen document may send this opaque-TTL notification.
    if (sender.url !== chrome.runtime.getURL(OFFSCREEN_EXPORT_DOCUMENT_PATH)) {
      return false;
    }
    exportDownloads?.handleOffscreenBlobExpired(message.requestId);
    return false;
  }
  if (isContentPlaybackUpdate(message)) {
    void handleContentPlaybackUpdate(message, sender);
    return false;
  }
  if (!isPopupRequest(message)) {
    return false;
  }
  void handlePopupRequest(message)
    .then((response) => sendResponse(response))
    .catch(() => sendResponse(failure("UNKNOWN", "Subtitler could not complete that request.")));
  return true;
});

async function handlePopupRequest(request: PopupRequest): Promise<PopupResponse> {
  try {
    await initialized;
  } catch {
    return failure("UNKNOWN", "Subtitler could not restore its local job state.");
  }

  switch (request.type) {
    case "popup.detect-media": {
      const active = await getActiveMedia();
      return active.ok ? success(active.data.detection) : active;
    }
    case "popup.get-jobs":
      reconcilePersistedJobs();
      return success(await jobsForActiveTab());
    case "popup.get-engine-state":
      return success(engineConnectionState.snapshot());
    case "popup.get-transcript":
      return getTranscriptPage(request);
    case "popup.export-transcript":
      return exportTranscript(request);
    case "popup.start-job":
      return startJob(request);
    case "popup.stop-job":
      return stopJob(request.jobId);
  }
}

/**
 * The popup can ask for a completed result only in bounded pages. No segment
 * text is copied into chrome.storage, and an old persisted job record alone
 * cannot make a transcript survive a service-worker restart.
 */
function getTranscriptPage(
  request: Extract<PopupRequest, { type: "popup.get-transcript" }>
): Result<PopupTranscriptPage> {
  const job = jobStore.get(request.jobId);
  if (!job || job.kind !== "transcript") {
    return failure("UNKNOWN", "That transcript is no longer available.");
  }
  if (job.status !== "completed") {
    return failure("UNKNOWN", "This transcript is still being created.");
  }
  const cursor = request.cursor ?? 0;
  const limit = request.limit ?? TRANSCRIPT_POPUP_PAGE_LIMIT;
  const page: TranscriptPage | undefined = transcriptResults.getPage(job.id, cursor, limit);
  if (!page) {
    return failure(
      "UNKNOWN",
      "This transcript is not available in this browser session. Create it again to view it."
    );
  }
  return success(page);
}

/**
 * The only route that materializes a browser download. A popup click has
 * already passed strict request validation; the result cache never leaves the
 * background except through the private offscreen Blob bridge.
 */
async function exportTranscript(
  request: Extract<PopupRequest, { type: "popup.export-transcript" }>
): Promise<PopupResponse> {
  const job = jobStore.get(request.jobId);
  if (!job || job.kind !== "transcript") {
    return failure("UNKNOWN", "That transcript is no longer available.");
  }
  if (job.status !== "completed") {
    return failure("UNKNOWN", "This transcript is still being created.");
  }
  if (!exportDownloads) {
    return failure("UNKNOWN", "This version of Chrome cannot create local transcript downloads.");
  }
  const result = transcriptResults.getCompletedResult(job.id);
  if (!result) {
    return failure(
      "UNKNOWN",
      "This transcript is not available in this browser session. Create it again to export it."
    );
  }
  const prepared = prepareTranscriptExport(request.format, result);
  if (!prepared.ok) {
    switch (prepared.reason) {
      case "subtitle_cues_unavailable":
        return failure(
          "UNKNOWN",
          "Subtitler did not receive final subtitle cues for this recording. TXT and JSON exports are still available."
        );
      case "subtitle_cues_invalid":
        return failure("UNKNOWN", "Subtitler received invalid final subtitle timing and will not create a subtitle export.");
      case "too_large":
        return failure("UNKNOWN", "This transcript is too large to package safely for a browser download.");
    }
  }
  const started = await exportDownloads.start(prepared.value);
  if (!started.ok) {
    return failure("UNKNOWN", started.message);
  }
  return success({ downloadId: started.downloadId, filename: prepared.value.filename });
}

async function startJob(request: Extract<PopupRequest, { type: "popup.start-job" }>): Promise<Result<JobRecord>> {
  if (request.pastedUrl) {
    return startPastedUrlJob(request);
  }

  const active = await getActiveMedia();
  if (!active.ok) {
    return failure(active.error.code, active.error.message);
  }
  const { tabId, detection, pageUrl } = active.data;
  if (detection.state === "none") {
    return failure("NO_MEDIA", "No compatible HTML5 media was found. Paste an accessible recording URL instead.");
  }
  const media = detection.media;

  if (request.jobKind === "subtitle" && media.captionTracks.length > 0 && !request.forceGenerate) {
    return startExistingCaptionOverlay(tabId, media, pageUrl);
  }
  if (media.protectedMedia) {
    return failure(
      "PROTECTED_MEDIA",
      "This recording is protected by browser media security. Subtitler will not bypass DRM, encryption, or access controls."
    );
  }
  if (media.source.kind === "local_file") {
    return startNativeJob({
      jobKind: request.jobKind,
      ...(request.forceGenerate ? { forceGenerateWithSubtitler: true } : {}),
      tabId,
      mediaId: media.id,
      mediaKind: media.mediaKind,
      durationSeconds: media.durationSeconds,
      initialPlayback: initialPlaybackFromMedia(media),
      source: { kind: "local_file", path: media.source.path },
      localFilePageUrl: pageUrl
    });
  }
  if (media.source.kind === "direct") {
    return startNativeJob({
      jobKind: request.jobKind,
      ...(request.forceGenerate ? { forceGenerateWithSubtitler: true } : {}),
      tabId,
      mediaId: media.id,
      mediaKind: media.mediaKind,
      durationSeconds: media.durationSeconds,
      initialPlayback: initialPlaybackFromMedia(media),
      source: { kind: "direct_url", mediaUrl: media.source.url }
    });
  }
  const platform = pageUrl ? classifyRecordingPageUrl(pageUrl) : undefined;
  if (platform?.id === "youtube" && platform.knownRecordingPath && pageUrl) {
    return startNativeJob({
      jobKind: request.jobKind,
      ...(request.forceGenerate ? { forceGenerateWithSubtitler: true } : {}),
      tabId,
      mediaId: media.id,
      mediaKind: media.mediaKind,
      durationSeconds: media.durationSeconds,
      initialPlayback: initialPlaybackFromMedia(media),
      source: { kind: "page", pageUrl }
    });
  }
  if ((platform?.id === "webex" || platform?.id === "zoom") && platform.knownRecordingPath) {
    const mediaUrl =
      (platform.id === "webex" && pageUrl ? await resolveWebexAuthorizedMedia(tabId, pageUrl) : undefined) ??
      (await discoverLoadedDirectMedia(tabId));
    if (mediaUrl) {
      return startNativeJob({
        jobKind: request.jobKind,
        ...(request.forceGenerate ? { forceGenerateWithSubtitler: true } : {}),
        tabId,
        mediaId: media.id,
        mediaKind: media.mediaKind,
        durationSeconds: media.durationSeconds,
        initialPlayback: initialPlaybackFromMedia(media),
        source: { kind: "direct_url", mediaUrl }
      });
    }
    return failure(
      "UNSUPPORTED_MEDIA",
      "Subtitler can see this recording, but its player has not exposed a safe direct audio or video file yet. It will not copy browser credentials or bypass media protections."
    );
  }
  return failure(
    "UNSUPPORTED_MEDIA",
    platform?.opaqueSourceGuidance ??
      "Subtitler cannot obtain an accessible direct media stream from this player. It will not copy credentials or bypass browser protections."
  );
}

interface WebexRecordingReference {
  siteName: string;
  recordingId: string;
}

/**
 * Resolves only Webex's documented same-origin recording stream metadata.
 * The code runs in the page's main world so the currently authorized browser
 * session sends its own narrow site cookies. No cookie value, response body,
 * or playback credential leaves the page; the returned value is limited to a
 * safe direct audio/video URL for immediate local processing.
 */
async function resolveWebexAuthorizedMedia(tabId: number, pageUrl: string): Promise<string | undefined> {
  const reference = parseWebexRecordingReference(pageUrl);
  if (!reference) {
    return undefined;
  }
  try {
    const results = await chrome.scripting.executeScript({
      target: { tabId },
      world: "MAIN",
      func: async (recordingId: string, siteName: string): Promise<string[]> => {
        const endpoint = `/webappng/api/v1/recordings/${encodeURIComponent(recordingId)}/stream?siteurl=${encodeURIComponent(siteName)}`;
        const response = await fetch(endpoint, {
          credentials: "include",
          cache: "no-store",
          redirect: "error",
          headers: { Accept: "application/json" }
        });
        if (!response.ok) {
          return [];
        }
        const text = await response.text();
        if (text.length > 512 * 1024) {
          return [];
        }
        let value: unknown;
        try {
          value = JSON.parse(text);
        } catch {
          return [];
        }
        const urls: string[] = [];
        const seen = new Set<string>();
        const visit = (candidate: unknown, depth: number, mediaHint: boolean): void => {
          if (depth > 7 || urls.length >= 12) {
            return;
          }
          if (typeof candidate === "string") {
            if (!mediaHint || candidate.length > 16_384) {
              return;
            }
            try {
              const url = new URL(candidate, location.origin);
              const path = url.pathname.toLowerCase();
              if (
                url.protocol === "https:" &&
                !url.username &&
                !url.password &&
                /\.(mp4|m4a|webm|mp3|aac|ogg|wav)$/.test(path) &&
                !seen.has(url.href)
              ) {
                seen.add(url.href);
                urls.push(url.href);
              }
            } catch {
              // Untrusted metadata is ignored rather than interpreted.
            }
            return;
          }
          if (Array.isArray(candidate)) {
            for (const item of candidate) {
              visit(item, depth + 1, mediaHint);
            }
            return;
          }
          if (!candidate || typeof candidate !== "object") {
            return;
          }
          for (const [key, item] of Object.entries(candidate)) {
            const nextHint = mediaHint || /(?:audio|video|mp4|download|stream|media|file)/i.test(key);
            visit(item, depth + 1, nextHint);
          }
        };
        visit(value, 0, false);
        return urls;
      },
      args: [reference.recordingId, reference.siteName]
    });
    const candidates = results[0]?.result;
    if (!Array.isArray(candidates)) {
      return undefined;
    }
    return candidates
      .map((candidate) => sanitizeDirectMediaUrl(candidate))
      .find((candidate): candidate is string => candidate !== null);
  } catch {
    return undefined;
  }
}

function parseWebexRecordingReference(value: string): WebexRecordingReference | undefined {
  try {
    const url = new URL(value);
    if (url.protocol !== "https:" || url.username || url.password || !url.hostname.endsWith(".webex.com")) {
      return undefined;
    }
    const match = /^\/webappng\/sites\/([a-z0-9-]+)\/recording\/([a-f0-9]{32})\/playback$/i.exec(url.pathname);
    if (!match) {
      return undefined;
    }
    const siteName = match[1]?.toLowerCase();
    const recordingId = match[2]?.toLowerCase();
    if (!siteName || !recordingId || url.hostname.toLowerCase() !== `${siteName}.webex.com`) {
      return undefined;
    }
    return { siteName, recordingId };
  } catch {
    return undefined;
  }
}

/**
 * Keeps page-observed signed media URLs transient: the content script reads
 * only existing resource timing names; the background validates the one it
 * immediately passes to native. It never reads cookies or persists URLs.
 */
async function discoverLoadedDirectMedia(tabId: number): Promise<string | undefined> {
  const response = await sendToContent<ContentResponse>(tabId, {
    type: "content.get-network-media-candidates"
  }).catch(() => undefined);
  if (!response?.ok || !("mediaUrls" in response.data) || !Array.isArray(response.data.mediaUrls)) {
    return undefined;
  }
  return response.data.mediaUrls
    .map((candidate) => sanitizeDirectMediaUrl(candidate))
    .find((candidate): candidate is string => candidate !== null);
}

async function startPastedUrlJob(
  request: Extract<PopupRequest, { type: "popup.start-job" }>
): Promise<Result<JobRecord>> {
  const sourceUrl = sanitizeDirectMediaUrl(request.pastedUrl);
  if (!sourceUrl) {
    return failure("INVALID_URL", "Paste a valid HTTPS recording URL without embedded credentials.");
  }
  const platform = classifyRecordingPageUrl(sourceUrl);
  if (platform.knownRecordingPath) {
    return failure(
      "UNSUPPORTED_MEDIA",
      `Open this ${platform.displayName} recording in Chrome first so Subtitler can inspect its visible player. ${platform.opaqueSourceGuidance}`
    );
  }
  return startNativeJob({
    jobKind: request.jobKind,
    ...(request.forceGenerate ? { forceGenerateWithSubtitler: true } : {}),
    mediaKind: "video",
    source: { kind: "direct_url", mediaUrl: sourceUrl }
  });
}

interface NativeJobInputBase {
  jobKind: "subtitle" | "transcript";
  forceGenerateWithSubtitler?: boolean;
  tabId?: number;
  mediaId?: string;
  mediaKind: "video" | "audio";
  durationSeconds?: number | null;
  /** The detector's in-memory playhead is forwarded only for native subtitle scheduling. */
  initialPlayback?: {
    positionSeconds: number;
    isPaused: boolean;
  };
  /**
   * Used only by the local-source branch as a second, privileged origin
   * check. It is never placed in the native payload or persisted state.
   */
  localFilePageUrl?: string | undefined;
}

/**
 * The detector owns this ephemeral metadata. Keep the native start payload
 * bounded and avoid sending it for transcript jobs, where playback must not
 * influence complete-file processing.
 */
function initialPlaybackFromMedia(media: MediaSnapshot): NonNullable<NativeJobInputBase["initialPlayback"]> {
  const positionSeconds = Number.isFinite(media.currentTimeSeconds) && media.currentTimeSeconds >= 0 ? media.currentTimeSeconds : 0;
  return {
    positionSeconds,
    isPaused: !media.playing || media.ended
  };
}

type NativeJobInput =
  | (NativeJobInputBase & {
      source: { kind: "page"; pageUrl: string };
    })
  | (NativeJobInputBase & {
      source: { kind: "direct_url"; mediaUrl: string };
    })
  | (NativeJobInputBase & {
      source: { kind: "local_file"; path: string };
    });

async function startNativeJob(input: NativeJobInput): Promise<Result<JobRecord>> {
  let nativeSource: NativeStartJobPayload["source"];
  if (input.source.kind === "page") {
    const pageUrl = sanitizeDirectMediaUrl(input.source.pageUrl);
    if (!pageUrl) {
      return failure("INVALID_URL", "Subtitler cannot use this recording page safely.");
    }
    nativeSource = { kind: "page", pageUrl, mediaKind: input.mediaKind };
  } else if (input.source.kind === "direct_url") {
    const sourceUrl = sanitizeDirectMediaUrl(input.source.mediaUrl);
    if (!sourceUrl) {
      return failure("INVALID_URL", "Subtitler cannot use this media URL safely.");
    }
    nativeSource = { kind: "direct_url", mediaUrl: sourceUrl, mediaKind: input.mediaKind };
  } else {
    // Recheck both the source path and its page origin at the privileged
    // extension-to-native boundary. A remote page must never choose a path for
    // the local host to read.
    if (!isSafeLocalFilePage(input.localFilePageUrl)) {
      return failure(
        "UNSUPPORTED_MEDIA",
        "Open the local media file from a local file page in Chrome before asking Subtitler to process it."
      );
    }
    const path = sanitizeLocalFilePath(input.source.path);
    if (!path) {
      return failure("UNSUPPORTED_MEDIA", "Subtitler cannot use this local media path safely.");
    }
    nativeSource = { kind: "local_file", path, mediaKind: input.mediaKind };
  }

  const jobId = crypto.randomUUID();
  const newJob: NewJob = {
    id: jobId,
    kind: input.jobKind
  };
  if (input.tabId !== undefined) {
    newJob.tabId = input.tabId;
  }
  if (input.mediaId) {
    newJob.mediaId = input.mediaId;
  }
  if (input.durationSeconds !== null && input.durationSeconds !== undefined) {
    newJob.mediaDurationSeconds = input.durationSeconds;
  }
  const record = await jobStore.create(newJob);

  if (input.jobKind === "subtitle" && input.tabId !== undefined && input.mediaId) {
    const overlay = await sendToContent<ContentResponse>(input.tabId, {
      type: "content.start-overlay",
      mediaId: input.mediaId,
      mode: "generated",
      cues: []
    });
    if (!overlay.ok) {
      const failed = await failJob(jobId, {
        code: "UNSUPPORTED_MEDIA",
        message: overlay.error.message,
        retryable: false
      });
      return failed ? success(failed) : failure(overlay.error.code, overlay.error.message);
    }
  }

  if (input.durationSeconds !== null && input.durationSeconds !== undefined) {
    nativeSource.durationSeconds = input.durationSeconds;
  }
  const nativePayload: NativeStartJobPayload = {
    jobId,
    jobKind: input.jobKind,
    source: nativeSource
  };
  if (input.jobKind === "subtitle" && input.initialPlayback) {
    nativePayload.initialPlayback = input.initialPlayback;
  }
  if (input.forceGenerateWithSubtitler) {
    nativePayload.forceGenerateWithSubtitler = true;
  }

  try {
    await jobStore.update(jobId, { status: "connecting" });
    nativeClient.startJob(nativePayload);
    return success((await jobStore.get(jobId)) ?? record);
  } catch {
    const failed = await failJob(jobId, {
      code: "COMPANION_UNAVAILABLE",
      message: "Subtitler needs its local processing engine. Install or restart the Subtitler Engine, then try again.",
      retryable: true
    });
    return failed ? success(failed) : failure("COMPANION_UNAVAILABLE", "Subtitler Engine is unavailable.");
  }
}

/**
 * The pathname is deliberately discarded: this check only establishes that
 * the active tab is a local, non-UNC file document. No local path becomes
 * persisted extension state.
 */
function isSafeLocalFilePage(pageUrl: string | undefined): boolean {
  return isSafeLocalFilePageUrl(pageUrl);
}

async function startExistingCaptionOverlay(
  tabId: number,
  media: MediaSnapshot,
  pageUrl: string | undefined
): Promise<Result<JobRecord>> {
  const selectedTrack = chooseUsableCaptionTrack(media.captionTracks);
  if (selectedTrack?.provider === "youtube") {
    return startYoutubeCaptionOverlay(tabId, media, pageUrl, selectedTrack);
  }

  const record = await createExistingCaptionJob(tabId, media);
  const response = await sendToContent<ContentResponse>(tabId, {
    type: "content.start-overlay",
    mediaId: media.id,
    mode: "existing"
  });
  if (!response.ok) {
    const failed = await failJob(record.id, {
      code: "UNSUPPORTED_MEDIA",
      message: response.error.message,
      retryable: false
    });
    return failed ? success(failed) : failure(response.error.code, response.error.message);
  }
  return success(record);
}

/**
 * Existing YouTube captions are fetched only from the active page's bounded
 * timed-text endpoint. Their text is sent straight to the current page in
 * 200-cue messages; neither the endpoint nor the cue body is persisted or
 * forwarded to the native engine.
 */
async function startYoutubeCaptionOverlay(
  tabId: number,
  media: MediaSnapshot,
  pageUrl: string | undefined,
  track: CaptionTrackDescriptor
): Promise<Result<JobRecord>> {
  const loaded = await loadYoutubeCaptionCues(tabId, pageUrl, track);
  if (!loaded.ok) {
    // Some pages visibly render captions but do not provide a usable timedtext
    // body. Use only that currently visible page text; do not fetch media,
    // retain caption content, or claim a full transcript/export.
    if (loaded.reason === "caption_response_invalid" || loaded.reason === "caption_cues_unavailable") {
      return startVisibleYoutubeCaptionOverlay(tabId, media);
    }
    return failure(
      "UNSUPPORTED_MEDIA",
      youtubeCaptionFailureMessage(loaded.reason)
    );
  }
  const cues = loaded.cues;

  const record = await createExistingCaptionJob(tabId, media);
  const overlay = await sendToContent<ContentResponse>(tabId, {
    type: "content.start-overlay",
    mediaId: media.id,
    // The page overlay accepts supplied, page-local cues in generated mode;
    // this remains an existing-caption job and never starts a native worker.
    mode: "generated",
    cues: []
  });
  if (!overlay.ok) {
    const failed = await failJob(record.id, {
      code: "UNSUPPORTED_MEDIA",
      message: overlay.error.message,
      retryable: false
    });
    return failed ? success(failed) : failure(overlay.error.code, overlay.error.message);
  }

  for (const page of chunkSubtitleCues(cues, 200)) {
    const appended = await sendToContent<ContentResponse>(tabId, {
      type: "content.append-overlay-cues",
      mediaId: media.id,
      cues: page
    });
    if (!appended.ok) {
      const failed = await failJob(record.id, {
        code: "UNSUPPORTED_MEDIA",
        message: appended.error.message,
        retryable: false
      });
      return failed ? success(failed) : failure(appended.error.code, appended.error.message);
    }
  }
  return success(record);
}

/**
 * Page-only fallback for visible YouTube captions. Caption text remains in
 * the active content-script overlay and never reaches storage or native code.
 */
async function startVisibleYoutubeCaptionOverlay(tabId: number, media: MediaSnapshot): Promise<Result<JobRecord>> {
  const record = await createExistingCaptionJob(tabId, media);
  const overlay = await sendToContent<ContentResponse>(tabId, {
    type: "content.start-overlay",
    mediaId: media.id,
    mode: "generated",
    cues: [],
    visibleCaptionFallback: true
  });
  if (!overlay.ok) {
    const failed = await failJob(record.id, {
      code: "UNSUPPORTED_MEDIA",
      message: overlay.error.message,
      retryable: false
    });
    return failed ? success(failed) : failure(overlay.error.code, overlay.error.message);
  }
  return success(record);
}

function youtubeCaptionFailureMessage(
  reason:
    | "invalid_caption_track"
    | "caption_metadata_unavailable"
    | "caption_endpoint_unavailable"
    | "caption_fetch_failed"
    | "caption_http_rejected"
    | "caption_redirect_rejected"
    | "caption_response_invalid"
    | "caption_cues_unavailable"
): string {
  switch (reason) {
    case "caption_cues_unavailable":
      return "Subtitler found YouTube caption metadata, but this page did not return usable timestamped captions.";
    case "caption_response_invalid":
      return "Subtitler found YouTube captions, but the page returned an unusable caption response.";
    case "caption_fetch_failed":
      return "Subtitler found YouTube captions, but this browser session could not retrieve them from YouTube.";
    case "caption_http_rejected":
      return "Subtitler found YouTube captions, but YouTube rejected the caption request from this page.";
    case "caption_redirect_rejected":
      return "Subtitler found YouTube captions, but its safe caption request could not follow YouTube's redirect.";
    case "invalid_caption_track":
    case "caption_metadata_unavailable":
    case "caption_endpoint_unavailable":
      return "Subtitler found YouTube captions, but they are no longer available from this authorized page. Try again or choose Generate with Subtitler when an accessible media stream is available.";
  }
}

async function createExistingCaptionJob(tabId: number, media: MediaSnapshot): Promise<JobRecord> {
  const jobId = crypto.randomUUID();
  const newJob: NewJob = {
    id: jobId,
    kind: "subtitle",
    tabId,
    mediaId: media.id,
    usesExistingCaptions: true
  };
  if (media.durationSeconds !== null) {
    newJob.mediaDurationSeconds = media.durationSeconds;
  }
  return jobStore.create(newJob);
}

async function stopJob(jobId: string): Promise<Result<JobRecord>> {
  const job = jobStore.get(jobId);
  if (!job) {
    return failure("UNKNOWN", "That Subtitler job no longer exists.");
  }
  if (job.kind === "transcript") {
    // A user stop is also a privacy action for the transient readable result.
    // Any late native page is ignored by NativeClient after its fetch stops.
    transcriptResults.discard(job.id);
  }
  if (job.tabId !== undefined && job.mediaId) {
    await sendToContent<ContentResponse>(job.tabId, {
      type: "content.stop-playback-observation",
      mediaId: job.mediaId,
      jobId: job.id
    }).catch(() => undefined);
    await sendToContent<ContentResponse>(job.tabId, { type: "content.stop-overlay", mediaId: job.mediaId }).catch(() => undefined);
  }
  if (!job.usesExistingCaptions && job.status !== "completed" && job.status !== "failed") {
    try {
      nativeClient.stopJob(job.id, job.nativeJobId);
    } catch {
      // The local host may already have exited; the persisted state is still updated below.
    }
  }
  const stopped = await jobStore.update(job.id, { status: "stopped" });
  return stopped ? success(stopped) : failure("UNKNOWN", "Subtitler could not update this job.");
}

interface ActiveMedia {
  tabId: number;
  detection: MediaDetectionResult;
  pageUrl?: string;
}

/**
 * Popup status is scoped to the current tab.  A retained job from a different
 * recording must not make this tab claim that an unrelated transcript is
 * ready, nor offer a route to display it here.
 */
async function jobsForActiveTab(): Promise<JobRecord[]> {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (tab?.id === undefined) {
    return [];
  }
  return jobsForTab(jobStore.list(), tab.id);
}

async function getActiveMedia(): Promise<Result<ActiveMedia>> {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (tab?.id === undefined) {
    return failure("PAGE_UNAVAILABLE", "Subtitler could not access the active browser tab.");
  }
  try {
    await chrome.scripting.executeScript({ target: { tabId: tab.id }, files: ["content.js"] });
  } catch {
    return failure("PAGE_UNAVAILABLE", "This browser page does not allow Subtitler to inspect media.");
  }
  const response = await sendToContent<ContentResponse>(tab.id, { type: "content.get-media" });
  if (!response.ok) {
    return failure(response.error.code, response.error.message);
  }
  if (!isMediaDetectionResult(response.data)) {
    return failure("PAGE_UNAVAILABLE", "Subtitler received an unexpected response from this page.");
  }
  const detection = await enrichDetectedMediaWithYoutubeCaptions(tab.id, tab.url, response.data);
  const active: ActiveMedia = { tabId: tab.id, detection };
  if (typeof tab.url === "string") {
    active.pageUrl = tab.url;
  }
  return success(active);
}

/**
 * A YouTube `video` normally has an opaque `blob:` source even when the page
 * exposes usable existing captions. Enrich only this ephemeral detection
 * result, without storing caption endpoints or player-response data.
 */
async function enrichDetectedMediaWithYoutubeCaptions(
  tabId: number,
  pageUrl: string | undefined,
  detection: MediaDetectionResult
): Promise<MediaDetectionResult> {
  if (detection.state !== "detected") {
    return detection;
  }
  const youtubeTracks = await discoverYoutubeCaptionTracks(tabId, pageUrl);
  if (youtubeTracks.length === 0) {
    return detection;
  }
  const existingIds = new Set(detection.media.captionTracks.map((track) => track.id));
  const captionTracks = [
    ...detection.media.captionTracks,
    ...youtubeTracks.filter((track) => !existingIds.has(track.id))
  ];
  return {
    ...detection,
    media: {
      ...detection.media,
      captionTracks
    }
  };
}

function* chunkSubtitleCues(cues: readonly SubtitleCue[], size: number): Generator<SubtitleCue[]> {
  for (let index = 0; index < cues.length; index += size) {
    yield cues.slice(index, index + size);
  }
}

async function sendToContent<T>(tabId: number, request: ContentRequest): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    chrome.tabs.sendMessage(tabId, request, (response: T) => {
      const error = chrome.runtime.lastError;
      if (error) {
        reject(new Error(error.message));
        return;
      }
      resolve(response);
    });
  });
}

async function handleNativeMessage(message: NativeInboundMessage): Promise<void> {
  await initialized;
  switch (message.type) {
    case "engine.ready":
      engineConnectionState.markReady(message.payload.localProcessingAvailable, message.payload.localProcessingAdvisory);
      publishEngineConnectionState();
      return;
    case "job.accepted": {
      const accepted = await jobStore.update(message.payload.jobId, {
        status: "processing",
        nativeJobId: message.payload.nativeJobId
      });
      if (accepted?.kind === "transcript" && accepted.status !== "stopped") {
        transcriptResults.begin(accepted.id);
      }
      if (accepted && accepted.status !== "stopped") {
        await startGeneratedSubtitlePlaybackObservation(accepted);
      }
      return;
    }
    case "job.progress":
      await jobStore.update(message.payload.jobId, { status: message.payload.status, progress: message.payload.progress });
      return;
    case "job.subtitle-cues": {
      await handOffGeneratedSubtitleCues(message.payload.jobId, message.payload.cues);
      return;
    }
    case "job.transcript-segments":
      await handOffTranscriptSegments(message.payload.jobId, message.payload.segments);
      return;
    case "job.transcript-cues":
      await handOffTranscriptCues(message.payload.jobId, message.payload.cues);
      return;
    case "job.completed": {
      const current = jobStore.get(message.payload.jobId);
      // Do not let late terminal traffic make a stopped/failed transcript
      // readable again. NativeClient emits pages before this event, and this
      // queue preserves that order in the service worker.
      if (!current || current.status === "stopped" || current.status === "failed") {
        return;
      }
      if (current.kind === "transcript") {
        transcriptResults.complete(current.id);
      }
      await jobStore.update(message.payload.jobId, { status: "completed" });
      await stopGeneratedSubtitlePlaybackObservation(message.payload.jobId);
      return;
    }
    case "job.cancelled": {
      const current = jobStore.get(message.payload.jobId);
      if (current?.kind === "transcript") {
        transcriptResults.discard(current.id);
      }
      await jobStore.update(message.payload.jobId, { status: "stopped" });
      await stopGeneratedSubtitlePlaybackObservation(message.payload.jobId);
      return;
    }
    case "job.stale": {
      const current = jobStore.get(message.payload.jobId);
      if (current?.kind === "transcript") {
        transcriptResults.discard(current.id);
      }
      await jobStore.update(message.payload.jobId, {
        status: "stale",
        error: {
          code: "NATIVE_ERROR",
          message: message.payload.message,
          retryable: true
        }
      });
      await stopGeneratedSubtitlePlaybackObservation(message.payload.jobId);
      return;
    }
    case "job.failed":
      await failJob(message.payload.jobId, {
        code: message.payload.code,
        message: message.payload.message,
        retryable: message.payload.retryable
      });
      return;
  }
}

/**
 * A popup may be open while the native handshake completes. Broadcast only the
 * validated, coarse connection state so it can update without polling. The
 * state also remains available through `popup.get-engine-state` for a popup
 * opened after the connection is already ready.
 */
function publishEngineConnectionState(): void {
  const event: EngineConnectionStateUpdate = {
    type: "engine.connection-state",
    payload: engineConnectionState.snapshot()
  };
  try {
    chrome.runtime.sendMessage(event, () => {
      // Chrome sets lastError when the popup is closed. Reading it prevents an
      // expected "receiving end does not exist" warning without retaining it.
      void chrome.runtime.lastError;
    });
  } catch {
    // A service-worker shutdown or absent extension receiver must not affect a
    // native job; the state is still available on the next popup request.
  }
}

/**
 * The content script reports only bounded playback metadata. Match it to the
 * same tab and media element as an active generated-subtitle job before it can
 * reach native messaging; existing captions and transcript-only jobs never use
 * this route.
 */
async function handleContentPlaybackUpdate(message: ContentPlaybackUpdate, sender: chrome.runtime.MessageSender): Promise<void> {
  try {
    await initialized;
  } catch {
    return;
  }
  const tabId = sender.tab?.id;
  if (tabId === undefined) {
    return;
  }
  const job = findActiveGeneratedSubtitleJob(jobStore.list(), {
    tabId,
    jobId: message.jobId,
    mediaId: message.mediaId
  });
  if (!job) {
    return;
  }
  nativeClient.updatePlayback(job.id, {
    positionMs: message.positionMs,
    playbackRateMilli: message.playbackRateMilli,
    isPaused: message.isPaused,
    seekGeneration: message.seekGeneration
  });
}

/** Observation begins only after the native engine has accepted a generated subtitle job. */
async function startGeneratedSubtitlePlaybackObservation(job: JobRecord): Promise<void> {
  if (job.kind !== "subtitle" || job.usesExistingCaptions === true || job.tabId === undefined || !job.mediaId) {
    return;
  }
  await sendToContent<ContentResponse>(job.tabId, {
    type: "content.start-playback-observation",
    mediaId: job.mediaId,
    jobId: job.id
  }).catch(() => undefined);
}

async function stopGeneratedSubtitlePlaybackObservation(jobId: string): Promise<void> {
  const job = jobStore.get(jobId);
  if (!job || job.kind !== "subtitle" || job.usesExistingCaptions === true || job.tabId === undefined || !job.mediaId) {
    return;
  }
  await sendToContent<ContentResponse>(job.tabId, {
    type: "content.stop-playback-observation",
    mediaId: job.mediaId,
    jobId: job.id
  }).catch(() => undefined);
}

/**
 * NativeClient emits each bounded native cue page. The content controller
 * appends/deduplicates pages in page memory, so long recordings never put a
 * full transcript-sized cue list in chrome.storage or one extension message.
 */
async function handOffGeneratedSubtitleCues(jobId: string, cues: readonly SubtitleCue[]): Promise<void> {
  const job = jobStore.get(jobId);
  if (
    !job ||
    job.kind !== "subtitle" ||
    job.status === "stopped" ||
    job.status === "failed" ||
    job.tabId === undefined ||
    !job.mediaId
  ) {
    return;
  }
  await sendToContent<ContentResponse>(job.tabId, {
    type: "content.append-overlay-cues",
    mediaId: job.mediaId,
    cues: [...cues]
  }).catch(() => undefined);
}

/**
 * Page events are admitted only for an active full-transcript job. The store
 * has a defensive process-memory cap; exceeding it fails the view cleanly
 * rather than persisting or retaining an arbitrary amount of user speech.
 */
async function handOffTranscriptSegments(
  jobId: string,
  segments: ReadonlyArray<TranscriptSegment>
): Promise<void> {
  const job = jobStore.get(jobId);
  if (!job || job.kind !== "transcript" || job.status === "stopped" || job.status === "failed") {
    return;
  }
  const appended = transcriptResults.append(jobId, segments);
  if (appended.ok || appended.reason === "inactive") {
    return;
  }
  await failJob(jobId, {
    code: "NATIVE_ERROR",
    message: "This transcript is too large to display safely in the current Subtitler session.",
    retryable: false
  });
}

/**
 * Final export cues are only admitted for the still-active completed
 * transcript fetch. TranscriptResultStore validates chronology and bounds the
 * combined in-memory text before any completion event exposes the result.
 */
async function handOffTranscriptCues(jobId: string, cues: ReadonlyArray<SubtitleCue>): Promise<void> {
  const job = jobStore.get(jobId);
  if (!job || job.kind !== "transcript" || job.status === "stopped" || job.status === "failed") {
    return;
  }
  const appended = transcriptResults.appendCues(jobId, cues);
  if (appended.ok || appended.reason === "inactive") {
    return;
  }
  await failJob(jobId, {
    code: "NATIVE_ERROR",
    message:
      appended.reason === "invalid"
        ? "Subtitler received invalid final subtitle timing for this transcript."
        : "This transcript is too large to prepare safely in the current Subtitler session.",
    retryable: false
  });
}

async function failJob(jobId: string, error: JobFailure): Promise<JobRecord | undefined> {
  const existing = jobStore.get(jobId);
  if (existing?.kind === "transcript") {
    transcriptResults.discard(existing.id);
  }
  const job = await jobStore.update(jobId, { status: "failed", error });
  if (job?.tabId !== undefined && job.mediaId) {
    await stopGeneratedSubtitlePlaybackObservation(job.id);
    await sendToContent<ContentResponse>(job.tabId, { type: "content.stop-overlay", mediaId: job.mediaId }).catch(() => undefined);
  }
  return job;
}

async function markDisconnectedJobsRecovering(): Promise<void> {
  await initialized;
  transcriptResults.discardIncomplete();
  const activeStatuses = new Set(["connecting", "processing", "buffering"]);
  const interrupted = jobStore
    .list()
    .filter((job) => activeStatuses.has(job.status) && !job.usesExistingCaptions);
  await Promise.all(
    interrupted.map(async (job) => {
      await jobStore.update(job.id, { status: "recovering" });
      await stopGeneratedSubtitlePlaybackObservation(job.id);
    })
  );
}

function reconcilePersistedJobs(): void {
  const recoverable = jobStore
    .list()
    .filter(
      (job) =>
        job.nativeJobId !== undefined &&
        !job.usesExistingCaptions &&
        ["recovering", "connecting", "processing", "buffering"].includes(job.status)
    )
    .map((job) => ({
      clientJobId: job.id,
      nativeJobId: job.nativeJobId as string,
      kind: job.kind
    }));
  nativeClient.reconcileJobs(recoverable);
}

function isMediaDetectionResult(value: unknown): value is MediaDetectionResult {
  if (typeof value !== "object" || value === null || !("state" in value)) {
    return false;
  }
  return value.state === "detected" || value.state === "none";
}
