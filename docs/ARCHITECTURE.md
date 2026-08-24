# Subtitler V1 Architecture

**Status:** Phase 1 decision record
**Scope:** Chrome and prerecorded media. This document deliberately does not promise support for live meetings, DRM-protected media, or bypassing a platform's authentication, encryption, or access controls.

**Implementation status:** This is the target V1 architecture, not a claim that
every component already exists. The current build contains the Phase 2 MV3
extension/native-host foundation, a Phase 3 generic local path (a direct HTTPS
source is first acquired through a controlled, DNS-pinned downloader into a
private local artifact; a user-opened local file remains local; FFmpeg then
performs audio-only normalization into a private WAV artifact; a
configuration-gated whisper.cpp CLI adapter parses timestamps and produces
deterministic exports), the Phase 4 overlay handoff, a bounded Phase 5
ahead-of-playhead path, a narrow Phase 6 platform layer, a bounded Phase 7
local-first planning/consent layer, and a Phase 8 result-delivery slice. For a
generated-subtitle job with a positive media-duration hint, the in-process host
accepts lossy timeline updates, schedules bounded local audio ranges around the
playhead, exposes contiguous completed-buffer depth and pacing status,
cooperatively preempts an obsolete active range after a seek, and pages
finalized cues while processing. Phase 6 adds strict in-page YouTube
existing-caption overlay support, safe Webex/Zoom recording-route recognition,
and safe local-file-tab routing; it does not add platform media extraction.
Phase 7 adds a conservative hardware-derived advisory, an in-memory popup
rendering of it, and a no-upload cloud consent contract; it does not ship a
model manager, a cloud uploader, or diarization. Phase 8 makes chronological
transcript segments available only after completion in bounded native pages,
drains the final cue pages, retains the result only in a bounded
service-worker-memory cache, and exposes a lazy popup reader plus explicit
browser downloads. It does not make results durable across a service-worker
restart, test a real Chrome save dialog, or turn the browser into a
media/credential bridge. This is tested through deterministic fixtures and
injected process runners, not a bundled FFmpeg/model, browser automation,
authenticated platform sessions, or a licensed real-media performance corpus.
The durable `subtitler-engine`, private local IPC, packaged model management,
full platform acquisition, and benchmark-backed performance rules remain
planned work.

## Executive decision

Subtitler is a local-first system with a deliberately small browser extension and a durable native engine. The extension detects media, presents the two primary actions, observes playback, finds existing captions, and renders an overlay. The native engine owns every long-running or resource-intensive concern: acquisition, decoding, audio normalization, ASR, timing, export, caching, cleanup, and scheduling.

The extension is **not** an ASR runtime and it is **not** a general-purpose browser network proxy. The primary control plane is Chrome Native Messaging; a per-user native engine persists jobs independently of the popup and of the Manifest V3 service worker lifetime. Native Messaging carries validated commands, short-lived direct-media descriptors, status, and small pages of results only. It never transports media, complete transcripts, browser cookies, or authentication headers.

```text
Chrome tab
  Page probe + platform adapter        Subtitle overlay
             |                               ^
             v                               |
MV3 service worker <---- typed messages ---- content script
       |  Native Messaging (control plane, stdio)
       v
Subtitler native host  <---- authenticated local IPC ---->  Subtitler Engine
                                                             |
                  +------------------------------------------+-----------------+
                  |                    |                     |                 |
              acquisition/FFmpeg    job scheduler        ASR + VAD       subtitle/export
                  |                    |                     |                 |
                media cache        SQLite metadata     whisper.cpp      SRT/VTT/TXT/JSON
```

This boundary makes the product responsive, keeps sensitive media on the device by default, and prevents a browser-popup lifetime from deciding whether a two-hour transcript completes.

## Product boundaries and success criteria

V1 accepts a supported prerecorded source only when Subtitler can obtain captions, a direct media representation, a platform-provided download, or an explicitly authorized, non-DRM browser-session request. It must never attempt to decrypt EME/DRM content, derive platform-protected signing algorithms, or evade a source's controls.

The two user-visible actions remain:

```text
[ Create Subtitles ]       -> durable SubtitleGenerationJob
[ Get Full Transcript ]    -> durable FullTranscriptJob
```

Both jobs share the same normalized, timestamped transcript cache. A subtitle job prioritizes coverage around the active playhead; a transcript job independently covers the entire source. Existing reliable captions are used immediately and the UI separately offers **Generate with Subtitler**.

## Final component design

### Extension: Chromium Manifest V3

The extension is TypeScript in `strict` mode and uses Chrome Manifest V3. V1 uses standards-based DOM/Web Components and CSS rather than a large UI framework; esbuild produces the extension bundles; hand-written exhaustive runtime guards validate messages at every untrusted boundary. Vitest covers pure TypeScript logic. A Playwright-driven Chromium extension suite is planned for later browser integration phases, after the skeleton has a packaged extension entry point.

Extension responsibilities are deliberately narrow:

- **Popup:** detects the current page, shows the two primary actions, current job status, and a compact advanced entry point. For a completed transcript held by the current service-worker lifetime, it can lazily request display pages and explicitly choose one export format. It never owns a job or receives a native filesystem path.
- **Service worker:** owns the Native Messaging port, routes schema-validated messages, persists only non-sensitive UI/session metadata in `chrome.storage`, reconnects after suspension, and rehydrates state from the engine. The current Phase 8 result cache is a separate, bounded in-memory cache; transcript text, cues, and Blob URLs are deliberately not persisted.
- **Content scripts:** probe `<video>`, `<audio>`, `<track>`, duration, `currentTime`, playback rate, fullscreen changes, and caption DOM. For an active generated-subtitle job, a small observer forwards timeline metadata only: an initial snapshot, immediate control/seek/rate snapshots, and an interval while playing. The current YouTube adapter uses a narrowly scoped, ephemeral MAIN-world caption operation only; platform adapters do not become an unrestricted scraper or a network/session harvester.
- **Overlay:** a Shadow DOM custom element attached to the media player's fullscreen-capable container where possible. It receives only timing and subtitle segments, does not inspect credentials, and cannot intercept player controls.
- **Shared package:** discriminated TypeScript message types, media descriptors, job view models, runtime validators, and error codes.

The current manifest uses `activeTab`, `scripting`, `storage`, `nativeMessaging`,
`downloads`, and `offscreen`. `downloads` is used only after an explicit
completed-transcript export action with a fixed product filename and Chrome's
save dialog. `offscreen` is created with the `BLOBS` reason only, so the
bundled document can create and revoke the temporary Blob URL that backs that
one download. It has no Subtitler path to native messaging, extension storage,
page media, or the downloads API. Site access is requested at the moment a
user asks Subtitler to work on a site through `optional_host_permissions`. V1
does not request Chrome's `cookies`, `debugger`, or network-interception
permissions. Chrome recommends optional permissions and `activeTab` where
possible, rather than broad permanent access. [Chrome permission guidance](https://developer.chrome.com/docs/extensions/develop/concepts/declare-permissions) [Chrome privacy guidance](https://developer.chrome.com/docs/extensions/develop/security-privacy/user-privacy) [Chrome downloads API](https://developer.chrome.com/docs/extensions/reference/api/downloads) [Chrome offscreen API](https://developer.chrome.com/docs/extensions/reference/api/offscreen)

### Native companion and durable engine

The target companion is Rust. The current workspace explicitly declares Cargo edition 2021 and Rust 1.78 as its minimum supported compiler; release packaging will pin a tested toolchain rather than letting builds drift. The target V1 process topology consists of two processes:

1. **`subtitler-native-host`** is the very small Chrome stdio adapter registered as `com.subtitler.native_host`. It authenticates the calling extension through Chrome's native-host manifest and forwards validated protocol messages to the engine.
2. **`subtitler-engine`** is one per-user background process. It owns the scheduler and the database, survives a popup close and Native Messaging reconnection, and exposes no TCP listener. The host reaches it over a per-user named pipe on Windows or a permissions-restricted Unix domain socket on macOS/Linux.

The engine starts on demand, uses an interprocess single-instance lock, and exits only after no running jobs and a modest idle period. Its local IPC endpoint is protected with a user-only ACL/mode plus an installation secret held in the OS secret store. This is defense in depth against an unrelated local process; code executing as the same OS user remains a meaningful local-machine threat.

The Rust stack is intentionally conventional:

| Concern | Chosen technology | Reason |
| --- | --- | --- |
| Async orchestration | Tokio | bounded async I/O, cancellation, process supervision |
| Serialization/contracts | Serde + `serde_json` + JSON Schema snapshots | stable native/extension protocol and test fixtures |
| Error handling/logging | `thiserror`, `tracing`, redaction layer | typed recoverable failures; no content/token logs |
| Persistence | SQLite through `rusqlite` | transactional job/segment metadata with no server dependency |
| Secrets | OS key store (DPAPI/Keychain/libsecret) with `secrecy`/zeroization | API keys and local-IPC secret never enter SQLite or logs |
| HTTP and URL validation | `reqwest`, `url`, Rustls | bounded, validated, TLS-only acquisition/provider traffic |
| Media decode | a pinned FFmpeg distribution invoked with direct process arguments | mature codec/container support without putting a shell in the trust boundary |
| ASR | `whisper.cpp` built as a narrow C/C++ FFI dependency | efficient local Whisper-compatible inference with CPU/GPU backend variants |
| VAD | WebRTC VAD behind a Rust trait | lightweight speech/silence boundary signal, not a second transcription model |
| Optional diarization | ONNX Runtime C API behind a feature flag | permits a local segmentation/embedding pipeline without Python in the installed runtime |

FFmpeg is launched with `std::process::Command`, explicit argument vectors, a fixed executable path, bounded environment, and no shell. Release packaging must document FFmpeg and model licenses, pin their source hashes, and build/ship only legally compatible codec configurations.

### Canonical data model

Every time value is a signed 64-bit integer number of microseconds in the media timeline. Browser seconds are converted at the boundary; floating-point seconds are never used as cache keys or equality tests.

```text
MediaSource { source_id, fingerprint, duration_us?, kind, access, platform }
Job { job_id, source_id, kind, state, model_plan, language_plan, progress }
AudioRange { start_us, end_us, representation_id, cache_state }
TranscriptWord { start_us, end_us, text, confidence?, speaker? }
TranscriptSegment { start_us, end_us, text, words[], speaker?, provenance }
SubtitleCue { start_us, end_us, lines[1..2], segment_revision }
```

`fingerprint` is a privacy-preserving local digest of stable source characteristics (canonicalized origin/path without query tokens where possible, duration, representation metadata, and a short audio fingerprint when available). Raw signed URLs and source cookies are not used as persistent identifiers.

## Native Messaging and job control

The exact wire contract is in [NATIVE_MESSAGING_PROTOCOL.md](NATIVE_MESSAGING_PROTOCOL.md). The important architectural choices are:

- The extension service worker maintains one `runtime.connectNative()` port for control/status while Chrome keeps it available. Content scripts never call the native host directly; Chrome limits that API to extension pages and service workers. [Chrome Native Messaging](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)
- Chrome Native Messaging is UTF-8 JSON framed by a native-endian 32-bit byte length. A native-host-to-Chrome message is limited to 1 MiB, so the protocol caps individual events at 256 KiB and pages transcript data. It sends no audio, model data, export payloads, browser cookies, or authentication headers over this channel. [Protocol limits](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)
- The native host manifest names the exact production extension ID in `allowed_origins`; Chrome does not allow wildcards there. The installer registers the native host at the user level by default. [Host manifest requirements](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)
- A `hello`/capability handshake is required before work. Each command has a UUID request ID, an engine-generated job ID, a protocol version, and a schema-validated payload. Unknown fields are rejected for security-sensitive messages.
- Native Messaging is a **control plane**, not a durable job queue. The engine persists job state before acknowledging creation. On a port reconnect, the service worker calls `job.list`/`job.subscribe` and derives its UI again.
- Large results are accessed as bounded, paginated transcript/segment pages; a full transcript is never pushed through the port. In the current host, `get_transcript_segments` is completion-only and contains display-safe timing/text/speaker fields, while a completed full-transcript job also drains final cue pages for subtitle-file rendering. The development host's private export bundle is not exposed as a browser path. Browser export is a separate explicit popup action: the service worker renders one bounded in-memory format, asks a bundled offscreen document for a temporary Blob URL, and starts a fixed-name Chrome download/save flow. The browser JSON is deliberately a segment-only display format, not the canonical native word-level artifact.

### Current Phase 8 boundary

The shipped development host is still an in-process native-host job registry,
not the durable two-process engine shown above. It owns jobs only while the
host remains alive and polls them while its Native Messaging port is connected.
The actual flat protocol-v1 implementation includes `playback_update`,
progressive `get_subtitle_cues` pages, and completion-only
`get_transcript_segments` pages; its exact wire shape is documented in
[NATIVE_MESSAGING_PROTOCOL.md](NATIVE_MESSAGING_PROTOCOL.md). The playback
message contains position, rate, paused state, and a monotonically increasing
seek generation—never audio, URLs, cookies, headers, cue text, or transcript
data. The extension holds at most one unsent snapshot and the host treats the
updates as advisory, lossy scheduling hints.

For a successful transcript job, the native client drains bounded chronological
segment pages and the final bounded cue pages before it marks the extension
result readable. The service worker holds that result only for its current
lifetime under segment, cue, and aggregate-text limits; the popup retrieves at
most 100 transcript segments per display request. Stops and native-host
disconnects discard incomplete results. A user can then choose TXT,
timestamped TXT, SRT, VTT, or JSON. The renderer fails instead of truncating an
output above 16 MiB, permits one active save operation, and owns the Blob URL
only until the terminal download event or a short independent cleanup TTL.
This current extension path has unit coverage, but has not yet been proven in a
manual Chrome download/save-dialog run.

## Media acquisition architecture

### Adapter contract and ordered strategy

Every source is probed through a common adapter contract:

```text
probe(page) -> MediaCandidate[]
find_existing_captions(candidate) -> CaptionTrack[]
resolve_access(candidate, user_authorization) -> MediaAccess | Unsupported
```

`MediaAccess` is constrained to one of these forms: `existing-captions`, `direct-url` (including a time-limited signed URL), `local-file`, or `platform-download-handoff`. It includes an expiry, expected origin/representation, and a maximum intended byte/time range. It is not an arbitrary URL supplied by page JavaScript.

The engine uses the following strategy in order:

1. **Existing captions.** Discover `<track kind="subtitles|captions">`, platform caption endpoints exposed to the authorized page, and accessible rendered caption metadata. Validate timing, language, and cue order; display credible captions immediately. Preserve the source label and offer **Generate with Subtitler**.
2. **Direct accessible audio/video representation.** Prefer a source's direct non-DRM audio URL, media track, local file URL with user-enabled file access, or a signed URL that the page explicitly exposes. Request only audio from FFmpeg/decoder; no full video decode is needed.
3. **Platform download handoff.** If a Webex, Zoom, or similar authorized recording offers its own normal download action, direct the user through that platform-provided action and transcribe the resulting local file. This preserves the browser session without exporting credentials.
4. **Browser-session-assisted resolution without credential handoff.** An adapter may use the already-authorized page to discover an accessible, time-limited direct representation or to initiate the platform's ordinary download flow. It never copies Chrome cookies or authorization headers to the engine. If the browser cannot provide a lawful direct representation or download, the source is unsupported.
5. **Unsupported.** Return an actionable cause: DRM/encrypted media, no accessible representation, expired signed URL, insufficient permission, or unsupported player.

The target V1 adapters are `GenericHtml5Adapter`, `YouTubeAdapter`,
`WebexAdapter`, and `ZoomAdapter`. They are intended to implement the same
contract and share test fixtures. An adapter may support caption discovery even
when it cannot safely resolve raw media.

### Current Phase 6 platform-adapter boundary

Phase 6 intentionally delivers only a small, caption-first subset of that
target:

| Page/source | Current behavior | Explicitly not implemented |
| --- | --- | --- |
| Strictly recognized YouTube video route | Discover one existing English caption track, prefer manual over ASR, use a bounded authorized-page timed-text fetch, and render the received cues in the current page's overlay. | Direct video/audio discovery, `streamingData` inspection, native-engine media transfer, full-transcript export from an opaque player, caption persistence, or translation. |
| Recognized Webex/Zoom recording route | Return a safe platform label and an actionable no-bypass explanation if the player is opaque or protected. | Platform caption discovery, authenticated media resolution, download handoff, cookie/header transfer, and direct platform extraction. |
| Generic HTML5 media | Retain the existing caption-track path, a direct HTTPS path that is acquired through the controlled downloader before FFmpeg sees it, and a user-opened safe local-file-tab path. A normal direct source can enter the local pipeline even if its page happens to be a recognized platform. | Treating a `blob:`/MSE/EME source as direct media, proxying an arbitrary page request, a network/UNC file path, or an arbitrary pasted filesystem path. |
| Opaque or protected player | Reject generated/transcript acquisition with clear guidance. A visible existing caption overlay remains permissible because it does not acquire the protected media. | DRM/key extraction, credential copying, signature deciphering, screen/audio capture, or access-control bypass. |

For YouTube, the service worker runs an allowlisted function in the active
page's MAIN world. That function copies only fields from the fixed
`captions.playerCaptionsTracklistRenderer` path, and the later fetch accepts
only a validated HTTPS `/api/timedtext` endpoint with redirects disabled,
bounded response size, and `cache: "no-store"`. The page's authorization may
be used by that one fetch, but cookies, tokens, the caption endpoint, player
response, and caption text are never persisted in `chrome.storage`, logged, or
sent to Native Messaging. Captions are chunked directly into the page-local
overlay and discarded with the active page/job state.

Route recognition is deliberately not proof that a recording, captions, or a
raw stream is accessible. It accepts only exact HTTPS platform-domain
boundaries and narrow video/recording paths, does not retain URL query data,
and falls back to generic handling otherwise. The detailed delivery and gap
record is in [PHASE_6_PLATFORM_ADAPTERS.md](PHASE_6_PLATFORM_ADAPTERS.md).

For the current generic remote path, the native host resolves and validates
every DNS answer at every manual redirect hop, pins that hop to the approved
addresses, streams the complete object to a capped private temporary file, and
then gives FFmpeg that file only. This closes the direct-URL DNS/redirect
boundary without exporting browser credentials. Its intentional present-day
tradeoff is that a large remote recording must finish staging before the first
subtitle range can be processed; HLS/DASH manifests fail closed until a
segment-aware controlled acquirer exists. A local-file start is accepted only
from a safe user-opened `file:` tab with Chrome file access enabled, and both
the extension and native host reject UNC/device/network paths.

### Important browser and platform limitation

An extension does not have a universal, safe API that turns every `<video>` playback request into a native HTTP stream with all of the browser's ambient authentication, DRM keys, service-worker state, and CORS privileges. A `blob:` `currentSrc`, an MSE buffer, or an EME-protected player is not a recoverable audio URL. Extension cross-origin requests require explicit host permissions, while content scripts remain subject to the page origin's restrictions. [Chrome cross-origin request model](https://developer.chrome.com/docs/extensions/develop/concepts/network-requests)

Consequences:

- Never implement signature deciphering, DRM/key extraction, cookie-database scraping, a generic DevTools-network harvester, or hidden screen/audio capture as a workaround.
- Do not promise generated transcription for every YouTube/Webex/Zoom page. V1 provides reliable detection and existing captions first; generated output is offered only when a lawful, accessible representation or platform download is available.
- A platform adapter can use the user's already-authorized session only when the platform and browser make the necessary request data available without defeating a control. If it cannot, it explains the limitation instead of asking for a redundant password or silently failing.
- Direct URLs are HTTPS-only by default; localhost, link-local, private-network, custom-scheme, and redirect-to-private-address targets are rejected unless they are an explicitly user-picked local file. Redirects are bounded and revalidated at every hop.

### Decode, cache, and cleanup

The media pipeline is:

```text
validated access descriptor
  -> bounded fetch/range reader or platform download
  -> FFmpeg demux/decode audio only
  -> mono 16 kHz PCM normalization
  -> VAD marks + overlapping speech windows
  -> ASR words/segments
  -> subtitle segmentation + persisted time ranges
```

The engine prefers range requests/HLS segments and pipes decoded PCM between processes. It spills to an app-private cache only when seeking, decoder behavior, retry, or source limitations require it. Temporary files get user-only permissions, random names, a fixed job-scoped directory, a size quota, and deletion on success/cancel/failure. A startup reaper removes expired orphaned jobs. “Keep media” is an explicit advanced choice; it is off by default.

## ASR and language strategy

`SpeechProvider` is a stable internal interface:

```text
capabilities() -> ProviderCapabilities
plan(source, hardware, requested_quality) -> ModelPlan
transcribe(range, audio, language_plan, cancellation) -> TimedTranscript
cancel(job_id)
```

### Local default

`LocalWhisperProvider` is the default and uses whisper.cpp with model files managed by the engine. It uses English-only models for known English input and multilingual models when language detection says a supported non-English input is likely. V1's target language is fixed to English:

- English source -> English transcript/subtitles.
- Supported non-English source -> source-language recognition plus Whisper-compatible translation-to-English output.
- Uncertain language -> show the detected language and require a lightweight confirmation before a long job if translation materially changes the result.

The initial model policy is a benchmark-tuned tier, not a hard-coded hardware claim:

| Situation | Initial selection | Rationale |
| --- | --- | --- |
| Strong GPU / ample memory | `medium`/`medium.en` quantized model | prefer materially better accuracy where real-time factor is safe |
| Typical recent CPU | `small`/`small.en` quantized model | V1 quality default |
| Constrained CPU or low memory | `base`/`base.en` quantized model | responsive fallback with visible quality trade-off |
| Unsupported/insufficient local plan | no automatic upload | offer pause-to-buffer, smaller local model, or explicit cloud choice |

Model names and quantizations are release-manifest data, selected after reproducible benchmarks, not UI controls in the normal flow. The hardware planner combines OS, architecture, physical/available memory, supported compiled backend, and a small local benchmark. CUDA, Metal, and other accelerated builds are optional distribution variants; the engine never claims a GPU backend merely because a device name is present.

### Current Phase 7 implementation

The current host has a conservative, deterministic planner rather than the
target's benchmark-backed model manager. It observes logical CPU count and
available system memory, and accepts a non-CPU backend only when an
installer/development environment explicitly declares that the installed build
supports it. It does not enumerate GPU names, drivers, or VRAM. The handshake
may expose a deliberately coarse, in-memory-only local-processing advisory;
the ASR/FFmpeg availability flags remain authoritative. Until a signed model
manifest owns asset metadata, an actual development run requires an explicit
matching model/quantization/backend triple and fails closed on partial values.
The delivered policy and its remaining limits are recorded in
[PHASE_7_LOCAL_FIRST_PROCESSING.md](PHASE_7_LOCAL_FIRST_PROCESSING.md).

ASR runs on overlapping, speech-aware ranges. VAD skips only confidently long silence and creates useful boundaries; it never removes low-energy speech by itself. Overlaps are reconciled by token/word agreement, then timing is normalized into the canonical media timeline. Word timestamps are retained where the engine can generate/align them; lower-confidence inferred word timings are flagged internally so the segmenter can choose conservative boundaries.

## Subtitle generation and ahead-of-playhead scheduling

### Segment construction

`subtitler-subtitles` turns timed words into cues, rather than putting arbitrary ASR chunks on screen. It uses a deterministic boundary scorer (punctuation, silence, sentence/phrase likelihood, word confidence) and constrained dynamic programming over valid word boundaries. Constraints are release-configured and benchmarked, including two maximum lines, language-aware line length, reading speed, minimum/maximum cue duration, gap handling, and avoidance of isolated function words. The same canonical segments export valid SRT, WebVTT, plain TXT, timestamped TXT, and JSON.

### Scheduler

For a playing source, `covered(t)` is the highest contiguous, finalized subtitle timestamp after the current playhead `p`. The user-visible lead is:

```text
lead_us = covered(p) - p
```

The scheduler maintains an interval map of decoded, transcribed, and finalized ranges, then scores work as follows:

1. Existing captions or cached Subtitler segments covering `p` win immediately.
2. On start/seek, enqueue a small cursor window containing `p` plus the nearest future subtitle buffer. This preempts queued work that does not protect the new playhead.
3. Build contiguous coverage out to an adaptive target lead. Source byte/range capability determines whether this can be a true seek-ahead fetch; sequential-only sources are explicitly marked as limited.
4. Continue expanding the horizon while observed processing speed has sufficient margin over playback rate. A full-transcript job remains playback-independent; a future shared scheduler may allocate spare capacity, but it must never make transcript progress depend on a subtitle cursor.
5. Finalize a cue revision only when neighboring context is stable. Do not rewrite displayed cues except for a clearly marked error recovery; future cues may improve as more context arrives.

At each update, the engine estimates media-seconds processed per wall-second `r` and playback rate `v`. If `r <= v`, it computes predicted lead exhaustion and reports it before subtitles drift. If `r > v`, it targets a lead sized from recent variance, seek latency, and available cache quota, clamped by policy. Initial values are tuned by the benchmark suite rather than embedded as a product promise. The UI offers a concrete recovery choice before the lead runs out:

- pause briefly to build the selected buffer;
- switch to an available faster local model, with the quality change stated;
- choose a cloud provider after an explicit data-transfer disclosure.

The overlay uses `HTMLVideoElement.currentTime` as the timeline authority, `requestVideoFrameCallback` when the browser supports it, and animation-frame/timeupdate fallbacks. It binary-searches cues by timestamp on each relevant update; pause, rate changes, and seeks therefore need no transcript re-generation if coverage already exists.

### Current Phase 5 implementation

For a `subtitle_generation` job with a positive media-duration hint, the
native host creates a pure `SubtitleBufferScheduler`. Its initial,
benchmark-tunable settings are deliberately explicit:

| Setting | Current value | Meaning |
| --- | --- | --- |
| Minimum usable lead | 30 seconds | Below this contiguous completed lead, the scheduler treats playback as under-buffered. |
| Preferred lead | 120 seconds | Normal target; a job setting may change this value only within the minimum/maximum bounds. |
| Maximum useful lead | 300 seconds | Target while playback is paused, and as a cushion for very fast or slower-than-playback observed processing. |
| Scheduled source range | 30 seconds | One FFmpeg/ASR task; the media layer separately rejects a range longer than 15 minutes. |
| Context before a new playhead | 5 seconds | Allows a cue that began just before a seek target to be regenerated. |

The scheduler leases non-overlapping ranges and counts only completed ranges in
`subtitle_buffer_ahead_ms`. It records wall-clock processing samples to
calculate real-time factor and reports measuring, keeping-up, at-risk,
cannot-keep-up, or pause-recommended states. A seek generation increase
preempts outstanding subtitle leases and cooperatively cancels the active
FFmpeg/ASR range where possible; a valid late completion can still contribute
coverage. The host decodes the selected half-open range directly to a private
canonical WAV artifact, offsets chunk-local ASR timestamps back onto the media
timeline, and publishes final cue pages incrementally.

This is not an ordinary live-caption loop. No browser audio is captured, and
the host does not process a full recording merely to reach a nearby playhead.
When the current adaptive target is covered, it waits for a later playback
observation instead of processing unrelated media. Consequently, a subtitle
job remains active until its coverage reaches the full recording. A
`full_transcript` job deliberately creates no playback scheduler and continues
through the complete-media path independently of every playhead update.

The bounded implementation has important gaps: it does not yet provide a
durable range/cue cache, a private engine IPC service, HTTP range-capability
negotiation, cross-chunk revision reconciliation, browser end-to-end tests, or
benchmark-derived performance guarantees. See
[PHASE_5_AHEAD_OF_PLAYHEAD.md](PHASE_5_AHEAD_OF_PLAYHEAD.md) for the precise
delivery and limit record.

## Speaker attribution

Speaker handling is a progressive enhancement, never a blocker:

1. Keep reliable platform-provided names only when their source marks them as such.
2. If the user enabled diarization and the hardware/model plan is feasible, run local segmentation plus speaker-embedding clustering after (or concurrently with) ASR. Emit stable `Speaker 1`, `Speaker 2`, etc.; do not guess human names.
3. If diarization is unavailable, too slow, too uncertain, or conflicts with subtitle responsiveness, return an unlabeled transcript.

The local diarization implementation is an optional ONNX model package with a licensing and quality gate. It is scheduled below cursor-protecting ASR and is disabled by default for subtitle-only jobs. No general-purpose LLM is used to infer identities.

**Current status:** the whisper.cpp adapter currently advertises no lightweight
diarization capability, so the available fallback is an unlabeled transcript.
It does not invent speaker names. A future source adapter may preserve
reliable source-provided labels, and a future local diarization package must
meet the above performance/quality gate before it is enabled.

## Cloud fallback and privacy model

Cloud providers conform to the same `SpeechProvider` contract:

```text
LocalWhisperProvider
OpenAIProvider
OpenRouterProvider
OpenAICompatibleProvider
```

There is no silent fallback. Before any cloud job, the extension presents provider identity, privacy/retention link, data type (audio or selected ranges), destination/region when available, estimated cost when available, and the reason local processing was insufficient. The user must choose the provider for that job. API keys reside only in the OS secret store and are never synced through `chrome.storage` or added to logs. The default upload unit is the minimum normalized audio range necessary for the requested job; provider-specific whole-file upload is allowed only after the same disclosure.

Local mode keeps media, PCM, transcripts, timestamps, and models on the user's device. A small SQLite job store lives in an app-private directory; the application uses OS user permissions and, where supported, a locally protected cache key for sensitive retained data. Temporary media has the retention policy described above. Results persist until the user deletes them or their configured retention policy expires, because a completed transcript must be reopenable.

That persistence statement is a durable-engine target, not current Phase 8
behavior. The current extension can show or export only a completed result held
in its bounded service-worker-memory cache; a worker restart loses that cache.
The current in-process host's private development exports remain local to its
configured export root and their paths are not exposed to the extension.

**Current status:** `subtitler-asr` now contains provider-agnostic local/cloud
route and consent types, including OpenAI, OpenRouter, and OpenAI-compatible
configuration. That layer deliberately has no HTTP client, uploader,
extension/host setting, or automatic fallback; the current native request wire
remains local-only. A consent is tied to the job, selected provider/model,
exact internally validated endpoint, redacted source identity, and audio
scope. See [PHASE_7_LOCAL_FIRST_PROCESSING.md](PHASE_7_LOCAL_FIRST_PROCESSING.md)
for its precise present boundary.

## Security and privacy threat model

| Asset/threat | Primary mitigation |
| --- | --- |
| Recording, PCM, transcript leaks | local-first pipeline; no analytics payloads; redacted structured logs; private cache permissions; secure cleanup and retention controls |
| Signed URLs and API keys | URL expiry/origin/range validation; signed URLs redacted and never persistent; OS secret store for cloud keys; zeroization/redaction; browser cookies and authorization headers are never accepted by the engine |
| Malicious page sends a privileged fetch/job request | validate extension sender/tab/frame; content script messages carry a per-tab nonce; service worker validates media descriptor against a recent user gesture and adapter allowlist; no arbitrary URL proxy |
| Fake or malicious extension reaches engine | exact Chrome extension ID in native-host `allowed_origins`; host validates Chrome-supplied origin; engine accepts only authenticated host IPC |
| Malformed media, URLs, subtitle text | allowlisted schemes; redirect/DNS checks; fixed argument process execution; resource quotas/timeouts; parser fuzzing; overlay uses `textContent`, never remote HTML |
| Native host/engine compromise or update tampering | code-signed installers and binaries; checksum/signature-verified model artifacts; dependency lockfiles/SBOM; least-privilege user install; stdout reserved for Native Messaging frames |
| Disk exhaustion or fork bombs | job-level byte, duration, CPU/concurrency, cache, retry, and decoder time limits; one durable engine instance |
| Cloud disclosure failure | no automatic cloud path; explicit per-job consent and provider record; upload/cancellation audit with no content data |
| A hostile process running as the same OS user | user-only local endpoint ACL plus installation secret reduces accidental cross-process access, but cannot fully protect against same-user malware; document this operating-system boundary honestly |

## Browser and OS limitations that shape V1

- Manifest V3 service workers can suspend; neither popup nor worker is a durable job host. The native engine is therefore the source of truth.
- Native Messaging connects only from an extension page/service worker, not a content script; content scripts route through the service worker. Its host is stdio-only, not a public server. [Chrome Native Messaging](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)
- A service-worker Native Messaging connection has no useful native window parent on Windows. The current browser-export path therefore uses an explicit Chrome download/save flow backed by a temporary extension Blob, rather than assuming that the in-process development host can parent an OS save dialog. A future durable native export UI must retain the same explicit-user-action rule.
- Extension host access may be restricted by browser policy, incognito settings, site permission settings, or enterprise policy. The user must enable file URL access separately for `file:` media. [Chrome permissions](https://developer.chrome.com/docs/extensions/develop/concepts/declare-permissions)
- A page's video element can represent `blob:`/MSE media whose bytes are not safely addressable by the extension; EME content must be rejected. Cross-origin iframes also need their own allowed host access, and fullscreen overlays can require attachment inside the element entering fullscreen.
- Native GPU acceleration depends on the OS, driver, model build, memory, and packaging. Hardware detection advises rather than guarantees performance. CPU inference is the portable baseline.

## Proposed monorepo structure

```text
subtitler/
├── extension/
│   ├── src/
│   │   ├── background/       # MV3 service worker and native-port client
│   │   ├── content/          # common probe and platform adapters
│   │   ├── overlay/          # Shadow DOM subtitle renderer
│   │   ├── popup/            # two-action user interface
│   │   └── shared/           # schemas, types, error presentation
│   ├── public/
│   └── tests/
├── native/
│   ├── crates/
│   │   ├── subtitler-core/          # domain, scheduler, job state
│   │   ├── subtitler-engine/        # durable per-user engine binary
│   │   ├── subtitler-native-host/   # Chrome stdio bridge only
│   │   ├── subtitler-media/         # descriptors, acquisition, FFmpeg, cache
│   │   ├── subtitler-asr/           # provider traits, whisper.cpp FFI, VAD
│   │   ├── subtitler-subtitles/     # segmentation and all export formats
│   │   ├── subtitler-platforms/     # generic/YouTube/Webex/Zoom contracts
│   │   ├── subtitler-providers/     # optional cloud providers
│   │   └── subtitler-store/         # SQLite, files, retention, secret facade
│   ├── tests/
│   └── Cargo.toml
├── third_party/
│   ├── whisper.cpp/                 # pinned source/submodule and notices
│   └── ffmpeg/                      # packaging metadata/notices, not mutable cache
├── docs/
│   ├── ARCHITECTURE.md
│   ├── NATIVE_MESSAGING_PROTOCOL.md
│   ├── adr/
│   └── benchmarks/
├── scripts/                         # reproducible build/package/model tooling
├── tests/
│   ├── fixtures/                    # licensed short media only
│   ├── integration/
│   └── benchmarks/
├── Cargo.lock
├── package-lock.json
└── README.md
```

## Phased V1 roadmap and exit gates

| Phase | Deliverable | Exit criteria |
| --- | --- | --- |
| 1. Architecture | this decision record, protocol contract, ADRs, threat model, fixture/licensing plan | decisions reviewed; no unsupported-access promise hidden in scope |
| 2. Skeleton + IPC | MV3 extension build, Rust workspace, registered native host, durable engine handshake | extension and workspace build; contract tests prove reconnect and protocol-size rejection |
| 3. Generic transcription | local file/direct accessible HTML5 source -> FFmpeg -> local ASR -> timed transcript -> TXT/SRT/VTT/JSON | automated export/timing tests plus at least one real accessible MP4 fixture |
| 4. Overlay | cue segmenter, Shadow DOM overlay, playback/rate/pause/seek synchronization, bounded completed-cue delivery | unit coverage proves cue-page mapping/deduplication and timeline synchronization; a Chromium fullscreen/playback integration suite remains required before release |
| 5. Ahead-of-playhead | bounded 30-second local ranges, adaptive scheduler, seek preemption, partial cue pages, and pacing status | deterministic scheduler/host/extension tests cover lead calculations, priority inversion prevention, cancellation, and paging; real-media replay, browser integration, durable caching, and recovery UI actions remain required |
| 6. Platform adapters | strict YouTube existing-caption overlay plus Webex/Zoom recording-route recognition and generic direct HTML5 routing | deterministic route/parser/privacy tests pass; real authorized/non-DRM browser examples, platform caption/download/media paths, and full platform adapters remain required |
| 7. Enhancement paths | optional diarization and explicit cloud providers | no-cloud-by-default tests, provider-consent tests, speaker fallback tests |
| 8. Result delivery, harden/package | Current slice: completion-only transcript pages, transient popup viewing, final cue-page drain, and explicit fixed-name browser exports. Remaining release scope: benchmark suite, fault injection, signed installers, model management, and privacy review. | Deterministic paging/cache/export tests pass; a real Chrome download/save flow, licensed real-media corpus, performance matrix, cache cleanup/recovery test, security review, and reproducible packaged smoke test remain required. |

Each phase ends with a clean compile, relevant unit/integration tests, error fixes before advancing, a short implementation note, and the next highest-value item. The benchmark suite records word error rate against licensed references, real-time factor, CPU/GPU/memory use, startup time, cue timing error, and subtitle-buffer depth on low-end CPU, typical laptop, Apple Silicon, and CUDA-capable hardware where available.

## Major risks and mitigations

| Risk | Mitigation and decision |
| --- | --- |
| Authorized browser playback is not directly retrievable | caption-first and direct/download strategies; use the authorized page only to resolve a non-secret direct representation or platform download; clear unsupported state rather than circumvention |
| Local ASR cannot stay ahead | early real-time-factor estimate, adaptive lead, scheduler preemption, explicit pause/faster-model/cloud choices; never silently accumulate delay |
| Whisper accuracy and word timing vary by audio | benchmark models by domain; retain word confidence; use overlaps and conservative cue segmentation; expose a regenerate path |
| FFmpeg/ASR package size and licensing | optional model downloads, backend-specific packages, pinned notices/SBOM, evaluate codec/model license before distribution |
| MV3 lifecycle drops UI connection | durable engine and persisted job state; reconnect/resubscribe protocol; no job ownership in popup/service worker |
| Platform pages change frequently | small adapter contract, fixture-based contract tests, capability discovery, feature flags, graceful fallback to generic/unsupported |
| Signed media URL leaks through diagnostics or retention | no browser-cookie/header transfer; expiry/origin/range validation; strict redaction; no URL in persistent job identity, UI events, or generic proxy |
| Transcript download leaks content or retains it indefinitely | completion-only bounded result pages; transient worker cache; fixed filenames; explicit user save action; one temporary offscreen Blob revoked on terminal download or TTL; no transcript/Blob URL in extension storage |
| Diarization adds disproportionate latency | source metadata first; optional low-priority local ONNX path; unlabeled transcript is always valid |
| Subtitle overlay clashes with page/fullscreen | isolated Shadow DOM, player-container attachment, conservative pointer events, documented iframe/fullscreen fallback |

## Architecture sources

- [Chrome Native Messaging](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging) -- framing, size limits, allowed origins, Windows registration, and service-worker/content-script boundary.
- [Chrome extension permissions](https://developer.chrome.com/docs/extensions/develop/concepts/declare-permissions) -- optional host permissions and file/incognito access behavior.
- [Chrome extension cross-origin requests](https://developer.chrome.com/docs/extensions/develop/concepts/network-requests) -- difference between extension and content-script request privileges.
- [Chrome privacy guidance](https://developer.chrome.com/docs/extensions/develop/security-privacy/user-privacy) -- least privilege and `activeTab` guidance.
- [Chrome downloads API](https://developer.chrome.com/docs/extensions/reference/api/downloads) -- explicit browser download flow and terminal download state.
- [Chrome offscreen API](https://developer.chrome.com/docs/extensions/reference/api/offscreen) -- MV3 offscreen-document lifecycle and `BLOBS` reason.
