import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  NativeClient,
  PLAYBACK_UPDATE_MIN_INTERVAL_MS,
  RESULT_PAGE_RESPONSE_TIMEOUT_MS,
  RESTORE_STATUS_FALLBACK_MS
} from "../src/background/native-client";

type PortListener = (value: unknown) => void;

const NATIVE_JOB_ID = "11111111-1111-4111-8111-111111111111";
const NATIVE_SUBTITLE_JOB_ID = "22222222-2222-4222-8222-222222222222";

describe("NativeClient", () => {
  let posted: unknown[];
  let messageListener: PortListener | undefined;
  let disconnectListener: (() => void) | undefined;

  beforeEach(() => {
    posted = [];
    messageListener = undefined;
    disconnectListener = undefined;

    const port = {
      postMessage: (message: unknown) => posted.push(message),
      onMessage: { addListener: (listener: PortListener) => (messageListener = listener) },
      onDisconnect: { addListener: (listener: () => void) => (disconnectListener = listener) }
    } as unknown as chrome.runtime.Port;

    (globalThis as unknown as { chrome: typeof chrome }).chrome = {
      runtime: {
        connectNative: vi.fn(() => port),
        lastError: undefined
      }
    } as unknown as typeof chrome;
  });

  afterEach(() => {
    vi.useRealTimers();
    delete (globalThis as unknown as { chrome?: typeof chrome }).chrome;
  });

  function replyWithHandshake(): void {
    const handshake = posted.find(
      (message): message is { request_id: string; command: string } => (message as { command?: string }).command === "handshake"
    );
    expect(handshake).toBeDefined();
    messageListener?.({
      request_id: handshake?.request_id,
      response: "handshake",
      native_host_name: "com.subtitler.native_host",
      protocol_version: 1,
      native_version: "0.1.0",
      capabilities: {
        protocol_version: 1,
        local_asr_available: false,
        ffmpeg_available: false,
        direct_media_acquisition: false,
        browser_mediated_acquisition: false,
        cloud_processing_requires_explicit_approval: true
      }
    });
  }

  it("uses the Rust host's framed-message JSON contract, polls status, and maps generated IDs", () => {
    vi.useFakeTimers();
    const client = new NativeClient();
    const events: unknown[] = [];
    client.onMessage((event) => events.push(event));

    client.startJob({
      jobId: "extension-job-1",
      jobKind: "transcript",
      source: { kind: "direct_url", mediaUrl: "https://media.example.test/recording.mp4", mediaKind: "video", durationSeconds: 60 }
    });

    expect(posted).toHaveLength(1);
    const handshake = posted[0] as { request_id: string; command: string };
    expect(handshake.command).toBe("handshake");

    messageListener?.({
      request_id: handshake.request_id,
      response: "handshake",
      native_host_name: "com.subtitler.native_host",
      protocol_version: 1,
      native_version: "0.1.0",
      capabilities: {
        protocol_version: 1,
        local_asr_available: false,
        ffmpeg_available: false,
        direct_media_acquisition: false,
        browser_mediated_acquisition: false,
        cloud_processing_requires_explicit_approval: true
      }
    });
    const start = posted[1] as { request_id: string; command: string; job: { kind: string } };
    expect(start.command).toBe("start");
    expect(start.job.kind).toBe("full_transcript");
    messageListener?.({
      request_id: start.request_id,
      response: "job_started",
      job: {
        job_id: NATIVE_JOB_ID,
        kind: "full_transcript",
        state: "queued",
        progress: { processed_ms: 0, media_duration_ms: 60_000 }
      }
    });

    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.accepted",
      payload: { jobId: "extension-job-1", nativeJobId: NATIVE_JOB_ID }
    });

    vi.advanceTimersByTime(2_000);
    const status = posted[2] as { request_id: string; command: string; job_id: string };
    expect(status).toMatchObject({ command: "status", job_id: NATIVE_JOB_ID });
    messageListener?.({
      request_id: status.request_id,
      response: "job_status",
      job: {
        job_id: NATIVE_JOB_ID,
        kind: "full_transcript",
        state: "completed",
        progress: { processed_ms: 60_000, media_duration_ms: 60_000 }
      }
    });
    const transcriptPage = posted[3] as { request_id: string; command: string; job_id: string; cursor?: number; limit?: number };
    expect(transcriptPage).toEqual({
      request_id: transcriptPage.request_id,
      command: "get_transcript_segments",
      job_id: NATIVE_JOB_ID,
      limit: 100
    });
    expect(events.some((event) => (event as { type?: string }).type === "job.completed")).toBe(false);
    messageListener?.({
      request_id: transcriptPage.request_id,
      response: "transcript_segments",
      job_id: NATIVE_JOB_ID,
      segments: [{ timing: { start_ms: 0, end_ms: 1_000 }, text: "Ready for viewing.", speaker: "Ada" }]
    });
    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.transcript-segments",
      payload: {
        jobId: "extension-job-1",
        segments: [{ startSeconds: 0, endSeconds: 1, text: "Ready for viewing.", speaker: "Ada" }]
      }
    });
    const cuePage = posted[4] as { request_id: string; command: string; job_id: string; cursor?: number; limit?: number };
    expect(cuePage).toEqual({
      request_id: cuePage.request_id,
      command: "get_subtitle_cues",
      job_id: NATIVE_JOB_ID,
      limit: 200
    });
    expect(events.some((event) => (event as { type?: string }).type === "job.completed")).toBe(false);
    messageListener?.({
      request_id: cuePage.request_id,
      response: "subtitle_cues",
      job_id: NATIVE_JOB_ID,
      cues: [{ timing: { start_ms: 0, end_ms: 1_000 }, lines: ["Ready for viewing."] }],
      next_cursor: 1
    });
    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.transcript-cues",
      payload: {
        jobId: "extension-job-1",
        cues: [{ id: `${NATIVE_JOB_ID}:0`, startSeconds: 0, endSeconds: 1, text: "Ready for viewing." }]
      }
    });
    expect(events.some((event) => (event as { type?: string }).type === "job.completed")).toBe(false);
    const secondCuePage = posted[5] as { request_id: string; command: string; job_id: string; cursor?: number; limit?: number };
    expect(secondCuePage).toEqual({
      request_id: secondCuePage.request_id,
      command: "get_subtitle_cues",
      job_id: NATIVE_JOB_ID,
      cursor: 1,
      limit: 200
    });
    messageListener?.({
      request_id: secondCuePage.request_id,
      response: "subtitle_cues",
      job_id: NATIVE_JOB_ID,
      cues: [{ timing: { start_ms: 1_000, end_ms: 2_000 }, lines: ["Final cue"] }]
    });
    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.transcript-cues",
      payload: {
        jobId: "extension-job-1",
        cues: [{ id: `${NATIVE_JOB_ID}:1`, startSeconds: 1, endSeconds: 2, text: "Final cue" }]
      }
    });
    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.completed",
      payload: { jobId: "extension-job-1" }
    });

    client.stopJob("extension-job-1");
    expect(posted[6]).toMatchObject({ command: "cancel", job_id: NATIVE_JOB_ID });
    expect(disconnectListener).toBeDefined();
  });

  it("reconciles a completed restore when only request correlation was lost", () => {
    const client = new NativeClient();
    const events: unknown[] = [];
    client.onMessage((event) => events.push(event));

    client.reconcileJobs([
      {
        clientJobId: "extension-restored-job",
        nativeJobId: NATIVE_JOB_ID,
        kind: "transcript"
      }
    ]);
    replyWithHandshake();

    const restore = posted[1] as { request_id: string; command: string; job_id: string };
    expect(restore).toMatchObject({ command: "restore", job_id: NATIVE_JOB_ID });

    // A service-worker interruption can lose a pending request entry even
    // though the persisted browser job still maps this opaque native UUID.
    messageListener?.({
      request_id: "correlation-lost",
      response: "job_restored",
      job: {
        job_id: NATIVE_JOB_ID,
        kind: "full_transcript",
        state: "completed",
        progress: {
          processed_ms: 60_000,
          media_duration_ms: 60_000,
          phase: "complete",
          worker_pid: null,
          worker_status: "finished"
        }
      }
    });

    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.accepted",
      payload: { jobId: "extension-restored-job", nativeJobId: NATIVE_JOB_ID }
    });
    expect(posted[2]).toMatchObject({ command: "get_transcript_segments", job_id: NATIVE_JOB_ID });
  });

  it("falls back to native status when Chrome misses a restore reply", () => {
    vi.useFakeTimers();
    const client = new NativeClient();
    const events: unknown[] = [];
    client.onMessage((event) => events.push(event));

    client.reconcileJobs([
      {
        clientJobId: "extension-restore-status-fallback",
        nativeJobId: NATIVE_JOB_ID,
        kind: "transcript"
      }
    ]);
    replyWithHandshake();
    expect(posted[1]).toMatchObject({ command: "restore", job_id: NATIVE_JOB_ID });

    vi.advanceTimersByTime(RESTORE_STATUS_FALLBACK_MS);
    const status = posted[2] as { request_id: string; command: string; job_id: string };
    expect(status).toMatchObject({ command: "status", job_id: NATIVE_JOB_ID });
    messageListener?.({
      request_id: status.request_id,
      response: "job_status",
      job: {
        job_id: NATIVE_JOB_ID,
        kind: "full_transcript",
        state: "completed",
        progress: { processed_ms: 60_000, media_duration_ms: 60_000 }
      }
    });

    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.accepted",
      payload: { jobId: "extension-restore-status-fallback", nativeJobId: NATIVE_JOB_ID }
    });
    expect(posted[3]).toMatchObject({ command: "get_transcript_segments", job_id: NATIVE_JOB_ID });
    expect(events.some((event) => (event as { type?: string }).type === "job.failed")).toBe(false);
  });

  it("retries a lost completed-transcript page instead of leaving Chrome in processing forever", () => {
    vi.useFakeTimers();
    const client = new NativeClient();
    const events: unknown[] = [];
    client.onMessage((event) => events.push(event));

    client.reconcileJobs([
      {
        clientJobId: "extension-lost-page",
        nativeJobId: NATIVE_JOB_ID,
        kind: "transcript"
      }
    ]);
    replyWithHandshake();

    const restore = posted[1] as { request_id: string };
    messageListener?.({
      request_id: restore.request_id,
      response: "job_restored",
      job: {
        job_id: NATIVE_JOB_ID,
        kind: "full_transcript",
        state: "completed",
        progress: { processed_ms: 60_000, media_duration_ms: 60_000 }
      }
    });
    const firstPage = posted[2] as { request_id: string; command: string; job_id: string };
    expect(firstPage).toMatchObject({ command: "get_transcript_segments", job_id: NATIVE_JOB_ID });

    // Simulate the service worker losing the reply while the native host keeps
    // running. The next request must use the same opaque cursor, not start a
    // second transcript job or remain permanently in flight.
    vi.advanceTimersByTime(RESULT_PAGE_RESPONSE_TIMEOUT_MS);
    const retryPage = posted[3] as { request_id: string; command: string; job_id: string };
    expect(retryPage).toMatchObject({ command: "get_transcript_segments", job_id: NATIVE_JOB_ID });
    expect(retryPage.request_id).not.toBe(firstPage.request_id);

    messageListener?.({
      request_id: retryPage.request_id,
      response: "transcript_segments",
      job_id: NATIVE_JOB_ID,
      segments: []
    });
    const cuePage = posted[4] as { request_id: string; command: string; job_id: string };
    expect(cuePage).toMatchObject({ command: "get_subtitle_cues", job_id: NATIVE_JOB_ID });
    messageListener?.({
      request_id: cuePage.request_id,
      response: "subtitle_cues",
      job_id: NATIVE_JOB_ID,
      cues: []
    });

    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.completed",
      payload: { jobId: "extension-lost-page" }
    });
  });

  it("forwards only the validated coarse local-processing advisory on engine readiness", () => {
    const client = new NativeClient();
    const events: unknown[] = [];
    client.onMessage((event) => events.push(event));
    client.startJob({
      jobId: "extension-advisory-job",
      jobKind: "transcript",
      source: { kind: "direct_url", mediaUrl: "https://media.example.test/recording.mp4", mediaKind: "video" }
    });

    const handshake = posted[0] as { request_id: string };
    messageListener?.({
      request_id: handshake.request_id,
      response: "handshake",
      native_host_name: "com.subtitler.native_host",
      protocol_version: 1,
      native_version: "0.1.0",
      capabilities: {
        protocol_version: 1,
        local_asr_available: false,
        ffmpeg_available: false,
        direct_media_acquisition: true,
        browser_mediated_acquisition: false,
        cloud_processing_requires_explicit_approval: true,
        local_processing_advisory: {
          selection_source: "automatic",
          model: "small",
          quantization: "q5_k_m",
          backend: "cpu",
          local_performance: "good",
          memory_mb: 16384
        }
      }
    });

    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "engine.ready",
      payload: {
        engineVersion: "0.1.0",
        localProcessingAvailable: false,
        localProcessingAdvisory: {
          selectionSource: "automatic",
          model: "small",
          quantization: "q5_k_m",
          backend: "cpu",
          localPerformance: "good"
        }
      }
    });
  });

  it("pages completed subtitle cues into bounded overlay updates before completing the job", () => {
    const client = new NativeClient();
    const events: unknown[] = [];
    client.onMessage((event) => events.push(event));

    client.startJob({
      jobId: "extension-subtitle-job",
      jobKind: "subtitle",
      source: { kind: "direct_url", mediaUrl: "https://media.example.test/recording.mp4", mediaKind: "video" }
    });

    replyWithHandshake();

    const start = posted[1] as { request_id: string; command: string };
    messageListener?.({
      request_id: start.request_id,
      response: "job_started",
      job: {
        job_id: NATIVE_SUBTITLE_JOB_ID,
        kind: "subtitle_generation",
        state: "completed",
        progress: { processed_ms: 4_000, media_duration_ms: 4_000 }
      }
    });

    const firstPage = posted[2] as { request_id: string; command: string; job_id: string; cursor?: number; limit?: number };
    expect(firstPage).toEqual({
      request_id: firstPage.request_id,
      command: "get_subtitle_cues",
      job_id: NATIVE_SUBTITLE_JOB_ID,
      limit: 200
    });
    messageListener?.({
      request_id: firstPage.request_id,
      response: "subtitle_cues",
      job_id: NATIVE_SUBTITLE_JOB_ID,
      cues: [
        { timing: { start_ms: 0, end_ms: 1_000 }, lines: ["First", "cue"], speaker: "Ada" }
      ],
      next_cursor: 1
    });

    const secondPage = posted[3] as { request_id: string; command: string; job_id: string; cursor?: number; limit?: number };
    expect(secondPage).toEqual({
      request_id: secondPage.request_id,
      command: "get_subtitle_cues",
      job_id: NATIVE_SUBTITLE_JOB_ID,
      cursor: 1,
      limit: 200
    });
    messageListener?.({
      request_id: secondPage.request_id,
      response: "subtitle_cues",
      job_id: NATIVE_SUBTITLE_JOB_ID,
      cues: [{ timing: { start_ms: 1_000, end_ms: 2_000 }, lines: ["Second cue"] }]
    });

    const firstCueEvent = {
      protocolVersion: 1,
      type: "job.subtitle-cues",
      payload: {
        jobId: "extension-subtitle-job",
        cues: [{ id: `${NATIVE_SUBTITLE_JOB_ID}:0`, startSeconds: 0, endSeconds: 1, text: "First\ncue", speaker: "Ada" }]
      }
    };
    const secondCueEvent = {
      protocolVersion: 1,
      type: "job.subtitle-cues",
      payload: {
        jobId: "extension-subtitle-job",
        cues: [{ id: `${NATIVE_SUBTITLE_JOB_ID}:1`, startSeconds: 1, endSeconds: 2, text: "Second cue" }]
      }
    };
    expect(events).toContainEqual(firstCueEvent);
    expect(events).toContainEqual(secondCueEvent);
    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.completed",
      payload: { jobId: "extension-subtitle-job" }
    });
    const firstCueIndex = events.findIndex(
      (event) =>
        (event as { type?: string }).type === "job.subtitle-cues" &&
        (event as { payload?: { cues?: Array<{ id?: string }> } }).payload?.cues?.[0]?.id === `${NATIVE_SUBTITLE_JOB_ID}:0`
    );
    const secondCueIndex = events.findIndex(
      (event) =>
        (event as { type?: string }).type === "job.subtitle-cues" &&
        (event as { payload?: { cues?: Array<{ id?: string }> } }).payload?.cues?.[0]?.id === `${NATIVE_SUBTITLE_JOB_ID}:1`
    );
    const completedIndex = events.findIndex(
      (event) =>
        (event as { type?: string }).type === "job.completed" &&
        (event as { payload?: { jobId?: string } }).payload?.jobId === "extension-subtitle-job"
    );
    expect(firstCueIndex).toBeLessThan(secondCueIndex);
    expect(secondCueIndex).toBeLessThan(completedIndex);
  });

  it("drains completed transcript pages before completion and ignores a late page after stop", () => {
    const client = new NativeClient();
    const events: unknown[] = [];
    client.onMessage((event) => events.push(event));
    client.startJob({
      jobId: "extension-transcript-pages",
      jobKind: "transcript",
      source: { kind: "direct_url", mediaUrl: "https://media.example.test/recording.mp4", mediaKind: "video" }
    });

    replyWithHandshake();

    const start = posted[1] as { request_id: string };
    messageListener?.({
      request_id: start.request_id,
      response: "job_started",
      job: {
        job_id: NATIVE_JOB_ID,
        kind: "full_transcript",
        state: "completed",
        progress: { processed_ms: 4_000, media_duration_ms: 4_000 }
      }
    });
    const firstPage = posted[2] as { request_id: string; command: string; cursor?: number; limit?: number };
    expect(firstPage).toEqual({
      request_id: firstPage.request_id,
      command: "get_transcript_segments",
      job_id: NATIVE_JOB_ID,
      limit: 100
    });
    messageListener?.({
      request_id: firstPage.request_id,
      response: "transcript_segments",
      job_id: NATIVE_JOB_ID,
      segments: [{ timing: { start_ms: 0, end_ms: 1_000 }, text: "First segment" }],
      next_cursor: 1
    });
    const secondPage = posted[3] as { request_id: string; command: string; cursor?: number; limit?: number };
    expect(secondPage).toEqual({
      request_id: secondPage.request_id,
      command: "get_transcript_segments",
      job_id: NATIVE_JOB_ID,
      cursor: 1,
      limit: 100
    });

    // A user stop removes the active fetch before any late response is
    // admitted. The late completed page must never revive the UI job.
    client.stopJob("extension-transcript-pages");
    messageListener?.({
      request_id: secondPage.request_id,
      response: "transcript_segments",
      job_id: NATIVE_JOB_ID,
      segments: [{ timing: { start_ms: 1_000, end_ms: 2_000 }, text: "Late segment" }]
    });

    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.transcript-segments",
      payload: { jobId: "extension-transcript-pages", segments: [{ startSeconds: 0, endSeconds: 1, text: "First segment" }] }
    });
    expect(events.some((event) => (event as { type?: string }).type === "job.completed")).toBe(false);
    expect(
      events.some((event) =>
        (event as { payload?: { segments?: Array<{ text?: string }> } }).payload?.segments?.some(
          (segment) => segment.text === "Late segment"
        )
      )
    ).toBe(false);
  });

  it("drops a pending transcript page after the native connection disconnects", () => {
    const client = new NativeClient();
    const events: unknown[] = [];
    client.onMessage((event) => events.push(event));
    client.startJob({
      jobId: "extension-transcript-disconnect",
      jobKind: "transcript",
      source: { kind: "direct_url", mediaUrl: "https://media.example.test/recording.mp4", mediaKind: "video" }
    });
    replyWithHandshake();
    const start = posted[1] as { request_id: string };
    messageListener?.({
      request_id: start.request_id,
      response: "job_started",
      job: {
        job_id: NATIVE_JOB_ID,
        kind: "full_transcript",
        state: "completed",
        progress: { processed_ms: 1_000, media_duration_ms: 1_000 }
      }
    });
    const page = posted[2] as { request_id: string };
    disconnectListener?.();
    messageListener?.({
      request_id: page.request_id,
      response: "transcript_segments",
      job_id: NATIVE_JOB_ID,
      segments: [{ timing: { start_ms: 0, end_ms: 1_000 }, text: "Must be ignored" }]
    });
    expect(events.some((event) => (event as { type?: string }).type === "job.transcript-segments")).toBe(false);
    expect(events.some((event) => (event as { type?: string }).type === "job.completed")).toBe(false);
  });

  it("keeps only the newest playback state, waits for native acceptance, and rate-limits scheduler hints", () => {
    vi.useFakeTimers();
    const client = new NativeClient();

    client.startJob({
      jobId: "extension-playback-job",
      jobKind: "subtitle",
      source: { kind: "direct_url", mediaUrl: "https://media.example.test/recording.mp4", mediaKind: "video" }
    });
    client.updatePlayback("extension-playback-job", {
      positionMs: 5_000,
      playbackRateMilli: 1_000,
      isPaused: false,
      seekGeneration: 0
    });
    client.updatePlayback("extension-playback-job", {
      positionMs: 7_500,
      playbackRateMilli: 1_250,
      isPaused: false,
      seekGeneration: 1
    });

    // Pre-acceptance snapshots remain in one slot while the handshake is pending.
    expect(posted).toHaveLength(1);
    replyWithHandshake();
    expect(posted).toHaveLength(2);
    const start = posted[1] as { request_id: string; command: string };
    messageListener?.({
      request_id: start.request_id,
      response: "job_started",
      job: {
        job_id: NATIVE_SUBTITLE_JOB_ID,
        kind: "subtitle_generation",
        state: "processing",
        progress: { processed_ms: 0, media_duration_ms: 60_000 }
      }
    });

    // Processing subtitle jobs also ask for any already-finalized cue page;
    // locate the lossy scheduling hint by command rather than message index.
    const firstUpdate = posted.find(
      (message): message is Record<string, unknown> => (message as { command?: string }).command === "playback_update"
    );
    expect(firstUpdate).toMatchObject({
      command: "playback_update",
      job_id: NATIVE_SUBTITLE_JOB_ID,
      position_ms: 7_500,
      playback_rate_milli: 1_250,
      is_paused: false,
      seek_generation: 1
    });

    client.updatePlayback("extension-playback-job", {
      positionMs: 8_000,
      playbackRateMilli: 1_000,
      isPaused: false,
      seekGeneration: 1
    });
    client.updatePlayback("extension-playback-job", {
      positionMs: 44_000,
      playbackRateMilli: 2_000,
      isPaused: true,
      seekGeneration: 2
    });
    expect(posted.filter((message) => (message as { command?: string }).command === "playback_update")).toHaveLength(1);

    vi.advanceTimersByTime(PLAYBACK_UPDATE_MIN_INTERVAL_MS - 1);
    expect(posted.filter((message) => (message as { command?: string }).command === "playback_update")).toHaveLength(1);
    vi.advanceTimersByTime(1);
    const playbackUpdates = posted.filter(
      (message): message is Record<string, unknown> => (message as { command?: string }).command === "playback_update"
    );
    expect(playbackUpdates).toHaveLength(2);
    expect(playbackUpdates[1]).toMatchObject({
      job_id: NATIVE_SUBTITLE_JOB_ID,
      position_ms: 44_000,
      playback_rate_milli: 2_000,
      is_paused: true,
      seek_generation: 2
    });

    // Stopping the job clears a pending low-priority scheduler hint.
    client.updatePlayback("extension-playback-job", {
      positionMs: 45_000,
      playbackRateMilli: 1_000,
      isPaused: false,
      seekGeneration: 2
    });
    client.stopJob("extension-playback-job");
    vi.advanceTimersByTime(PLAYBACK_UPDATE_MIN_INTERVAL_MS);
    expect(posted.filter((message) => (message as { command?: string }).command === "playback_update")).toHaveLength(2);
  });

  it("does not start a job stopped while its native handshake is pending", () => {
    const client = new NativeClient();
    const events: unknown[] = [];
    client.onMessage((event) => events.push(event));
    client.startJob({
      jobId: "extension-stop-before-accepted",
      jobKind: "subtitle",
      source: { kind: "direct_url", mediaUrl: "https://media.example.test/recording.mp4", mediaKind: "video", durationSeconds: 60 }
    });

    client.stopJob("extension-stop-before-accepted");
    expect(posted.filter((message) => (message as { command?: string }).command === "cancel")).toHaveLength(0);
    replyWithHandshake();
    expect(posted.filter((message) => (message as { command?: string }).command === "start")).toHaveLength(0);
    expect(posted.filter((message) => (message as { command?: string }).command === "cancel")).toHaveLength(0);
    expect(events.some((event) => (event as { type?: string }).type === "job.accepted")).toBe(false);
    expect(events.some((event) => (event as { type?: string }).type === "job.progress")).toBe(false);
  });

  it("keeps a processing subtitle job open after an empty current cue page and drains it on completion", () => {
    const client = new NativeClient();
    const events: unknown[] = [];
    client.onMessage((event) => events.push(event));
    client.startJob({
      jobId: "extension-progressive-cues",
      jobKind: "subtitle",
      source: { kind: "direct_url", mediaUrl: "https://media.example.test/recording.mp4", mediaKind: "video" }
    });

    replyWithHandshake();

    const start = posted[1] as { request_id: string };
    messageListener?.({
      request_id: start.request_id,
      response: "job_started",
      job: {
        job_id: NATIVE_SUBTITLE_JOB_ID,
        kind: "subtitle_generation",
        state: "processing",
        progress: { processed_ms: 0, media_duration_ms: 60_000, subtitle_buffer_ahead_ms: 0 }
      }
    });
    const firstCueRequest = posted.find(
      (message): message is { request_id: string; command: string } =>
        (message as { command?: string }).command === "get_subtitle_cues"
    );
    expect(firstCueRequest).toBeDefined();
    messageListener?.({
      request_id: firstCueRequest?.request_id,
      response: "subtitle_cues",
      job_id: NATIVE_SUBTITLE_JOB_ID,
      cues: []
    });
    expect(events.some((event) => (event as { type?: string }).type === "job.completed")).toBe(false);

    messageListener?.({
      response: "job_status",
      job: {
        job_id: NATIVE_SUBTITLE_JOB_ID,
        kind: "subtitle_generation",
        state: "completed",
        progress: { processed_ms: 60_000, media_duration_ms: 60_000, subtitle_buffer_ahead_ms: 0 }
      }
    });
    const cueRequests = posted.filter(
      (message): message is { request_id: string; command: string } =>
        (message as { command?: string }).command === "get_subtitle_cues"
    );
    const finalCueRequest = cueRequests.at(-1);
    messageListener?.({
      request_id: finalCueRequest?.request_id,
      response: "subtitle_cues",
      job_id: NATIVE_SUBTITLE_JOB_ID,
      cues: [{ timing: { start_ms: 0, end_ms: 1_000 }, lines: ["Ready now"] }]
    });
    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.subtitle-cues",
      payload: {
        jobId: "extension-progressive-cues",
        cues: [{ id: `${NATIVE_SUBTITLE_JOB_ID}:0`, startSeconds: 0, endSeconds: 1, text: "Ready now" }]
      }
    });
    expect(events).toContainEqual({
      protocolVersion: 1,
      type: "job.completed",
      payload: { jobId: "extension-progressive-cues" }
    });
  });
});
