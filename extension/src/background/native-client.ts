import {
  createNativeCancelRequest,
  createNativeGetSubtitleCuesRequest,
  createNativeGetTranscriptSegmentsRequest,
  createNativeHandshakeRequest,
  createNativePlaybackUpdateRequest,
  createNativeRestoreRequest,
  createNativeStartRequest,
  createNativeStatusRequest,
  NATIVE_HOST_NAME,
  parseNativeHostResponse,
  type NativeInboundMessage,
  type NativeHostResponse,
  type NativeSubtitleCue,
  type NativeTranscriptSegment,
  type NativeStartJobPayload,
  type PlaybackUpdateSnapshot
} from "../shared/protocol";
import type { JobProgress, SubtitleCue, TranscriptSegment } from "../shared/domain";

export type NativeMessageListener = (message: NativeInboundMessage) => void;
export type NativeDisconnectListener = (reason: string | undefined) => void;

const STATUS_POLL_INTERVAL_MS = 2_000;
/**
 * Native Messaging is normally request/response ordered. If Chrome loses the
 * restore reply while retaining the port, query the known opaque job ID once
 * instead of leaving the popup in recovery indefinitely.
 */
export const RESTORE_STATUS_FALLBACK_MS = 1_500;
/** Bounded page retrieval prevents a completed job from appearing active forever. */
export const RESULT_PAGE_RESPONSE_TIMEOUT_MS = 5_000;
const MAX_RESULT_PAGE_RETRIES = 2;
const SUBTITLE_CUE_PAGE_LIMIT = 200;
const TRANSCRIPT_SEGMENT_PAGE_LIMIT = 100;
/** A second, native-side-facing cap beyond the page observer's two-second cadence. */
export const PLAYBACK_UPDATE_MIN_INTERVAL_MS = 750;

/**
 * Minimal, typed wrapper around Chrome native messaging. The connection is made
 * only after an explicit job request; it deliberately has no cookie/session API.
 */
export class NativeClient {
  private port: chrome.runtime.Port | undefined;
  private readonly messageListeners = new Set<NativeMessageListener>();
  private readonly disconnectListeners = new Set<NativeDisconnectListener>();
  private readonly pendingRequests = new Map<string, PendingRequest>();
  private readonly nativeJobIds = new Map<string, string>();
  private readonly pollTimers = new Map<string, ReturnType<typeof setInterval>>();
  private readonly statusRequestsInFlight = new Set<string>();
  /** Tracks active pagination; cue text itself is immediately handed to the page overlay. */
  private readonly cueFetches = new Map<string, SubtitleCueFetch>();
  /**
   * Completed full transcripts are drained in bounded pages before a terminal
   * event. The background owns the transient result cache; NativeClient never
   * stores segment text itself.
   */
  private readonly transcriptFetches = new Map<string, TranscriptSegmentFetch>();
  private readonly transcriptSegmentRetryTimers = new Map<string, ReturnType<typeof setTimeout>>();
  /** Final cue pages for a completed transcript stay separate from overlay cue paging. */
  private readonly transcriptCueFetches = new Map<string, TranscriptCueFetch>();
  private readonly transcriptCueRetryTimers = new Map<string, ReturnType<typeof setTimeout>>();
  /** At most one lossy, metadata-only playback update is retained per client job. */
  private readonly queuedPlaybackUpdates = new Map<string, PlaybackUpdateSnapshot>();
  private readonly playbackUpdateTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private readonly playbackUpdateLastSentAt = new Map<string, number>();
  /** A start request waits for the native host's authoritative handshake. */
  private handshakeReady = false;
  private readonly queuedStarts = new Map<string, NativeStartJobPayload>();
  /** Recovery is metadata-only: persisted IDs plus job kind, never a source. */
  private readonly queuedRestores = new Map<string, RecoverableNativeJob>();
  private readonly restoreRequestsInFlight = new Set<string>();
  private readonly restoreStatusFallbackTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private readonly restoredJobKeys = new Set<string>();
  /**
   * User stops are terminal from the extension's point of view, even when the
   * native host has not returned its authoritative job ID yet. Keep this
   * tombstone until a delayed `job_started` can be cancelled at the host.
   */
  private readonly cancellationRequested = new Set<string>();

  onMessage(listener: NativeMessageListener): () => void {
    this.messageListeners.add(listener);
    return () => this.messageListeners.delete(listener);
  }

  onDisconnect(listener: NativeDisconnectListener): () => void {
    this.disconnectListeners.add(listener);
    return () => this.disconnectListeners.delete(listener);
  }

  startJob(payload: NativeStartJobPayload): void {
    this.ensureConnected();
    if (this.handshakeReady) {
      this.postStart(payload);
      return;
    }
    this.queuedStarts.set(payload.jobId, payload);
  }

  /**
   * Reconcile persisted extension metadata with the host's private checkpoint
   * after a service-worker/native-host restart. Completed transcript text is
   * fetched only through the existing bounded endpoints after this succeeds.
   */
  reconcileJobs(jobs: readonly RecoverableNativeJob[]): void {
    if (jobs.length === 0) {
      return;
    }
    this.ensureConnected();
    for (const job of jobs) {
      if (this.cancellationRequested.has(job.clientJobId)) {
        continue;
      }
      const recoveryKey = `${job.clientJobId}:${job.nativeJobId}`;
      if (this.restoredJobKeys.has(recoveryKey)) {
        continue;
      }
      this.nativeJobIds.set(job.clientJobId, job.nativeJobId);
      if (this.handshakeReady) {
        this.requestRestore(job);
      } else {
        this.queuedRestores.set(job.clientJobId, job);
      }
    }
  }

  stopJob(clientJobId: string, persistedNativeJobId?: string): void {
    this.cancellationRequested.add(clientJobId);
    this.queuedStarts.delete(clientJobId);
    this.stopPolling(clientJobId);
    this.stopCueFetch(clientJobId);
    this.stopTranscriptSegmentFetch(clientJobId);
    this.stopTranscriptCueFetch(clientJobId);
    this.clearPlaybackUpdate(clientJobId);
    this.ensureConnected();
    const nativeJobId = persistedNativeJobId ?? this.nativeJobIds.get(clientJobId);
    if (!nativeJobId) {
      return;
    }
    const request = createNativeCancelRequest(nativeJobId);
    this.postTracked(request, { kind: "cancel", clientJobId });
  }

  /**
   * Retain only the most recent scheduler hint. It can arrive before the native
   * start response; in that case it is sent after the job is accepted.
   */
  updatePlayback(clientJobId: string, snapshot: PlaybackUpdateSnapshot): void {
    if (this.cancellationRequested.has(clientJobId)) {
      return;
    }
    this.queuedPlaybackUpdates.set(clientJobId, { ...snapshot });
    const nativeJobId = this.nativeJobIds.get(clientJobId);
    if (nativeJobId) {
      this.schedulePlaybackUpdate(clientJobId, nativeJobId);
    }
  }

  private ensureConnected(): void {
    if (this.port) {
      return;
    }
    const port = chrome.runtime.connectNative(NATIVE_HOST_NAME);
    this.port = port;
    port.onMessage.addListener((value) => {
      this.handleHostResponse(value);
    });
    port.onDisconnect.addListener(() => {
      if (this.port !== port) {
        return;
      }
      this.port = undefined;
      this.handshakeReady = false;
      this.pendingRequests.clear();
      this.queuedStarts.clear();
      this.queuedRestores.clear();
      this.restoreRequestsInFlight.clear();
      this.clearAllRestoreStatusFallbacks();
      this.restoredJobKeys.clear();
      this.nativeJobIds.clear();
      this.stopAllPolling();
      this.cueFetches.clear();
      this.transcriptFetches.clear();
      this.clearAllTranscriptSegmentRetryTimers();
      this.transcriptCueFetches.clear();
      this.clearAllTranscriptCueRetryTimers();
      this.clearAllPlaybackUpdates();
      const reason = chrome.runtime.lastError?.message;
      for (const listener of this.disconnectListeners) {
        listener(reason);
      }
    });
    const request = createNativeHandshakeRequest();
    this.postTracked(request, { kind: "handshake" });
  }

  private postTracked(message: { request_id: string }, pending: PendingRequest): void {
    this.pendingRequests.set(message.request_id, pending);
    try {
      this.post(message);
    } catch (error) {
      this.pendingRequests.delete(message.request_id);
      throw error;
    }
  }

  private postStart(payload: NativeStartJobPayload): void {
    if (this.cancellationRequested.has(payload.jobId)) {
      return;
    }
    const request = createNativeStartRequest(payload);
    try {
      this.postTracked(request, { kind: "start", clientJobId: payload.jobId });
    } catch {
      this.notify({
        protocolVersion: 1,
        type: "job.failed",
        payload: {
          jobId: payload.jobId,
          code: "NATIVE_ERROR",
          message: "Subtitler could not start the local processing engine.",
          retryable: true
        }
      });
    }
  }

  private flushQueuedStarts(): void {
    const starts = [...this.queuedStarts.values()];
    this.queuedStarts.clear();
    for (const payload of starts) {
      this.postStart(payload);
    }
  }

  private failQueuedStarts(message: string, retryable: boolean): void {
    const starts = [...this.queuedStarts.values()];
    this.queuedStarts.clear();
    for (const payload of starts) {
      if (this.cancellationRequested.delete(payload.jobId)) {
        continue;
      }
      this.notify({
        protocolVersion: 1,
        type: "job.failed",
        payload: { jobId: payload.jobId, code: "NATIVE_ERROR", message, retryable }
      });
    }
  }

  private handleHostResponse(value: unknown): void {
    const response = parseNativeHostResponse(value);
    if (!response) {
      return;
    }
    const pending = response.request_id ? this.pendingRequests.get(response.request_id) : undefined;
    if (response.request_id) {
      this.pendingRequests.delete(response.request_id);
    }
    if (pending?.kind === "status" && pending.clientJobId) {
      this.statusRequestsInFlight.delete(pending.clientJobId);
    }

    switch (response.response) {
      case "handshake":
        if (pending?.kind === "handshake") {
          this.handshakeReady = true;
          const payload: Extract<NativeInboundMessage, { type: "engine.ready" }> ["payload"] = {
            engineVersion: response.native_version,
            localProcessingAvailable: response.capabilities.local_asr_available && response.capabilities.ffmpeg_available
          };
          if (response.capabilities.localProcessingAdvisory) {
            payload.localProcessingAdvisory = response.capabilities.localProcessingAdvisory;
          }
          this.notify({
            protocolVersion: 1,
            type: "engine.ready",
            payload
          });
          this.flushQueuedStarts();
          this.flushQueuedRestores();
        }
        return;
      case "job_started":
        if (pending?.kind === "start" && pending.clientJobId) {
          this.nativeJobIds.set(pending.clientJobId, response.job.job_id);
          if (this.cancellationRequested.has(pending.clientJobId)) {
            this.cancelAcceptedJob(pending.clientJobId, response.job.job_id, response.job.state);
            return;
          }
          this.notify({
            protocolVersion: 1,
            type: "job.accepted",
            payload: { jobId: pending.clientJobId, nativeJobId: response.job.job_id }
          });
          this.handleJobStatus(pending.clientJobId, response);
          if (!isTerminalState(response.job.state)) {
            this.schedulePlaybackUpdate(pending.clientJobId, response.job.job_id);
            this.startPolling(pending.clientJobId, response.job.job_id);
          }
        }
        return;
      case "job_restored":
        // Chrome can keep the service worker and native port alive while
        // losing only this in-memory request correlation. The persisted
        // browser job has already mapped the opaque native job ID before the
        // restore request is sent, so a valid response for that known ID is
        // still safe to reconcile. Do not discard a completed private result
        // merely because the request-id bookkeeping was interrupted.
        const restoredClientJobId =
          pending?.kind === "restore" && pending.clientJobId
            ? pending.clientJobId
            : this.clientJobIdFor(response.job.job_id);
        if (restoredClientJobId) {
          const recoveryKey = `${restoredClientJobId}:${response.job.job_id}`;
          if (pending?.kind !== "restore" && this.restoredJobKeys.has(recoveryKey)) {
            return;
          }
          this.restoreRequestsInFlight.delete(restoredClientJobId);
          this.clearRestoreStatusFallback(restoredClientJobId);
          this.nativeJobIds.set(restoredClientJobId, response.job.job_id);
          this.restoredJobKeys.add(recoveryKey);
          this.notify({
            protocolVersion: 1,
            type: "job.accepted",
            payload: { jobId: restoredClientJobId, nativeJobId: response.job.job_id }
          });
          this.handleJobStatus(restoredClientJobId, response);
        }
        return;
      case "job_status": {
        const clientJobId = this.clientJobIdFor(response.job.job_id);
        if (clientJobId) {
          if (this.cancellationRequested.has(clientJobId)) {
            if (isTerminalState(response.job.state)) {
              this.forgetStoppedJob(clientJobId);
            }
            return;
          }
          this.handleJobStatus(clientJobId, response);
        }
        return;
      }
      case "job_cancelled":
        if (pending?.kind === "cancel" && pending.clientJobId) {
          this.stopPolling(pending.clientJobId);
          this.stopCueFetch(pending.clientJobId);
          this.stopTranscriptSegmentFetch(pending.clientJobId);
          this.stopTranscriptCueFetch(pending.clientJobId);
          this.clearPlaybackUpdate(pending.clientJobId);
          this.nativeJobIds.delete(pending.clientJobId);
          this.cancellationRequested.delete(pending.clientJobId);
          this.notify({ protocolVersion: 1, type: "job.cancelled", payload: { jobId: pending.clientJobId } });
        }
        return;
      case "subtitle_cues":
        if (pending?.kind === "transcript-cues") {
          this.handleTranscriptCuePage(response, pending);
        } else {
          this.handleSubtitleCuePage(response, pending);
        }
        return;
      case "transcript_segments":
        this.handleTranscriptSegmentPage(response, pending);
        return;
      case "error":
        if (pending?.kind === "handshake") {
          this.failQueuedStarts(response.message, response.retryable);
        }
        if (pending?.kind === "start" && pending.clientJobId) {
          this.clearPlaybackUpdate(pending.clientJobId);
          if (this.cancellationRequested.delete(pending.clientJobId)) {
            this.nativeJobIds.delete(pending.clientJobId);
          } else {
            this.notify({
              protocolVersion: 1,
              type: "job.failed",
              payload: {
                jobId: pending.clientJobId,
                code: mapHostErrorCode(response.code),
                message: response.message,
                retryable: response.retryable
              }
            });
          }
        }
        if (pending?.kind === "status" && pending.clientJobId) {
          this.stopPolling(pending.clientJobId);
          this.clearPlaybackUpdate(pending.clientJobId);
          if (!this.cancellationRequested.has(pending.clientJobId)) {
            this.notify({
              protocolVersion: 1,
              type: "job.failed",
              payload: {
                jobId: pending.clientJobId,
                code: mapHostErrorCode(response.code),
                message: response.message,
                retryable: response.retryable
              }
            });
          }
        }
        if (pending?.kind === "restore" && pending.clientJobId) {
          this.restoreRequestsInFlight.delete(pending.clientJobId);
          this.clearRestoreStatusFallback(pending.clientJobId);
          this.notify({
            protocolVersion: 1,
            type: "job.stale",
            payload: {
              jobId: pending.clientJobId,
              message: "Subtitler could not reconnect this local job. Retry it to create a fresh local result."
            }
          });
        }
        if (pending?.kind === "cancel" && pending.clientJobId) {
          this.forgetStoppedJob(pending.clientJobId);
        }
        if (pending?.kind === "subtitle-cues" && pending.clientJobId) {
          const fetch = this.cueFetches.get(pending.clientJobId);
          if (fetch) {
            fetch.inFlight = false;
          }
          // A partial cue page can briefly be unavailable while the current
          // local chunk is being committed. Keep the job alive and retry from
          // the same cursor on the next status poll; terminal retrieval still
          // fails clearly because there will be no later progress poll.
          if (fetch?.terminal) {
            this.failSubtitleCueFetch(
              pending.clientJobId,
              "Subtitler could not retrieve generated subtitles from the local engine.",
              response.retryable
            );
          }
        }
        if (pending?.kind === "transcript-segments" && pending.clientJobId) {
          const fetch = this.transcriptFetches.get(pending.clientJobId);
          if (fetch && fetch.requestId === response.request_id) {
            this.clearTranscriptSegmentRetry(pending.clientJobId);
            fetch.inFlight = false;
            delete fetch.requestId;
          }
          this.failTranscriptSegmentFetch(
            pending.clientJobId,
            "Subtitler could not retrieve the completed transcript from the local engine.",
            response.retryable
          );
        }
        if (pending?.kind === "transcript-cues" && pending.clientJobId) {
          const fetch = this.transcriptCueFetches.get(pending.clientJobId);
          if (fetch && fetch.requestId === response.request_id) {
            this.clearTranscriptCueRetry(pending.clientJobId);
            fetch.inFlight = false;
            delete fetch.requestId;
          }
          this.failTranscriptCueFetch(
            pending.clientJobId,
            "Subtitler could not retrieve final subtitle cues from the local engine.",
            response.retryable
          );
        }
        return;
    }
  }

  private handleJobStatus(
    clientJobId: string,
    response: Extract<NativeHostResponse, { response: "job_started" | "job_status" | "job_restored" }>
  ): void {
    if (this.cancellationRequested.has(clientJobId)) {
      return;
    }
    switch (response.job.state) {
      case "completed":
        this.stopPolling(clientJobId);
        this.clearPlaybackUpdate(clientJobId);
        if (response.job.kind === "subtitle_generation") {
          this.startSubtitleCueFetch(clientJobId, response.job.job_id, true);
        } else {
          this.startTranscriptSegmentFetch(clientJobId, response.job.job_id);
        }
        return;
      case "cancelled":
        this.stopPolling(clientJobId);
        this.stopCueFetch(clientJobId);
        this.stopTranscriptSegmentFetch(clientJobId);
        this.stopTranscriptCueFetch(clientJobId);
        this.clearPlaybackUpdate(clientJobId);
        this.notify({ protocolVersion: 1, type: "job.cancelled", payload: { jobId: clientJobId } });
        return;
      case "stale":
        this.stopPolling(clientJobId);
        this.stopCueFetch(clientJobId);
        this.stopTranscriptSegmentFetch(clientJobId);
        this.stopTranscriptCueFetch(clientJobId);
        this.clearPlaybackUpdate(clientJobId);
        this.notify({
          protocolVersion: 1,
          type: "job.stale",
          payload: {
            jobId: clientJobId,
            message: response.job.message ?? "Subtitler's local worker stopped without completing this job."
          }
        });
        return;
      case "failed": {
        this.stopPolling(clientJobId);
        this.stopCueFetch(clientJobId);
        this.stopTranscriptSegmentFetch(clientJobId);
        this.stopTranscriptCueFetch(clientJobId);
        this.clearPlaybackUpdate(clientJobId);
        const failure = response.job.failure;
        this.notify({
          protocolVersion: 1,
          type: "job.failed",
          payload: {
            jobId: clientJobId,
            code: mapHostErrorCode(failure?.code ?? "internal"),
            message: failure?.message ?? response.job.message ?? "Subtitler could not complete this local job.",
            retryable: failure?.retryable ?? false
          }
        });
        return;
      }
      case "processing":
        if (response.job.kind === "subtitle_generation") {
          this.startSubtitleCueFetch(clientJobId, response.job.job_id, false);
        }
        this.notifyProgress(clientJobId, response);
        return;
      default:
        this.notifyProgress(clientJobId, response);
    }
  }

  /**
   * Fetch only pages that the host has already finalized. While a subtitle
   * job remains processing, an empty final-current page simply leaves the
   * cursor parked until the next status poll observes more completed work.
   */
  private startSubtitleCueFetch(clientJobId: string, nativeJobId: string, terminal: boolean): void {
    if (this.cancellationRequested.has(clientJobId)) {
      return;
    }
    let fetch = this.cueFetches.get(clientJobId);
    if (!fetch) {
      fetch = { nativeJobId, cursor: 0, inFlight: false, terminal };
      this.cueFetches.set(clientJobId, fetch);
    } else if (fetch.nativeJobId !== nativeJobId) {
      this.failSubtitleCueFetch(clientJobId, "Subtitler received subtitles for a different local job.", false);
      return;
    } else if (terminal) {
      fetch.terminal = true;
    }
    if (!fetch.inFlight) {
      this.requestSubtitleCuePage(clientJobId, nativeJobId, fetch.cursor);
    }
  }

  private requestSubtitleCuePage(clientJobId: string, nativeJobId: string, cursor: number): void {
    const fetch = this.cueFetches.get(clientJobId);
    if (!fetch || fetch.nativeJobId !== nativeJobId || fetch.inFlight) {
      return;
    }
    const request = createNativeGetSubtitleCuesRequest(
      nativeJobId,
      cursor === 0 ? { limit: SUBTITLE_CUE_PAGE_LIMIT } : { cursor, limit: SUBTITLE_CUE_PAGE_LIMIT }
    );
    fetch.inFlight = true;
    try {
      this.postTracked(request, { kind: "subtitle-cues", clientJobId, nativeJobId, cursor });
    } catch {
      fetch.inFlight = false;
      this.failSubtitleCueFetch(clientJobId, "Subtitler could not retrieve generated subtitles from the local engine.", true);
    }
  }

  private handleSubtitleCuePage(
    response: Extract<NativeHostResponse, { response: "subtitle_cues" }>,
    pending: PendingRequest | undefined
  ): void {
    if (pending?.kind !== "subtitle-cues" || !pending.clientJobId) {
      return;
    }
    const fetch = this.cueFetches.get(pending.clientJobId);
    if (!fetch) {
      return;
    }
    if (response.job_id !== pending.nativeJobId || fetch.nativeJobId !== response.job_id) {
      this.failSubtitleCueFetch(
        pending.clientJobId,
        "Subtitler received subtitle cues for a different local job.",
        false
      );
      return;
    }
    fetch.inFlight = false;

    if (response.cues.length > 0) {
      this.notify({
        protocolVersion: 1,
        type: "job.subtitle-cues",
        payload: {
          jobId: pending.clientJobId,
          cues: mapNativeSubtitleCues(response.cues, response.job_id, pending.cursor)
        }
      });
    }

    if (response.next_cursor === undefined) {
      fetch.cursor = pending.cursor + response.cues.length;
      if (fetch.terminal) {
        this.cueFetches.delete(pending.clientJobId);
        this.notify({ protocolVersion: 1, type: "job.completed", payload: { jobId: pending.clientJobId } });
      }
      return;
    }

    if (response.next_cursor <= pending.cursor) {
      this.failSubtitleCueFetch(pending.clientJobId, "Subtitler received an invalid subtitle-cue page from the local engine.", false);
      return;
    }
    fetch.cursor = response.next_cursor;
    this.requestSubtitleCuePage(pending.clientJobId, response.job_id, response.next_cursor);
  }

  private failSubtitleCueFetch(clientJobId: string, message: string, retryable: boolean): void {
    this.stopCueFetch(clientJobId);
    this.notify({
      protocolVersion: 1,
      type: "job.failed",
      payload: { jobId: clientJobId, code: "NATIVE_ERROR", message, retryable }
    });
  }

  /**
   * Full transcripts are immutable at this point: the native host only admits
   * this request after completion. Drain one bounded page at a time so no
   * unbounded native message or extension-side queue is created.
   */
  private startTranscriptSegmentFetch(clientJobId: string, nativeJobId: string): void {
    if (this.cancellationRequested.has(clientJobId)) {
      return;
    }
    let fetch = this.transcriptFetches.get(clientJobId);
    if (!fetch) {
      fetch = { nativeJobId, cursor: 0, inFlight: false, retryCount: 0 };
      this.transcriptFetches.set(clientJobId, fetch);
    } else if (fetch.nativeJobId !== nativeJobId) {
      this.failTranscriptSegmentFetch(clientJobId, "Subtitler received transcript pages for a different local job.", false);
      return;
    }
    if (!fetch.inFlight) {
      this.requestTranscriptSegmentPage(clientJobId, nativeJobId, fetch.cursor);
    }
  }

  private requestTranscriptSegmentPage(clientJobId: string, nativeJobId: string, cursor: number): void {
    const fetch = this.transcriptFetches.get(clientJobId);
    if (!fetch || fetch.nativeJobId !== nativeJobId || fetch.inFlight || this.cancellationRequested.has(clientJobId)) {
      return;
    }
    const request = createNativeGetTranscriptSegmentsRequest(
      nativeJobId,
      cursor === 0 ? { limit: TRANSCRIPT_SEGMENT_PAGE_LIMIT } : { cursor, limit: TRANSCRIPT_SEGMENT_PAGE_LIMIT }
    );
    fetch.inFlight = true;
    fetch.requestId = request.request_id;
    try {
      this.postTracked(request, { kind: "transcript-segments", clientJobId, nativeJobId, cursor });
      this.scheduleTranscriptSegmentRetry(clientJobId, nativeJobId, cursor, request.request_id);
    } catch {
      fetch.inFlight = false;
      delete fetch.requestId;
      this.failTranscriptSegmentFetch(
        clientJobId,
        "Subtitler could not retrieve the completed transcript from the local engine.",
        true
      );
    }
  }

  private handleTranscriptSegmentPage(
    response: Extract<NativeHostResponse, { response: "transcript_segments" }>,
    pending: PendingRequest | undefined
  ): void {
    if (pending?.kind !== "transcript-segments" || !pending.clientJobId || this.cancellationRequested.has(pending.clientJobId)) {
      return;
    }
    const fetch = this.transcriptFetches.get(pending.clientJobId);
    if (!fetch) {
      return;
    }
    if (response.job_id !== pending.nativeJobId || fetch.nativeJobId !== response.job_id) {
      this.failTranscriptSegmentFetch(
        pending.clientJobId,
        "Subtitler received transcript segments for a different local job.",
        false
      );
      return;
    }
    if (fetch.requestId !== response.request_id || pending.cursor !== fetch.cursor) {
      return;
    }
    this.clearTranscriptSegmentRetry(pending.clientJobId);
    fetch.inFlight = false;
    delete fetch.requestId;

    if (response.segments.length > 0) {
      this.notify({
        protocolVersion: 1,
        type: "job.transcript-segments",
        payload: {
          jobId: pending.clientJobId,
          segments: mapNativeTranscriptSegments(response.segments)
        }
      });
    }

    if (response.next_cursor === undefined) {
      this.transcriptFetches.delete(pending.clientJobId);
      // A completed transcript is not available to the popup/export cache
      // until its separately paged, canonical subtitle cues have also drained.
      this.startTranscriptCueFetch(pending.clientJobId, response.job_id);
      return;
    }
    if (response.next_cursor <= pending.cursor) {
      this.failTranscriptSegmentFetch(
        pending.clientJobId,
        "Subtitler received an invalid transcript page from the local engine.",
        false
      );
      return;
    }
    fetch.cursor = response.next_cursor;
    fetch.retryCount = 0;
    this.requestTranscriptSegmentPage(pending.clientJobId, response.job_id, response.next_cursor);
  }

  private failTranscriptSegmentFetch(clientJobId: string, message: string, retryable: boolean): void {
    this.stopTranscriptSegmentFetch(clientJobId);
    this.stopTranscriptCueFetch(clientJobId);
    this.notify({
      protocolVersion: 1,
      type: "job.failed",
      payload: { jobId: clientJobId, code: "NATIVE_ERROR", message, retryable }
    });
  }

  /**
   * Full-transcript jobs use the host's same completed-only cue endpoint, but
   * never send the growing cue list to the popup. Each bounded page is handed
   * directly to the background's transient result store.
   */
  private startTranscriptCueFetch(clientJobId: string, nativeJobId: string): void {
    if (this.cancellationRequested.has(clientJobId)) {
      return;
    }
    let fetch = this.transcriptCueFetches.get(clientJobId);
    if (!fetch) {
      fetch = { nativeJobId, cursor: 0, inFlight: false, retryCount: 0 };
      this.transcriptCueFetches.set(clientJobId, fetch);
    } else if (fetch.nativeJobId !== nativeJobId) {
      this.failTranscriptCueFetch(clientJobId, "Subtitler received final subtitle cues for a different local job.", false);
      return;
    }
    if (!fetch.inFlight) {
      this.requestTranscriptCuePage(clientJobId, nativeJobId, fetch.cursor);
    }
  }

  private requestTranscriptCuePage(clientJobId: string, nativeJobId: string, cursor: number): void {
    const fetch = this.transcriptCueFetches.get(clientJobId);
    if (!fetch || fetch.nativeJobId !== nativeJobId || fetch.inFlight || this.cancellationRequested.has(clientJobId)) {
      return;
    }
    const request = createNativeGetSubtitleCuesRequest(
      nativeJobId,
      cursor === 0 ? { limit: SUBTITLE_CUE_PAGE_LIMIT } : { cursor, limit: SUBTITLE_CUE_PAGE_LIMIT }
    );
    fetch.inFlight = true;
    fetch.requestId = request.request_id;
    try {
      this.postTracked(request, { kind: "transcript-cues", clientJobId, nativeJobId, cursor });
      this.scheduleTranscriptCueRetry(clientJobId, nativeJobId, cursor, request.request_id);
    } catch {
      fetch.inFlight = false;
      delete fetch.requestId;
      this.failTranscriptCueFetch(
        clientJobId,
        "Subtitler could not retrieve final subtitle cues from the local engine.",
        true
      );
    }
  }

  private handleTranscriptCuePage(
    response: Extract<NativeHostResponse, { response: "subtitle_cues" }>,
    pending: PendingRequest | undefined
  ): void {
    if (pending?.kind !== "transcript-cues" || !pending.clientJobId || this.cancellationRequested.has(pending.clientJobId)) {
      return;
    }
    const fetch = this.transcriptCueFetches.get(pending.clientJobId);
    if (!fetch) {
      return;
    }
    if (response.job_id !== pending.nativeJobId || fetch.nativeJobId !== response.job_id) {
      this.failTranscriptCueFetch(
        pending.clientJobId,
        "Subtitler received final subtitle cues for a different local job.",
        false
      );
      return;
    }
    if (fetch.requestId !== response.request_id || pending.cursor !== fetch.cursor) {
      return;
    }
    this.clearTranscriptCueRetry(pending.clientJobId);
    fetch.inFlight = false;
    delete fetch.requestId;

    if (response.cues.length > 0) {
      this.notify({
        protocolVersion: 1,
        type: "job.transcript-cues",
        payload: {
          jobId: pending.clientJobId,
          cues: mapNativeSubtitleCues(response.cues, response.job_id, pending.cursor)
        }
      });
    }

    if (response.next_cursor === undefined) {
      this.transcriptCueFetches.delete(pending.clientJobId);
      this.notify({ protocolVersion: 1, type: "job.completed", payload: { jobId: pending.clientJobId } });
      return;
    }
    if (response.next_cursor <= pending.cursor) {
      this.failTranscriptCueFetch(
        pending.clientJobId,
        "Subtitler received an invalid final subtitle-cue page from the local engine.",
        false
      );
      return;
    }
    fetch.cursor = response.next_cursor;
    fetch.retryCount = 0;
    this.requestTranscriptCuePage(pending.clientJobId, response.job_id, response.next_cursor);
  }

  private failTranscriptCueFetch(clientJobId: string, message: string, retryable: boolean): void {
    this.stopTranscriptCueFetch(clientJobId);
    this.notify({
      protocolVersion: 1,
      type: "job.failed",
      payload: { jobId: clientJobId, code: "NATIVE_ERROR", message, retryable }
    });
  }

  /**
   * A completed job is only terminal in Chrome after every private result page
   * is received. Native Messaging can lose an individual reply when Chrome
   * suspends/restarts its service worker, so make each page request bounded and
   * idempotently retry the same opaque cursor before surfacing a clear error.
   */
  private scheduleTranscriptSegmentRetry(
    clientJobId: string,
    nativeJobId: string,
    cursor: number,
    requestId: string
  ): void {
    this.clearTranscriptSegmentRetry(clientJobId);
    const timer = setTimeout(() => {
      this.transcriptSegmentRetryTimers.delete(clientJobId);
      const fetch = this.transcriptFetches.get(clientJobId);
      if (
        !fetch ||
        fetch.nativeJobId !== nativeJobId ||
        !fetch.inFlight ||
        fetch.cursor !== cursor ||
        fetch.requestId !== requestId ||
        this.cancellationRequested.has(clientJobId)
      ) {
        return;
      }
      // The delayed original response is now stale. Removing its correlation
      // prevents it from completing a later retry for the same cursor.
      this.pendingRequests.delete(requestId);
      fetch.inFlight = false;
      delete fetch.requestId;
      if (fetch.retryCount >= MAX_RESULT_PAGE_RETRIES) {
        this.failTranscriptSegmentFetch(
          clientJobId,
          "Subtitler completed the local transcript, but Chrome could not retrieve its result pages. Reopen the popup to reconnect, then retry if the result is still unavailable.",
          true
        );
        return;
      }
      fetch.retryCount += 1;
      this.requestTranscriptSegmentPage(clientJobId, nativeJobId, cursor);
    }, RESULT_PAGE_RESPONSE_TIMEOUT_MS);
    this.transcriptSegmentRetryTimers.set(clientJobId, timer);
  }

  private clearTranscriptSegmentRetry(clientJobId: string): void {
    const timer = this.transcriptSegmentRetryTimers.get(clientJobId);
    if (timer !== undefined) {
      clearTimeout(timer);
      this.transcriptSegmentRetryTimers.delete(clientJobId);
    }
  }

  private clearAllTranscriptSegmentRetryTimers(): void {
    for (const timer of this.transcriptSegmentRetryTimers.values()) {
      clearTimeout(timer);
    }
    this.transcriptSegmentRetryTimers.clear();
  }

  private scheduleTranscriptCueRetry(clientJobId: string, nativeJobId: string, cursor: number, requestId: string): void {
    this.clearTranscriptCueRetry(clientJobId);
    const timer = setTimeout(() => {
      this.transcriptCueRetryTimers.delete(clientJobId);
      const fetch = this.transcriptCueFetches.get(clientJobId);
      if (
        !fetch ||
        fetch.nativeJobId !== nativeJobId ||
        !fetch.inFlight ||
        fetch.cursor !== cursor ||
        fetch.requestId !== requestId ||
        this.cancellationRequested.has(clientJobId)
      ) {
        return;
      }
      this.pendingRequests.delete(requestId);
      fetch.inFlight = false;
      delete fetch.requestId;
      if (fetch.retryCount >= MAX_RESULT_PAGE_RETRIES) {
        this.failTranscriptCueFetch(
          clientJobId,
          "Subtitler completed the local transcript, but Chrome could not retrieve its subtitle pages. Reopen the popup to reconnect, then retry if the result is still unavailable.",
          true
        );
        return;
      }
      fetch.retryCount += 1;
      this.requestTranscriptCuePage(clientJobId, nativeJobId, cursor);
    }, RESULT_PAGE_RESPONSE_TIMEOUT_MS);
    this.transcriptCueRetryTimers.set(clientJobId, timer);
  }

  private clearTranscriptCueRetry(clientJobId: string): void {
    const timer = this.transcriptCueRetryTimers.get(clientJobId);
    if (timer !== undefined) {
      clearTimeout(timer);
      this.transcriptCueRetryTimers.delete(clientJobId);
    }
  }

  private clearAllTranscriptCueRetryTimers(): void {
    for (const timer of this.transcriptCueRetryTimers.values()) {
      clearTimeout(timer);
    }
    this.transcriptCueRetryTimers.clear();
  }

  private startPolling(clientJobId: string, nativeJobId: string): void {
    this.stopPolling(clientJobId);
    const timer = setInterval(() => this.requestStatus(clientJobId, nativeJobId), STATUS_POLL_INTERVAL_MS);
    this.pollTimers.set(clientJobId, timer);
  }

  private flushQueuedRestores(): void {
    const restores = [...this.queuedRestores.values()];
    this.queuedRestores.clear();
    for (const job of restores) {
      this.requestRestore(job);
    }
  }

  private requestRestore(job: RecoverableNativeJob): void {
    if (this.restoreRequestsInFlight.has(job.clientJobId) || this.cancellationRequested.has(job.clientJobId)) {
      return;
    }
    this.restoreRequestsInFlight.add(job.clientJobId);
    try {
      const request = createNativeRestoreRequest(job.nativeJobId, job.kind);
      this.postTracked(request, { kind: "restore", clientJobId: job.clientJobId });
      this.scheduleRestoreStatusFallback(job);
    } catch {
      this.restoreRequestsInFlight.delete(job.clientJobId);
      this.clearRestoreStatusFallback(job.clientJobId);
      this.notify({
        protocolVersion: 1,
        type: "job.stale",
        payload: {
          jobId: job.clientJobId,
          message: "Subtitler could not reconnect this local job. Retry it to create a fresh local result."
        }
      });
    }
  }

  private scheduleRestoreStatusFallback(job: RecoverableNativeJob): void {
    this.clearRestoreStatusFallback(job.clientJobId);
    const timer = setTimeout(() => {
      this.restoreStatusFallbackTimers.delete(job.clientJobId);
      if (!this.restoreRequestsInFlight.has(job.clientJobId) || this.cancellationRequested.has(job.clientJobId)) {
        return;
      }
      // The restore request may have reached the host even when Chrome missed
      // its reply. A status request has the same opaque job ID and lets the
      // normal state handler reattach the result without replaying a source.
      this.requestStatus(job.clientJobId, job.nativeJobId);
    }, RESTORE_STATUS_FALLBACK_MS);
    this.restoreStatusFallbackTimers.set(job.clientJobId, timer);
  }

  private clearRestoreStatusFallback(clientJobId: string): void {
    const timer = this.restoreStatusFallbackTimers.get(clientJobId);
    if (timer !== undefined) {
      clearTimeout(timer);
      this.restoreStatusFallbackTimers.delete(clientJobId);
    }
  }

  private clearAllRestoreStatusFallbacks(): void {
    for (const timer of this.restoreStatusFallbackTimers.values()) {
      clearTimeout(timer);
    }
    this.restoreStatusFallbackTimers.clear();
  }

  private requestStatus(clientJobId: string, nativeJobId: string): void {
    if (!this.port || this.statusRequestsInFlight.has(clientJobId)) {
      return;
    }
    const request = createNativeStatusRequest(nativeJobId);
    this.statusRequestsInFlight.add(clientJobId);
    try {
      this.postTracked(request, { kind: "status", clientJobId });
    } catch {
      this.statusRequestsInFlight.delete(clientJobId);
      this.stopPolling(clientJobId);
      this.clearPlaybackUpdate(clientJobId);
      this.notify({
        protocolVersion: 1,
        type: "job.failed",
        payload: {
          jobId: clientJobId,
          code: "NATIVE_ERROR",
          message: "Subtitler could not query the local engine for this job.",
          retryable: true
        }
      });
    }
  }

  private stopPolling(clientJobId: string): void {
    const timer = this.pollTimers.get(clientJobId);
    if (timer !== undefined) {
      clearInterval(timer);
      this.pollTimers.delete(clientJobId);
    }
    this.statusRequestsInFlight.delete(clientJobId);
  }

  private stopCueFetch(clientJobId: string): void {
    this.cueFetches.delete(clientJobId);
  }

  private stopTranscriptSegmentFetch(clientJobId: string): void {
    this.transcriptFetches.delete(clientJobId);
    this.clearTranscriptSegmentRetry(clientJobId);
  }

  private stopTranscriptCueFetch(clientJobId: string): void {
    this.transcriptCueFetches.delete(clientJobId);
    this.clearTranscriptCueRetry(clientJobId);
  }

  private stopAllPolling(): void {
    for (const clientJobId of this.pollTimers.keys()) {
      this.stopPolling(clientJobId);
    }
  }

  private schedulePlaybackUpdate(clientJobId: string, nativeJobId: string): void {
    if (
      !this.port ||
      this.cancellationRequested.has(clientJobId) ||
      !this.queuedPlaybackUpdates.has(clientJobId) ||
      this.playbackUpdateTimers.has(clientJobId)
    ) {
      return;
    }
    const lastSentAt = this.playbackUpdateLastSentAt.get(clientJobId);
    const elapsed = lastSentAt === undefined ? PLAYBACK_UPDATE_MIN_INTERVAL_MS : Date.now() - lastSentAt;
    const delay = Math.max(0, PLAYBACK_UPDATE_MIN_INTERVAL_MS - elapsed);
    if (delay === 0) {
      this.flushPlaybackUpdate(clientJobId, nativeJobId);
      return;
    }
    const timer = setTimeout(() => {
      this.playbackUpdateTimers.delete(clientJobId);
      this.flushPlaybackUpdate(clientJobId, nativeJobId);
    }, delay);
    this.playbackUpdateTimers.set(clientJobId, timer);
  }

  private flushPlaybackUpdate(clientJobId: string, nativeJobId: string): void {
    if (
      !this.port ||
      this.cancellationRequested.has(clientJobId) ||
      this.nativeJobIds.get(clientJobId) !== nativeJobId
    ) {
      return;
    }
    const snapshot = this.queuedPlaybackUpdates.get(clientJobId);
    if (!snapshot) {
      return;
    }
    try {
      const request = createNativePlaybackUpdateRequest(nativeJobId, snapshot);
      // Native scheduling hints are intentionally fire-and-forget. A stale
      // update is harmless, and tracking every response would defeat lossiness.
      this.post(request);
      this.queuedPlaybackUpdates.delete(clientJobId);
      this.playbackUpdateLastSentAt.set(clientJobId, Date.now());
    } catch {
      // A new page snapshot will replace this one; never create a retry backlog.
      this.queuedPlaybackUpdates.delete(clientJobId);
    }
  }

  private clearPlaybackUpdate(clientJobId: string): void {
    const timer = this.playbackUpdateTimers.get(clientJobId);
    if (timer !== undefined) {
      clearTimeout(timer);
      this.playbackUpdateTimers.delete(clientJobId);
    }
    this.queuedPlaybackUpdates.delete(clientJobId);
    this.playbackUpdateLastSentAt.delete(clientJobId);
  }

  private clearAllPlaybackUpdates(): void {
    for (const clientJobId of this.playbackUpdateTimers.keys()) {
      this.clearPlaybackUpdate(clientJobId);
    }
    this.queuedPlaybackUpdates.clear();
    this.playbackUpdateLastSentAt.clear();
  }

  /** Send the first possible authoritative cancellation without reviving the UI job. */
  private cancelAcceptedJob(clientJobId: string, nativeJobId: string, state: NativeJobState): void {
    this.stopPolling(clientJobId);
    this.stopCueFetch(clientJobId);
    this.stopTranscriptSegmentFetch(clientJobId);
    this.stopTranscriptCueFetch(clientJobId);
    this.clearPlaybackUpdate(clientJobId);
    if (isTerminalState(state)) {
      this.forgetStoppedJob(clientJobId);
      return;
    }
    try {
      const request = createNativeCancelRequest(nativeJobId);
      this.postTracked(request, { kind: "cancel", clientJobId });
    } catch {
      // The persisted job is already stopped. Keep the tombstone so no delayed
      // status response can resurrect it; disconnect handling marks only live
      // jobs as recovering.
    }
  }

  private forgetStoppedJob(clientJobId: string): void {
    this.stopPolling(clientJobId);
    this.stopCueFetch(clientJobId);
    this.stopTranscriptSegmentFetch(clientJobId);
    this.stopTranscriptCueFetch(clientJobId);
    this.clearPlaybackUpdate(clientJobId);
    this.nativeJobIds.delete(clientJobId);
    this.cancellationRequested.delete(clientJobId);
  }

  private notifyProgress(
    clientJobId: string,
    response: Extract<NativeHostResponse, { response: "job_started" | "job_status" | "job_restored" }>
  ): void {
    const native = response.job.progress;
    const progress: JobProgress = { processedSeconds: native.processed_ms / 1_000 };
    if (native.media_duration_ms !== undefined) {
      progress.durationSeconds = native.media_duration_ms / 1_000;
    }
    if (native.subtitle_buffer_ahead_ms !== undefined) {
      progress.subtitleBufferSeconds = native.subtitle_buffer_ahead_ms / 1_000;
    }
    if (native.phase !== undefined) progress.phase = native.phase;
    if (native.last_progress_at_ms !== undefined) progress.lastProgressAt = new Date(native.last_progress_at_ms).toISOString();
    if (native.media_bytes_processed !== undefined) progress.mediaBytesProcessed = native.media_bytes_processed;
    if (native.audio_seconds_decoded_ms !== undefined) progress.audioSecondsDecoded = native.audio_seconds_decoded_ms / 1_000;
    if (native.audio_seconds_transcribed_ms !== undefined) progress.audioSecondsTranscribed = native.audio_seconds_transcribed_ms / 1_000;
    if (native.completed_intervals !== undefined) progress.completedIntervals = native.completed_intervals;
    if (native.worker_pid !== undefined) progress.workerPid = native.worker_pid;
    if (native.worker_status !== undefined) progress.workerStatus = native.worker_status;
    if (response.job.message !== undefined) {
      progress.statusMessage = response.job.message;
    }
    if (native.media_duration_ms !== undefined && native.media_duration_ms > 0) {
      progress.percent = Math.min(100, (native.processed_ms / native.media_duration_ms) * 100);
    }
    this.notify({
      protocolVersion: 1,
      type: "job.progress",
      payload: { jobId: clientJobId, status: "processing", progress }
    });
  }

  private clientJobIdFor(nativeJobId: string): string | undefined {
    for (const [clientJobId, candidate] of this.nativeJobIds) {
      if (candidate === nativeJobId) {
        return clientJobId;
      }
    }
    return undefined;
  }

  private notify(message: NativeInboundMessage): void {
    for (const listener of this.messageListeners) {
      listener(message);
    }
  }

  private post(message: unknown): void {
    if (!this.port) {
      throw new Error("Subtitler Native Engine is unavailable.");
    }
    this.port.postMessage(message);
  }
}

type PendingRequest =
  | { kind: "handshake" }
  | { kind: "start"; clientJobId: string }
  | { kind: "cancel"; clientJobId: string }
  | { kind: "status"; clientJobId: string }
  | { kind: "restore"; clientJobId: string }
  | { kind: "subtitle-cues"; clientJobId: string; nativeJobId: string; cursor: number }
  | { kind: "transcript-segments"; clientJobId: string; nativeJobId: string; cursor: number }
  | { kind: "transcript-cues"; clientJobId: string; nativeJobId: string; cursor: number };

interface SubtitleCueFetch {
  nativeJobId: string;
  /** First unrequested native-owned cue offset. */
  cursor: number;
  inFlight: boolean;
  /** The local host has reported completion; drain the final page then finish. */
  terminal: boolean;
}

interface TranscriptSegmentFetch {
  nativeJobId: string;
  /** First unrequested native-owned transcript offset. */
  cursor: number;
  inFlight: boolean;
  retryCount: number;
  requestId?: string;
}

interface TranscriptCueFetch {
  nativeJobId: string;
  /** First unrequested native-owned cue offset for a completed transcript. */
  cursor: number;
  inFlight: boolean;
  retryCount: number;
  requestId?: string;
}

interface RecoverableNativeJob {
  clientJobId: string;
  nativeJobId: string;
  kind: "transcript" | "subtitle";
}

type NativeJobState = Extract<NativeHostResponse, { response: "job_started" | "job_status" | "job_restored" }>["job"]["state"];

function isTerminalState(state: NativeJobState): boolean {
  return state === "completed" || state === "cancelled" || state === "stale" || state === "failed";
}

function mapHostErrorCode(code: string): "UNSUPPORTED_MEDIA" | "PROTECTED_MEDIA" | "NATIVE_ERROR" {
  if (code === "protected_media") {
    return "PROTECTED_MEDIA";
  }
  if (code === "unsupported_media" || code === "invalid_request") {
    return "UNSUPPORTED_MEDIA";
  }
  return "NATIVE_ERROR";
}

function mapNativeSubtitleCues(cues: readonly NativeSubtitleCue[], nativeJobId: string, cursor: number): SubtitleCue[] {
  return cues.map((cue, index) => {
    const mapped: SubtitleCue = {
      id: `${nativeJobId}:${cursor + index}`,
      startSeconds: cue.timing.start_ms / 1_000,
      endSeconds: cue.timing.end_ms / 1_000,
      text: cue.lines.join("\n")
    };
    if (cue.speaker !== undefined) {
      mapped.speaker = cue.speaker;
    }
    return mapped;
  });
}

function mapNativeTranscriptSegments(segments: readonly NativeTranscriptSegment[]): TranscriptSegment[] {
  return segments.map((segment) => {
    const mapped: TranscriptSegment = {
      startSeconds: segment.timing.start_ms / 1_000,
      endSeconds: segment.timing.end_ms / 1_000,
      text: segment.text
    };
    if (segment.speaker !== undefined) {
      mapped.speaker = segment.speaker;
    }
    return mapped;
  });
}
