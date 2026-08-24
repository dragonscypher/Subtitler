# Native Messaging Protocol

**Status:** target durable-engine contract plus current Phase 8 protocol-v1 record
**Protocol name:** `subtitler.native/v1`

**Implementation status:** The durable-engine protocol described after the
current-record section remains the V1 target. The current Phase 8
implementation is a smaller flat snake_case protocol-v1 shared exactly by the
TypeScript client and Rust host: handshake, start, cancel, status,
`playback_update`, `get_subtitle_cues`, and `get_transcript_segments`. It polls
in-process generic direct/local jobs while a Native Messaging port is
connected. It sends only lossy playback metadata to the scheduler, may deliver
bounded pages of finalized subtitle cues while a job is processing, and makes
completed transcript segments available in separate bounded pages. It never
transports audio, an unbounded transcript body, export paths, source URLs,
browser-session data, or credentials. It does **not** yet implement the
daemon, private pipe, reconnect recovery, browser-session-assisted
direct-source safeguards, or the durable event stream described below.
Subsequent phases must converge the implementation and target through
versioned compatibility tests rather than treating the target as already
shipped behavior.

This document defines the control-plane boundary between the Chrome extension and the Subtitler native companion. It is intentionally small, versioned, and hostile-input safe. The browser extension must not become a media-transfer channel or a durable job runner.

## Current protocol-v1 implementation (Phase 8)

The currently compiled host uses Chrome Native Messaging's native-endian frame
and a flat JSON request rather than the target `Envelope` shape below. Every
actual request has a short generated `request_id` and a `command`:

```json
{
  "request_id": "b75996dd-cab3-4e71-9103-5af8935ec407",
  "command": "playback_update",
  "job_id": "b88997dd-b811-4f92-b63a-f515a05fa439",
  "position_ms": 510000,
  "playback_rate_milli": 1000,
  "is_paused": false,
  "seek_generation": 4
}
```

`playback_update` has scheduling effect only for a non-terminal
known-duration `subtitle_generation` job. `position_ms` is a non-negative
media-timeline integer, `playback_rate_milli` is restricted to 250 through
4,000 (0.25x through 4.0x), and `seek_generation` must increase for a new
seek. The host returns a normal `job_status` response with safe job progress;
it does not return cue text, source information, or media data in response to
a playback update. A full-transcript job ignores the scheduling hint and
remains playback-independent.

Content scripts cannot reach the host. They send a metadata-only snapshot to
the extension service worker: one at startup and on playback control, seek,
rate, and metadata events, plus a two-second interval while playing. The
service worker/native client retains one newest unsent snapshot and sends it
no more often than every 750 ms. This intentional lossiness prevents a
per-frame queue and means an old position has no authority over the newest
seek generation.

### Current generated-cue paging

The existing `get_subtitle_cues` request remains the cue delivery mechanism:

```json
{
  "request_id": "c1e07948-6449-4c11-b2f4-cc79dc2e59af",
  "command": "get_subtitle_cues",
  "job_id": "b88997dd-b811-4f92-b63a-f515a05fa439",
  "cursor": 0,
  "limit": 200
}
```

During `processing`, the host returns the current append-only snapshot of
finalized cues. A page is capped at 200 cues and a 128 KiB serialized response
budget. The host returns `next_cursor` only when more of that current snapshot
exists. Therefore an empty page or an absent `next_cursor` while processing
means “nothing newer is finalized at this cursor,” not “the job completed.”
The extension leaves the cursor parked and refetches after a later status
poll. Once the job is `completed`, it drains the remaining final pages before
notifying completion.

Cue insertion order is stable for cursors but can differ from media-time order
after a seek. The extension validates every cue, then page-locally
deduplicates and sorts it for the overlay. Cue text is not persisted in
`chrome.storage`. A page never includes transcript bodies, exports,
filesystem paths, media URLs, cookies, or headers.

### Current completed-transcript paging

`get_transcript_segments` is deliberately separate from generated subtitle
cues. It is admitted only after a job has completed, so cursors refer to a
stable transcript sorted by media time rather than a changing ASR result:

```json
{
  "request_id": "f5dc6a06-9f6d-456e-b98d-9504630314d3",
  "command": "get_transcript_segments",
  "job_id": "b88997dd-b811-4f92-b63a-f515a05fa439",
  "cursor": 0,
  "limit": 100
}
```

The response is `transcript_segments` with `job_id`, an array of
`{ timing: { start_ms, end_ms }, text, speaker? }`, and an optional
`next_cursor`. `limit` is clamped to 1 through 100. Each serialized page is
limited to 120 KiB; an individual segment is limited to 16 KiB of UTF-8 text
and a 512-byte speaker label. A page omits word timestamps, language and
translation metadata, media details, source URLs, native filesystem paths, and
exports. The extension revalidates these bounds before placing segments in a
bounded, service-worker-memory-only result cache. It never copies transcript
text into `chrome.storage`, and an incomplete result is discarded on a native
disconnect or stop.

### Current popup viewing and browser export (outside Native Messaging)

The popup can request at most 100 cached segments at a time only after the
native client has drained both the completed transcript pages and final cue
pages. It is a transient service-worker result, not a durable record: a worker
restart loses it, and the native host does not send an export path or an
unbounded artifact to the extension.

Export begins only after an explicit popup choice. The extension accepts TXT,
timestamped TXT, SRT, VTT, or JSON and renders the selected output in memory
under a 16 MiB UTF-8 limit. A too-large result fails rather than truncating.
TXT variants use the display segments; SRT/VTT use final cue pages; and browser
`Transcript.json` contains only display-safe `{ start_ms, end_ms, text,
speaker? }` segments. It is not the native engine's canonical word-level JSON
artifact.

The browser export bridge is not a Native Messaging export command. The service
worker creates at most one temporary Blob through the bundled MV3 offscreen
document, invokes Chrome's user-visible fixed-name download/save flow, and
revokes that Blob after terminal download state or a short TTL. The offscreen
document owns no download, storage, native-host, or page-media capability; it
receives transcript text only in the validated background-to-offscreen request.

### Current local-processing advisory

The handshake may include this additive, optional field inside
`capabilities`:

```json
{
  "local_processing_advisory": {
    "selection_source": "automatic",
    "model": "small",
    "quantization": "q5_k_m",
    "backend": "cpu",
    "local_performance": "good"
  }
}
```

All values are fixed snake-case enums. `selection_source` is `automatic` or
`advanced_environment`; model is `tiny`, `base`, `small`, `medium`, or
`large_v3_turbo`; quantization is `q5_0`, `q5_k_m`, `q8_0`, or `f16`; backend
is `cpu`, `cuda`, `metal`, or `vulkan`; and performance is `excellent`,
`good`, `may_be_slow`, or `cloud_helpful`. The field is omitted when a plan
cannot be determined. It contains no model path, memory value, operating
system, GPU/device identity, URL, transcript, or credential. The normal
capability flags remain authoritative for whether local ASR is installed and
runnable; a planning advisory never enables cloud processing, uploads media,
or overrides the job's local-only preference.

### Current scheduler-facing response fields

The host exposes safe scheduling progress through the ordinary `job_status`
response:

```json
{
  "response": "job_status",
  "job": {
    "progress": {
      "media_duration_ms": 5040000,
      "processed_ms": 180000,
      "subtitle_buffer_ahead_ms": 120000
    },
    "message": "Generating subtitles locally. 120s buffered ahead (target 120s)."
  }
}
```

`subtitle_buffer_ahead_ms` is contiguous *completed* source coverage beyond
the latest playhead. In-flight decoding/ASR never counts as usable lead.
Messages report measuring, keeping-up, at-risk, slower-than-playback, or
pause-recommended conditions without disclosing sensitive media information.

## Target durable-engine transport boundary

Everything from this heading onward specifies the intended durable-engine
contract, not the current in-process host implementation, unless a section
explicitly says otherwise.

```text
content script
      | chrome.runtime messaging
      v
MV3 service worker
      | chrome.runtime.connectNative("com.subtitler.native_host")
      v
subtitler-native-host (Chrome stdio framing)
      | authenticated private named pipe / Unix domain socket
      v
subtitler-engine (durable job owner)
```

Chrome starts a native host as a separate process and exchanges UTF-8 JSON over `stdin`/`stdout`, each frame prefixed with a 32-bit native-endian byte length. Native-host-to-Chrome frames cannot exceed 1 MiB; Chrome-to-host frames have a 64 MiB maximum. The Subtitler application protocol is stricter: its serialized envelope is capped at 256 KiB in both directions. This leaves headroom below Chrome's host-output limit and prevents the protocol from becoming a bulk-data path. [Chrome Native Messaging protocol](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)

Only the service worker or extension page calls Native Messaging. Content scripts route through the service worker because Chrome does not expose native messaging to content scripts. The native-host executable writes protocol frames to `stdout` only; diagnostics go to a redacted file log or `stderr`, never `stdout`.

## Security invariants

- The native-host manifest allows the exact released extension ID only. `allowed_origins` does not support wildcards. [Chrome native-host manifest](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)
- The host verifies the Chrome-provided calling origin before it forwards any request. The engine separately authenticates the host over its private local endpoint with an installation secret stored in the OS secret store.
- All envelopes are JSON-schema validated by both sides before use. Unknown fields are rejected for requests and privileged payloads. Native code treats every field as untrusted.
- Every command requires a generated request UUID; every job has an engine-generated UUID. Browser-provided job IDs, file paths, shell fragments, or arbitrary URLs are never accepted as authority.
- No audio, decoded PCM, model file, unbounded transcript, arbitrary export-file bytes, browser cookie, or authentication header travels over Native Messaging. Readable results use explicit byte-bounded pages, and native export paths never leave the engine.
- A short-lived signed/direct media URL may be present in a validated `media.register` request when the authorized page exposes one. It is sensitive, never persisted as a job identity, never echoed in an event, and cannot turn the engine into a generic fetch proxy.

## Envelope

All Native Messaging message bodies use this envelope. The Chrome framing length is outside the JSON body.

```ts
type ProtocolVersion = 1;

type Envelope<TPayload> = {
  protocol: "subtitler.native/v1";
  version: ProtocolVersion;
  id: string;                    // UUID generated by request sender
  kind: "request" | "response" | "event";
  method: string;
  jobId?: string;                // engine-generated UUID when applicable
  sentAtUs: string;              // decimal i64 microseconds; never JS number
  payload: TPayload;
};
```

Time values cross JavaScript as decimal strings, avoiding loss of precision for 64-bit microseconds. Binary data is not represented as Base64 in this protocol.

### Response payloads

Every response has exactly one of the following forms:

```ts
type Success<T> = { ok: true; result: T };

type Failure = {
  ok: false;
  error: {
    code: ErrorCode;
    userMessage: string;          // safe to display after extension localization
    retryable: boolean;
    retryAfterMs?: number;
    details?: Record<string, string | number | boolean>; // never secret/content
  };
};
```

The extension may display `userMessage` but never `details` verbatim unless an error-code-specific renderer permits it. This prevents an upstream URL, FFmpeg error, or token from becoming unescaped extension UI.

## Handshake and reconnection

The service worker opens a long-lived `runtime.connectNative()` port when the popup, overlay, or a job needs the engine. A connection is a subscription transport only. Job ownership remains in the engine.

```text
extension                             host / engine
---------                             -------------
connectNative()
engine.hello  -------------------->   validate origin, protocol, host secret
              <--------------------   engine.hello response (capabilities)
job.list      -------------------->   load durable state
              <--------------------   current job summaries
job.subscribe -------------------->   register event subscription
              <--------------------   job.progress / subtitle.cues / state events
```

If Chrome suspends/restarts the service worker or the port disconnects, no job is cancelled. On reconnect, the extension repeats `engine.hello`, calls `job.list`, then subscribes from its last seen event sequence. The engine may send a compact `state.snapshot` if the requested sequence was pruned. The host converts engine events to Chrome frames but does not persist them.

### `engine.hello`

Request:

```json
{
  "protocol": "subtitler.native/v1",
  "version": 1,
  "id": "3e5d65f0-6078-4e77-a7d5-ebecbd1e5bd0",
  "kind": "request",
  "method": "engine.hello",
  "sentAtUs": "1786986670000000",
  "payload": {
    "extensionVersion": "0.1.0",
    "clientInstanceId": "d8502b9f-0064-4e50-96e8-8ad2a8d1120d",
    "supportedProtocolVersions": [1]
  }
}
```

Success result:

```json
{
  "ok": true,
  "result": {
    "selectedProtocolVersion": 1,
    "engineVersion": "0.1.0",
    "engineState": "ready",
    "capabilities": {
      "localAsr": true,
      "cloudProviders": ["openai", "openrouter", "openai-compatible"],
      "availableBackends": ["cpu"],
      "diarization": false,
      "maxConcurrentJobs": 1
    }
  }
}
```

The response contains no source URLs, model paths, operating-system username, hardware serial number, transcript content, or secret-bearing detail.

## Method catalog

### Engine and subscription methods

| Method | Direction | Purpose |
| --- | --- | --- |
| `engine.hello` | extension -> engine | version/capability handshake |
| `engine.status` | extension -> engine | companion readiness, safe hardware summary, model/download state |
| `job.list` | extension -> engine | bounded job summaries, excluding transcript text |
| `job.get` | extension -> engine | one job's current status and non-secret view state |
| `job.subscribe` | extension -> engine | register for event stream from a sequence number |
| `job.unsubscribe` | extension -> engine | stop a subscription; never stops a job |
| `engine.ping` | extension -> engine | connection health only |

### Media and job methods

| Method | Direction | Purpose |
| --- | --- | --- |
| `media.register` | extension -> engine | register a schema-validated candidate and bounded access descriptor; returns `mediaId` |
| `job.create` | extension -> engine | atomically persist a transcript or subtitle job; returns `jobId` before work starts |
| `playback.update` | extension -> engine | current playhead, rate, pause/seek generation for subtitle priority only |
| `job.cancel` | extension -> engine | request cooperative cancellation and cleanup |
| `job.delete` | extension -> engine | delete a completed/cancelled job and eligible retained result/cache data |

### Result and export methods

| Method | Direction | Purpose |
| --- | --- | --- |
| `job.transcript_page` | extension -> engine | bounded chronological page of transcript segments/words |
| `job.subtitle_page` | extension -> engine | bounded cue page for reconnect/cue cache recovery |
| `job.export` | extension -> engine | user-initiated export to TXT, timestamped TXT, SRT, VTT, or JSON |
| `job.set_overlay` | extension -> engine | toggle overlay delivery for a tab/job binding |

`media.register` may include a direct media URL with no embedded browser credential or a local file reference that has passed the extension's source validation. The engine revalidates URL scheme, origin, redirects, size/duration limits, and media kind; registration is not permission to fetch arbitrary URLs. A signed URL is treated as sensitive and is never placed in event payloads or job summaries.

The engine never accepts browser cookies or authorization headers. When a source needs a browser session, the adapter may use the authorized page to resolve a direct representation whose URL contains no browser credentials (for example, a short-lived platform URL, treated as sensitive) or invoke the platform's ordinary download path. If neither is available, the engine reports the source as inaccessible rather than attempting a credential handoff.

### `job.create` example

```json
{
  "protocol": "subtitler.native/v1",
  "version": 1,
  "id": "5d890271-d91b-4b66-b8e0-3630e73c4487",
  "kind": "request",
  "method": "job.create",
  "sentAtUs": "1786986671000000",
  "payload": {
    "mediaId": "5e07b4a1-a5bf-4a87-a088-dd88c0a185c3",
    "kind": "subtitle_generation",
    "tabBinding": { "tabId": 71, "frameId": 0 },
    "existingCaptions": "prefer",
    "generationMode": "generate-with-subtitler",
    "languageOutput": "english",
    "quality": "automatic",
    "diarization": "off",
    "cloud": { "mode": "local-only" }
  }
}
```

The engine commits a new job record and stable `jobId` before acknowledging success. A retry with the same `id` is idempotent while its request record is retained. `tabBinding` is routing metadata only; the native engine never tries to operate a browser tab.

### Target durable-engine `playback.update` example

```json
{
  "protocol": "subtitler.native/v1",
  "version": 1,
  "id": "2b269e33-8bdd-477e-9e73-8a6194cccd91",
  "kind": "request",
  "method": "playback.update",
  "jobId": "b88997dd-b811-4f92-b63a-f515a05fa439",
  "sentAtUs": "1786986672000000",
  "payload": {
    "positionUs": "510000000",
    "playbackRate": 1,
    "isPaused": false,
    "seekGeneration": 4
  }
}
```

Updates are lossy by design: only the newest position/rate/seek generation matters. The engine rate-limits them and never creates a database row per frame.

## Event catalog

Events are idempotent, monotonically sequenced per job, and include `eventSeq` inside `payload`. The extension discards duplicates and detects gaps.

| Event | Key payload | Consumer behavior |
| --- | --- | --- |
| `job.state` | state, cause, retryability | update popup/overlay status |
| `job.progress` | source coverage, decoded/transcribed/export progress | render concise progress without source URLs |
| `subtitle.cues` | revision, bounded list of final/provisional cues | route to the bound overlay; cache only a bounded near-playhead window |
| `subtitle.buffer` | contiguousCoverageEndUs, leadUs, estimated real-time factor | show buffer health/recovery state |
| `transcript.available` | first/last segment index, revision | fetch a bounded page when UI needs it |
| `job.warning` | stable warning code and safe user text | show non-blocking actionable notice |
| `job.error` | structured `Failure.error` | display clear failure and next action |
| `state.snapshot` | compact current job/cue ranges | replace a stale subscription after reconnect |

An event must be below 256 KiB. `subtitle.cues` batches by serialized byte size, not only cue count. A long transcript page has an explicit byte-limited cursor and must never be sent unbounded.

## Job states and cancellation

```text
queued -> resolving -> acquiring -> decoding -> transcribing -> segmenting
       -> buffering | completed | failed | cancelled
```

`buffering` is an overlay-focused state in which current-playhead coverage is insufficient but an active job is making progress. A full transcript may remain `transcribing` while a shared subtitle job is `buffering` around a seek.

Cancellation is cooperative and durable:

1. The extension sends `job.cancel` and receives an acknowledgement that cancellation was recorded.
2. The engine signals fetch/decoder/ASR tasks at safe boundaries, prevents new work, and emits state transitions.
3. The engine deletes temporary media/audio unless the user explicitly retained it, then emits `cancelled`.

A port loss never implies cancellation. A failed process is recovered through the persisted job state and explicit retry policy, not an opaque host restart loop.

## Error taxonomy

The following stable codes are sufficient for V1 UI behavior; they are not raw library errors:

```text
companion_not_installed         companion_version_mismatch
engine_start_failed             engine_busy
media_not_detected              media_not_accessible
media_protected                 media_expired
media_unsupported_codec         media_corrupt
browser_access_required         browser_access_not_available
permission_required             file_access_not_enabled
storage_insufficient            memory_insufficient
model_missing                   model_download_failed
local_processing_too_slow       cloud_consent_required
cloud_provider_failed           export_failed
job_cancelled                   internal_error
```

Errors carry only stable facts needed for recovery, for example `requiredPermission: "https://example.com/*"` or `minimumFreeBytes`. Raw URLs, HTTP authorization headers, cookie values, transcript text, and FFmpeg command lines are never returned to the extension.

## Native host registration and local engine IPC

The packaged native-host manifest is conceptually:

```json
{
  "name": "com.subtitler.native_host",
  "description": "Subtitler local processing bridge",
  "path": "<installer-owned native host binary>",
  "type": "stdio",
  "allowed_origins": ["chrome-extension://<released-extension-id>/"]
}
```

Windows installation registers the manifest path under the current user's Chrome Native Messaging host registry key. Chrome documents this registration mechanism and separately looks up 32-bit then 64-bit registry locations. macOS and Linux use browser-specific `NativeMessagingHosts` locations. The installer performs these operations; the extension never writes registry keys or binaries. [Chrome host locations](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)

Host-to-engine local IPC uses a separate, internal binary framing with the same logical schema but adds `hostInstanceId` and proof of the installation secret. It is not reachable over localhost/TCP. The engine rejects peers that do not satisfy both OS user access control and protocol authentication. The native host creates or reconnects to the engine, then relays only validated messages.

## Compatibility, test, and observability rules

- Only one protocol version is active in V1. An incompatible `engine.hello` response produces a clear install/update action, never undefined behavior.
- Schema fixtures live in both TypeScript and Rust tests. Golden-frame tests verify native-endian framing, UTF-8 byte sizes, 256 KiB caps, malformed lengths, unknown fields, duplicate request IDs, and redaction.
- Integration tests cover: missing host; fresh engine launch; worker reconnect; engine restart with retained job; port disconnect during transcript; rejected cookie/header payload; rejected private-network redirect; cancellation cleanup; and output larger than a native-message frame.
- Logs use job IDs and stable error codes. A redaction test asserts that fixture URLs with query tokens, authorization strings, media bytes, transcript text, and environment secrets never appear in logs.
