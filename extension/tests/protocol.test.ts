import { describe, expect, it } from "vitest";
import {
  createNativePlaybackUpdateRequest,
  createNativeGetSubtitleCuesRequest,
  createNativeGetTranscriptSegmentsRequest,
  createNativeStartRequest,
  createNativeStatusRequest,
  isContentPlaybackUpdate,
  isEngineConnectionStateUpdate,
  isPopupRequest,
  isSafeLocalFilePageUrl,
  NATIVE_HOST_NAME,
  NATIVE_PROTOCOL_VERSION,
  parseNativeHostResponse,
  parseEngineConnectionStateUpdate,
  sanitizeDirectMediaUrl,
  sanitizeLocalFilePath,
  sanitizeLocalFileUrl
} from "../src/shared/protocol";

const NATIVE_JOB_ID = "11111111-1111-4111-8111-111111111111";

describe("native protocol validation", () => {
  it("uses the shared native-host name and version", () => {
    expect(NATIVE_HOST_NAME).toBe("com.subtitler.native_host");
    expect(NATIVE_PROTOCOL_VERSION).toBe(1);
  });

  it("serializes a start request in the Rust host's exact flat shape", () => {
    const request = createNativeStartRequest(
      {
        jobId: "extension-job-1",
        jobKind: "subtitle",
        source: {
          kind: "direct_url",
          mediaUrl: "https://media.example.test/recording.mp4",
          mediaKind: "video",
          durationSeconds: 61.25
        },
        forceGenerateWithSubtitler: true
      },
      "request-1"
    );
    expect(request).toEqual({
      request_id: "request-1",
      command: "start",
      job: {
        client_job_id: "extension-job-1",
        kind: "subtitle_generation",
        media: {
          source: { kind: "direct_url", media_url: "https://media.example.test/recording.mp4" },
          hints: { duration_ms: 61_250 }
        },
        settings: {
          force_generate_with_subtitler: true,
          processing_preference: "local_only",
          speaker_diarization: false
        }
      }
    });
  });

  it("serializes a validated local file source in Rust's tagged MediaSource shape", () => {
    const request = createNativeStartRequest(
      {
        jobId: "extension-local-job",
        jobKind: "transcript",
        source: {
          kind: "local_file",
          path: "C:\\Users\\Alice\\Videos\\meeting.mp4",
          mediaKind: "video",
          durationSeconds: 42
        }
      },
      "local-request-1"
    );

    expect(request.job.media).toEqual({
      source: { kind: "local_file", path: "C:\\Users\\Alice\\Videos\\meeting.mp4" },
      hints: { duration_ms: 42_000 }
    });
  });

  it("serializes a recognized page for native direct-media resolution without browser secrets", () => {
    const request = createNativeStartRequest(
      {
        jobId: "extension-youtube-job",
        jobKind: "subtitle",
        source: {
          kind: "page",
          pageUrl: "https://youtu.be/ESjPc7I5h_Q?si=test",
          mediaKind: "video",
          durationSeconds: 258
        },
        forceGenerateWithSubtitler: true
      },
      "youtube-page-request"
    );
    expect(request.job.media).toEqual({
      source: { kind: "page", page_url: "https://youtu.be/ESjPc7I5h_Q?si=test" },
      hints: { duration_ms: 258_000 }
    });
  });

  it("places the initial generated-subtitle playhead in Start so scheduling does not begin at zero", () => {
    const request = createNativeStartRequest(
      {
        jobId: "extension-initial-playhead",
        jobKind: "subtitle",
        source: {
          kind: "direct_url",
          mediaUrl: "https://media.example.test/recording.mp4",
          mediaKind: "video",
          durationSeconds: 600
        },
        initialPlayback: { positionSeconds: 123.456, isPaused: true }
      },
      "initial-playhead-request"
    );

    expect(request.job.settings.initial_playback).toEqual({
      position_ms: 123_456,
      playback_rate_milli: 1_000,
      is_paused: true
    });
  });

  it("accepts the exact handshake response emitted by the Rust host", () => {
    const parsed = parseNativeHostResponse({
      request_id: "request-1",
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
    expect(parsed?.response).toBe("handshake");
  });

  it("strictly admits only the coarse local-processing handshake advisory", () => {
    const baseHandshake = {
      request_id: "request-advisory",
      response: "handshake",
      native_host_name: "com.subtitler.native_host",
      protocol_version: 1,
      native_version: "0.1.0",
      capabilities: {
        protocol_version: 1,
        local_asr_available: true,
        ffmpeg_available: true,
        direct_media_acquisition: true,
        browser_mediated_acquisition: false,
        cloud_processing_requires_explicit_approval: true
      }
    };
    const parsed = parseNativeHostResponse({
      ...baseHandshake,
      capabilities: {
        ...baseHandshake.capabilities,
        local_processing_advisory: {
          selection_source: "automatic",
          model: "small",
          quantization: "q5_k_m",
          backend: "cpu",
          local_performance: "good",
          device_name: "must-not-be-admitted"
        }
      }
    });
    expect(parsed).toMatchObject({
      response: "handshake",
      capabilities: {
        localProcessingAdvisory: {
          selectionSource: "automatic",
          model: "small",
          quantization: "q5_k_m",
          backend: "cpu",
          localPerformance: "good"
        }
      }
    });

    const malformed = parseNativeHostResponse({
      ...baseHandshake,
      capabilities: {
        ...baseHandshake.capabilities,
        local_processing_advisory: {
          selection_source: "automatic",
          model: "small",
          quantization: "q5_k_m",
          backend: "ssh",
          local_performance: "good"
        }
      }
    });
    expect(malformed).toMatchObject({ response: "handshake", capabilities: {} });
    if (!malformed || malformed.response !== "handshake") {
      throw new Error("Expected a valid base handshake.");
    }
    expect(malformed.capabilities.localProcessingAdvisory).toBeUndefined();

    const updateWithExtraneousField = {
      type: "engine.connection-state",
      payload: {
        connected: true,
        localProcessingAvailable: true,
        localProcessingAdvisory: {
          selectionSource: "automatic",
          model: "small",
          quantization: "q5_k_m",
          backend: "cpu",
          localPerformance: "good",
          deviceName: "must-not-reach-popup-state"
        }
      }
    };
    expect(isEngineConnectionStateUpdate(updateWithExtraneousField)).toBe(true);
    expect(parseEngineConnectionStateUpdate(updateWithExtraneousField)).toEqual({
      type: "engine.connection-state",
      payload: {
        connected: true,
        localProcessingAvailable: true,
        localProcessingAdvisory: {
          selectionSource: "automatic",
          model: "small",
          quantization: "q5_k_m",
          backend: "cpu",
          localPerformance: "good"
        }
      }
    });
    expect(
      isEngineConnectionStateUpdate({
        type: "engine.connection-state",
        payload: {
          connected: true,
          localProcessingAvailable: true,
          localProcessingAdvisory: { localPerformance: "good" }
        }
      })
    ).toBe(false);
  });

  it("serializes status polling and requires a safe failure body for failed jobs", () => {
    expect(createNativeStatusRequest(NATIVE_JOB_ID, "status-request")).toEqual({
      request_id: "status-request",
      command: "status",
      job_id: NATIVE_JOB_ID
    });
    expect(
      parseNativeHostResponse({
        request_id: "status-request",
        response: "job_status",
        job: {
          job_id: NATIVE_JOB_ID,
          kind: "full_transcript",
          state: "failed",
          progress: { processed_ms: 2_000 },
          failure: { code: "model_unavailable", message: "Install a local model.", retryable: true }
        }
      })
    ).toMatchObject({
      response: "job_status",
      job: { state: "failed", failure: { code: "model_unavailable", retryable: true } }
    });
    expect(
      parseNativeHostResponse({
        response: "job_status",
        job: { job_id: NATIVE_JOB_ID, kind: "full_transcript", state: "failed", progress: { processed_ms: 0 } }
      })
    ).toBeNull();
  });

  it("accepts a completed restore when Rust emits null optional worker telemetry", () => {
    expect(
      parseNativeHostResponse({
        request_id: "restore-completed",
        response: "job_restored",
        job: {
          job_id: NATIVE_JOB_ID,
          kind: "full_transcript",
          state: "completed",
          progress: {
            processed_ms: 60_000,
            media_duration_ms: 60_000,
            worker_status: null,
            worker_pid: null,
            last_progress_at_ms: null
          }
        }
      })
    ).toMatchObject({ response: "job_restored", job: { state: "completed" } });
  });

  it("serializes cue-page requests and parses the Rust subtitle_cues response shape", () => {
    expect(createNativeGetSubtitleCuesRequest(NATIVE_JOB_ID, { limit: 200 }, "cue-request-1")).toEqual({
      request_id: "cue-request-1",
      command: "get_subtitle_cues",
      job_id: NATIVE_JOB_ID,
      limit: 200
    });
    expect(createNativeGetSubtitleCuesRequest(NATIVE_JOB_ID, { cursor: 200, limit: 200 }, "cue-request-2")).toEqual({
      request_id: "cue-request-2",
      command: "get_subtitle_cues",
      job_id: NATIVE_JOB_ID,
      cursor: 200,
      limit: 200
    });

    const parsed = parseNativeHostResponse({
      request_id: "cue-request-1",
      response: "subtitle_cues",
      job_id: NATIVE_JOB_ID,
      cues: [
        {
          timing: { start_ms: 1_000, end_ms: 2_250 },
          lines: ["First line", "Second line"],
          speaker: null
        }
      ],
      next_cursor: 1
    });
    expect(parsed).toEqual({
      request_id: "cue-request-1",
      response: "subtitle_cues",
      job_id: NATIVE_JOB_ID,
      cues: [{ timing: { start_ms: 1_000, end_ms: 2_250 }, lines: ["First line", "Second line"] }],
      next_cursor: 1
    });
    expect(
      parseNativeHostResponse({
        request_id: "cue-request-1",
        response: "subtitle_cues",
        job_id: NATIVE_JOB_ID,
        cues: [{ timing: { start_ms: 2_000, end_ms: 1_000 }, lines: ["Invalid"] }]
      })
    ).toBeNull();
    expect(() => createNativeGetSubtitleCuesRequest("not-a-native-uuid", { limit: 200 }, "cue-request-3")).toThrow();
    expect(
      parseNativeHostResponse({
        request_id: "cue-request-1",
        response: "subtitle_cues",
        job_id: NATIVE_JOB_ID,
        cues: [],
        next_cursor: 4_294_967_296
      })
    ).toBeNull();
  });

  it("uses the bounded completed-transcript paging wire and rejects malformed segment pages", () => {
    expect(createNativeGetTranscriptSegmentsRequest(NATIVE_JOB_ID, { limit: 100 }, "transcript-page-1")).toEqual({
      request_id: "transcript-page-1",
      command: "get_transcript_segments",
      job_id: NATIVE_JOB_ID,
      limit: 100
    });
    expect(createNativeGetTranscriptSegmentsRequest(NATIVE_JOB_ID, { cursor: 2, limit: 100 }, "transcript-page-2")).toEqual({
      request_id: "transcript-page-2",
      command: "get_transcript_segments",
      job_id: NATIVE_JOB_ID,
      cursor: 2,
      limit: 100
    });
    expect(() => createNativeGetTranscriptSegmentsRequest(NATIVE_JOB_ID, { limit: 101 })).toThrow(
      "invalid transcript-segment page limit"
    );

    expect(
      parseNativeHostResponse({
        request_id: "transcript-page-1",
        response: "transcript_segments",
        job_id: NATIVE_JOB_ID,
        segments: [
          {
            timing: { start_ms: 1_000, end_ms: 2_500 },
            text: "A completed transcript segment.",
            speaker: "Ada"
          }
        ],
        next_cursor: 1
      })
    ).toEqual({
      request_id: "transcript-page-1",
      response: "transcript_segments",
      job_id: NATIVE_JOB_ID,
      segments: [
        {
          timing: { start_ms: 1_000, end_ms: 2_500 },
          text: "A completed transcript segment.",
          speaker: "Ada"
        }
      ],
      next_cursor: 1
    });
    expect(
      parseNativeHostResponse({
        response: "transcript_segments",
        job_id: NATIVE_JOB_ID,
        segments: [{ timing: { start_ms: 2_000, end_ms: 1_000 }, text: "Invalid" }]
      })
    ).toBeNull();
    expect(
      parseNativeHostResponse({
        response: "transcript_segments",
        job_id: NATIVE_JOB_ID,
        segments: [{ timing: { start_ms: 0, end_ms: 1 }, text: "x".repeat(16_385) }]
      })
    ).toBeNull();
    expect(
      parseNativeHostResponse({
        response: "transcript_segments",
        job_id: NATIVE_JOB_ID,
        segments: [{ timing: { start_ms: 0, end_ms: 1 }, text: "😀".repeat(4_097) }]
      })
    ).toBeNull();
    expect(
      parseNativeHostResponse({
        response: "transcript_segments",
        job_id: NATIVE_JOB_ID,
        segments: Array.from({ length: 101 }, () => ({ timing: { start_ms: 0, end_ms: 1 }, text: "Too many" }))
      })
    ).toBeNull();
    expect(
      parseNativeHostResponse({
        response: "transcript_segments",
        job_id: NATIVE_JOB_ID,
        segments: Array.from({ length: 100 }, () => ({ timing: { start_ms: 0, end_ms: 1 }, text: "x".repeat(2_000) }))
      })
    ).toBeNull();
    expect(
      isPopupRequest({ type: "popup.get-transcript", jobId: "extension-job", cursor: 0, limit: 100 })
    ).toBe(true);
    expect(
      isPopupRequest({ type: "popup.get-transcript", jobId: "extension-job", cursor: 0, limit: 101 })
    ).toBe(false);
    expect(
      isPopupRequest({ type: "popup.export-transcript", jobId: "extension-job", format: "srt" })
    ).toBe(true);
    expect(
      isPopupRequest({ type: "popup.export-transcript", jobId: "extension-job", format: "zip" })
    ).toBe(false);
  });

  it("serializes bounded playback hints in the native scheduler's snake_case shape", () => {
    expect(
      createNativePlaybackUpdateRequest(
        NATIVE_JOB_ID,
        { positionMs: 83_125, playbackRateMilli: 1_250, isPaused: false, seekGeneration: 4 },
        "playback-request-1"
      )
    ).toEqual({
      request_id: "playback-request-1",
      command: "playback_update",
      job_id: NATIVE_JOB_ID,
      position_ms: 83_125,
      playback_rate_milli: 1_250,
      is_paused: false,
      seek_generation: 4
    });
    expect(() =>
      createNativePlaybackUpdateRequest(
        NATIVE_JOB_ID,
        { positionMs: 0, playbackRateMilli: 249, isPaused: true, seekGeneration: 0 },
        "playback-request-2"
      )
    ).toThrow("invalid playback rate");
  });

  it("accepts only metadata-only page playback updates", () => {
    expect(
      isContentPlaybackUpdate({
        type: "content.playback-update",
        jobId: NATIVE_JOB_ID,
        mediaId: "media-1",
        positionMs: 12_345,
        playbackRateMilli: 1_000,
        isPaused: false,
        seekGeneration: 2
      })
    ).toBe(true);
    expect(
      isContentPlaybackUpdate({
        type: "content.playback-update",
        jobId: NATIVE_JOB_ID,
        mediaId: "media-1",
        positionMs: 12_345,
        playbackRateMilli: 5_000,
        isPaused: false,
        seekGeneration: 2
      })
    ).toBe(false);
  });

  it("rejects malformed native responses before they reach job state", () => {
    expect(parseNativeHostResponse({ response: "job_started", job: { job_id: "id" } })).toBeNull();
    expect(
      parseNativeHostResponse({
        request_id: "x".repeat(257),
        response: "error",
        code: "internal",
        message: "Invalid request identifier.",
        retryable: false
      })
    ).toBeNull();
    expect(() => createNativeGetSubtitleCuesRequest("11111111-1111-4111-8111-111111111111", {}, "x".repeat(257))).toThrow(
      "invalid native request identifier"
    );
  });

  it("accepts only credential-free direct web URLs", () => {
    expect(sanitizeDirectMediaUrl("https://media.example.test/audio.mp3#fragment")).toBe(
      "https://media.example.test/audio.mp3"
    );
   expect(sanitizeDirectMediaUrl("blob:https://example.test/id")).toBeNull();
    expect(sanitizeDirectMediaUrl("file:///C:/Users/Alice/meeting.mp4")).toBeNull();
   expect(sanitizeDirectMediaUrl("http://media.example.test/audio.mp3")).toBeNull();
    expect(sanitizeDirectMediaUrl("https://user:secret@example.test/audio.mp3")).toBeNull();
  });

  it("converts only safe local file URLs to absolute paths", () => {
    expect(sanitizeLocalFileUrl("file:///C:/Users/Alice/Videos/meeting%20one.mp4#t=10")).toBe(
      "C:\\Users\\Alice\\Videos\\meeting one.mp4"
    );
    expect(sanitizeLocalFileUrl("file:///var/tmp/meeting.mp4")).toBe("/var/tmp/meeting.mp4");
    expect(sanitizeLocalFilePath("C:\\Users\\Alice\\Videos\\meeting.mp4")).toBe(
      "C:\\Users\\Alice\\Videos\\meeting.mp4"
    );
    expect(isSafeLocalFilePageUrl("file:///C:/Users/Alice/player.html")).toBe(true);
  });

  it("rejects remote, malformed, or non-absolute local file input", () => {
    expect(sanitizeLocalFileUrl("file://recording-server/share/meeting.mp4")).toBeNull();
    expect(sanitizeLocalFileUrl("file:////recording-server/share/meeting.mp4")).toBeNull();
    expect(sanitizeLocalFileUrl("file:///C:/meeting.mp4?copy=1")).toBeNull();
    expect(sanitizeLocalFileUrl("file:///C:/bad%00name.mp4")).toBeNull();
    expect(sanitizeLocalFilePath("\\\\recording-server\\share\\meeting.mp4")).toBeNull();
    expect(sanitizeLocalFilePath("C:relative\\meeting.mp4")).toBeNull();
    expect(sanitizeLocalFilePath("relative/meeting.mp4")).toBeNull();
    expect(isSafeLocalFilePageUrl("https://example.test/player.html")).toBe(false);
    expect(isSafeLocalFilePageUrl("file://recording-server/share/player.html")).toBe(false);
    expect(() =>
      createNativeStartRequest({
        jobId: "extension-invalid-local-job",
        jobKind: "transcript",
        source: { kind: "local_file", path: "\\\\recording-server\\share\\meeting.mp4", mediaKind: "video" }
      })
    ).toThrow("invalid local media path");
  });
});
