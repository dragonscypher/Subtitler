# Testing

**Status:** Phase 8 adds a bounded, opt-in real local-pipeline smoke helper;
it invokes developer-supplied local FFmpeg, whisper.cpp, model, and licensed
media only. It is a functional sanity check and one local wall-clock
measurement, not a production benchmark or proof of ahead-of-playback
performance. Phase 8 also adds deterministic transcript-result tests:
completion-only native transcript paging, final cue-page drain, transient
service-worker cache limits/discard behavior, lazy popup pages, browser-export
formatting, and the offscreen Blob/download lifecycle. Those tests do **not**
launch Chrome, open a real save dialog, or download a real file. Phase 5
provides bounded ahead-of-playhead buffering with deterministic coverage. The
suite exercises the FFmpeg and whisper.cpp process boundaries through injected
runners, private-artifact cleanup, native job transitions, safe bounded audio
ranges, scheduler pacing/seek preemption, metadata-only playback forwarding,
progressive generated-cue paging, TypeScript/Rust cue-schema mapping, overlay
deduplication, timestamp parsing, and export generation. Phase 6 additionally
has deterministic tests for strict
YouTube-caption parsing/selection, safe bridge metadata, and Webex/Zoom route
recognition/no-bypass guidance. It does **not** yet prove a bundled
FFmpeg/model against a licensed real-media corpus, a Chromium browser overlay,
actual ahead-of-playback speed, durable job recovery, a real authorized
YouTube caption fetch, or a full platform adapter. Phase 7 additionally has
deterministic tests for conservative local-model feasibility/planning,
host-side hardware-observation normalization, safe advisory propagation, and
cloud route/consent validation. It does not exercise a real GPU, installed
model manager, cloud upload, provider credential store, or diarization model.

## Current CI contract

[.github/workflows/ci.yml](../.github/workflows/ci.yml) runs two independent `windows-latest` jobs for every push, pull request, and manual dispatch:

| Job | Commands | What a passing job establishes |
| --- | --- | --- |
| Extension | `npm ci`, `npm run typecheck`, `npm test`, `npm run build` | The locked JavaScript dependencies install, TypeScript type-checks, Vitest unit tests pass, and esbuild produces the unpacked extension output. |
| Native workspace | `..\scripts\test-real-local-pipeline-contract.ps1`, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --all-targets --locked`, `cargo build --workspace --locked` | The helper contract validates local-only/no-execution behavior without media, then Rust source is formatted, has no accepted Clippy warnings, passes its unit tests, and builds from the committed lockfile. |

The workflow deliberately uses a lockfile-respecting install/build path. It does not cache `node_modules`, run release packaging, download ASR models, decode real media, or contact media platforms. The real-pipeline contract validation creates only empty local temporary files, runs the helper's `-ValidateOnly` mode, and confirms that `SUBTITLER_*` environment variables are restored.

## Run the current checks locally

The foundation needs Node.js 20 or later, Rust stable with the `rustfmt` and `clippy` components, and on Windows the usual MSVC Rust build prerequisites. Current unit and build checks do **not** require FFmpeg, a whisper.cpp binary/model, Chrome, native-host registration, or a cloud-provider key.

From the repository root, run the same checks as CI:

```powershell
npm --prefix extension ci
npm --prefix extension run typecheck
npm --prefix extension test
npm --prefix extension run build

.\scripts\test-real-local-pipeline-contract.ps1
cargo fmt --manifest-path native/Cargo.toml --all -- --check
cargo clippy --manifest-path native/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path native/Cargo.toml --workspace --all-targets --locked
cargo build --manifest-path native/Cargo.toml --workspace --locked
```

`npm ci` replaces the local dependency tree with the exact `package-lock.json` resolution. Use it when validating a clean installation; use `npm --prefix extension run test -- <pattern>` for a focused Vitest run after dependencies are already present.

For the narrow Windows developer native-host registration logic, run
`.\scripts\test-native-host-registration.ps1`. It validates only GUID-named
developer fixtures and never writes the registry. Run
`.\scripts\verify.ps1 -SkipExtension` to include that validation, the no-media
real-local helper contract check, and the native-host framing smoke test.
Actual registration and a manual Chrome connection remain opt-in developer
steps described in
[INSTALLATION.md](INSTALLATION.md#windows-developer-native-host-registration).

## What is covered today

The current suite is intentionally unit- and smoke-oriented.

| Area | Current coverage | Explicitly not covered yet |
| --- | --- | --- |
| Extension media handling | Candidate ranking, protected-media recognition, direct-URL/protocol input validation, a metadata-only playback observer (initial/event-driven snapshots plus a playing interval), and safe platform-route classification. | Browser automation, real page/player discovery, cross-origin media acquisition, authenticated sessions, and player-specific integration. |
| Platform adapters | Strict YouTube video-route recognition, fixed-path existing-caption parsing/selection, timed-text endpoint sanitization, text normalization, safe provider-only detection metadata, and Webex/Zoom recording-route/no-bypass guidance. | Chrome MAIN-world execution, a real authorized caption fetch, YouTube direct media extraction, Webex/Zoom captions/downloads/media resolution, foreign-language caption translation, and any credential transfer. |
| Subtitle overlay logic | Cue normalization, active-cue selection, overlap handling, clearing stale cues after a seek, page-wise progressive-cue deduplication/sorting, and a 100,000-cue in-page bound. | Chrome rendering, fullscreen behavior, playback-rate behavior, and page/player-specific integration. |
| Extension/native boundary | TypeScript native request/response validation, status polling lifecycle, newest-only bounded `playback_update` forwarding, processing-time and completed `get_subtitle_cues` paging, completion-only `get_transcript_segments` paging, Rust native-message framing, dispatcher lifecycle, and cancellation behavior. | A Chrome process connected to a registered native host, reconnect recovery across service-worker suspension, and durable-engine IPC. |
| Completed transcript view/export | Native chronological page ordering and privacy fields, 100-segment/120 KiB page limits, final cue-page draining, bounded transient result caching, lazy popup pages, five-format browser rendering, fixed-name download validation, one-active-export behavior, offscreen message/origin checks, and terminal/TTL Blob cleanup. | Real Chrome offscreen/document behavior, an actual save dialog/download, a browser restart with an active export, durable result reopening, and export interoperability against licensed real ASR output. |
| Native domain/job policy | Timestamp/domain validation, job state transitions, HTTPS/local-media policy, DRM/browser-scheme/private-IP-literal rejection, DNS-result/private-address validation, UNC/device-path rejection, and URL-redaction behavior. | Real downloads against an external fixture, persisted jobs, retries, configurable durable storage quotas, and provider-session acquisition. |
| Local model and cloud planning | Deterministic CPU/memory/backend feasibility, quality-first model/quantization selection, conservative host observation conversion, advanced-override validation, redacted optional handshake advisory parsing, provider endpoint narrowing, opaque credential formatting, and per-job consent/route mismatch rejection. | A real GPU/VRAM detector, model-manager asset verification, benchmark-derived thresholds, OS credential storage, any cloud HTTP/upload flow, provider integration, or diarization inference. |
| Subtitle scheduler | Default-target validation, non-overlapping leases, contiguous completed-lead calculations, adaptive real-time-factor pacing, stale-update handling, seek preemption, valid late completion, and full-transcript independence. | Measured performance on real hardware, durable cache/index behavior, multi-worker scheduling, and recovery UI actions. |
| Audio/ASR boundary | FFmpeg and whisper.cpp argument-vector plans, file-only decoder protocol policy, controlled remote-download policy with fake DNS/transport, bounded range extraction, cancellation/timeout paths, bounded diagnostics, private source/WAV/JSON cleanup, canonical WAV validation, configuration validation, and timestamped JSON parsing through injected runners. | Invoking a released FFmpeg/whisper.cpp package, a real controlled HTTPS download, decoding real audio, model download/selection UX, word-error rate, and translation quality. |
| Subtitle/export library | Deterministic cue segmentation, SRT/VTT/TXT/JSON formatting, and atomic five-file export bundles with partial-output cleanup. | Interoperability against independent subtitle parsers and exports from a licensed real transcription fixture. |

A green workflow establishes deterministic safety and integration behavior at these boundaries. It must not be described as measured proof that a released FFmpeg/model package can transcribe real recordings accurately or stay ahead of playback.

## Current acceptance criteria

For the current implementation, acceptance is mechanical and enforced in CI:

- Every command in the two CI jobs exits successfully on Windows.
- `cargo fmt --check` produces no formatting diff.
- Clippy runs with `-D warnings`; new accepted lint warnings fail CI.
- Node and Cargo dependency resolution use their committed lockfiles.
- Extension bundles and every workspace target build without requiring a locally installed media/ASR runtime.
- Phase 5 policy tests establish only deterministic range selection, cancellation,
  pacing-state, and paging behavior. They do not establish that a machine can
  process a particular recording ahead of playback.
- Phase 6 tests establish input narrowing and page-local caption handling,
  not that a third-party platform will expose a usable recording or caption
  track in a real authenticated browser session.
- Phase 7 tests establish deterministic routing and disclosure invariants, not
  that a selected local model exists, can use an accelerator, or that a cloud
  provider can receive media. Cloud upload is intentionally absent today.
- Phase 8 tests establish bounded, completion-only result delivery and the
  explicit browser-export contract. They do not establish that Chrome created a
  file, displayed a save dialog, retained a completed transcript across a
  service-worker restart, or decoded a real media fixture.

## Opt-in real local-pipeline check

`scripts/test-real-local-pipeline.ps1` exercises the compiled native host with
an explicitly supplied local FFmpeg executable, whisper.cpp CLI, model file,
and local media fixture. It sends a `local_file` job through real Native
Messaging framing, waits for completion, verifies all five exports, validates
the exported timestamped transcript JSON without printing its contents, and
emits counts plus a single end-to-end real-time-factor calculation using the
developer-supplied fixture duration. It removes its own randomly named
temporary cache/export directory unless `-KeepArtifacts` is supplied.

It is intentionally absent from normal CI: it must be run only with a
redistributable fixture and locally installed assets whose license, version,
and checksum have been reviewed. It rejects relative, UNC, device, alternate
data-stream, reparse-point, and non-local-drive inputs before it starts the
host. It sends no URL, cookies, browser headers, credentials, or media bytes
to a network endpoint. It does not access, test, or bypass DRM/authentication;
the supplied media must already be an ordinary local file that the developer is
authorized to use.

```powershell
cargo build --manifest-path native/Cargo.toml --release -p subtitler-native-host
.\scripts\test-real-local-pipeline.ps1 `
  -FfmpegPath C:\tools\ffmpeg.exe `
  -WhisperCliPath C:\tools\whisper-cli.exe `
  -ModelPath C:\models\ggml-small.bin `
  -MediaPath C:\fixtures\licensed-speech.wav `
  -MediaDurationMs 60000 `
  -RequireSpeech `
  -Model small -Quantization f16
```

To exercise the generated-cue pages as well, use the same licensed speech
fixture with `-JobKind subtitle -VerifySubtitleCues`. The helper verifies that
every returned page belongs to the completed job and that each cue has positive
timing and at least one subtitle line; it still removes its temporary directory
unless `-KeepArtifacts` is supplied. The duration must match the complete
fixture for a subtitle job. Since no browser clock exists in this local-only
run, the helper supplies a synthetic end-of-media playhead to let the scheduler
finish the recording. That is intentionally not a controlled playback replay.

The default whole-run deadline is 30 minutes, configurable with
`-TimeoutSeconds` from 15 seconds through four hours; each Native Messaging
reply has a separate 30-second default, configurable and capped with
`-NativeMessageTimeoutSeconds`. A timeout stops the helper-host process and
cleans its owned temporary directory. `-KeepArtifacts` is an explicit local
debugging opt-in and prints the temporary artifact directory only to the
invoking developer. The helper snapshots and restores every process-scoped
`SUBTITLER_*` environment variable in `finally`, including when it is invoked
from a long-lived or dot-sourced PowerShell session.

For a no-media/no-engine validation of the harness itself, run:

```powershell
.\scripts\test-real-local-pipeline-contract.ps1
```

That contract check is in Windows CI. It uses empty temporary files with
`-ValidateOnly`, asserts that UNC media is rejected, confirms that no
`SUBTITLER_*` environment value leaks, and statically blocks common PowerShell
download commands from the helper. It is not an ASR test.

The returned real-time factor is `wall elapsed time / declared media duration`.
It includes local process launch, decode, ASR, segmentation, and export for one
run. It does not establish accuracy, model quality, a hardware profile,
repeatability, or that subtitles can stay ahead of a real player. Do not use it
as a release benchmark result.

## Phase 5 deterministic coverage

The normal workspace and extension suites now include the Phase 5 unit and
host-integration coverage; no ignored target or real media asset is required.
They assert:

- 30-second minimum / 120-second preferred / 300-second maximum buffer policy,
  30-second source leases, and five-second pre-playhead context.
- Contiguous *completed* buffer calculations, adaptive real-time-factor
  pacing, stale seek suppression, and a new seek's priority over obsolete
  outstanding work.
- Bounded FFmpeg seek/duration plans, range validation, cancellation, cleanup,
  and source-timeline timestamp offsetting.
- A `playback_update` rate/position/seek-generation boundary, active-range
  cancellation, full-transcript independence, and partial cue-page limits.
- Extension observation/forwarding lossiness and the distinction between an
  exhausted in-progress cue page and a terminal completed result.

The suite is deliberately hermetic. It does not run FFmpeg or whisper.cpp
against real media, measure CPU/GPU/RAM, or use a live Chrome tab.

## Next real-media coverage

The manual helper above is available now, but a pinned redistributable corpus,
independent parser validation, controlled playback replay, and scheduled
benchmark reporting remain future work. The following corpus-driven commands
are intentionally **not available today**; add the matching test target/script
and fixture manifest in the same change that implements the feature. They
belong in opt-in or scheduled jobs, not ordinary PR CI, because they use real
media bytes and model runtimes.

```powershell
# Future corpus-driven local media regression targets.
$env:SUBTITLER_REAL_MEDIA = "1"
cargo test --manifest-path native/Cargo.toml -p subtitler-media --test real_media -- --ignored
cargo test --manifest-path native/Cargo.toml -p subtitler-asr --test fixture_transcription -- --ignored
cargo test --manifest-path native/Cargo.toml -p subtitler-subtitles --test end_to_end_exports -- --ignored

# Planned Phase 4: packaged-extension browser integration.
npm --prefix extension run test:browser

# Planned Phase 5: licensed real-media replay with a controlled playhead.
cargo test --manifest-path native/Cargo.toml -p subtitler-native-host --test ahead_of_playback_replay -- --ignored
```

Phase 3 fixtures must be actual, redistributable audio/video files with a checked-in manifest containing license/source, SHA-256, duration, codec/container metadata, a human reference transcript, and where practical reference word/cue timings. Do not commit customer meetings, signed URLs, authentication material, or unlicensed platform recordings.

The Phase 3 exit gate should prove all of the following:

- Supported local MP4/AAC, WebM/Opus, and audio-only fixtures decode into canonical mono 16 kHz PCM without decoding video solely for transcription.
- VAD/chunk boundaries, ASR words, transcript segments, and subtitle cues have finite, monotonic, non-overlapping time ranges; malformed/corrupt fixtures fail with an actionable error and cleanup.
- A real, pinned local ASR runtime creates timestamped output; the unavailable stub may no longer be used for a successful job.
- Generated SRT and VTT parse in independent parsers, and every exported cue has a positive duration, valid ordering, and no more than the configured line limit.
- Temporary test data is removed on success, cancellation, and simulated decoder/engine failure unless explicit retention is enabled.

Phase 4 must add a Chromium/extension suite for playback, pause, seek, fullscreen where available, and existing-caption behavior against a local HTML5 fixture page. The overlay must show the cue valid at the current media time and clear or replace it after a seek; it may not keep a stale cue while waiting for unrelated prior work.

Phase 5 deterministic tests now exercise the interval scheduler with fake
providers. Before release, the real-media replay must add a controlled media
clock and prove the same properties against a licensed fixture: a seek to an
uncovered later timestamp reprioritizes work around that playhead, a
full-transcript job continues independently, and processing-time measurements
are carried through the UI/job state. It must also verify the recovery path:
when processing cannot keep up, the user receives an actionable pause/model or
explicit-cloud choice rather than silently accumulating delay.

## Platform and cloud validation after the core pipeline

Phase 6 now has deterministic extension tests in
`youtube-captions.test.ts`, `youtube-caption-bridge.test.ts`, and
`recording-platforms.test.ts`. They validate exact HTTPS route/domain
boundaries, fixed-path caption parsing, timed-text endpoint filtering,
privacy-safe descriptors, caption normalization, and no-bypass guidance. They
do not execute `chrome.scripting.executeScript` in a real MAIN world, fetch a
real caption endpoint, or use a browser login.

YouTube, Webex, and Zoom still require their own authorized fixture or
test-account plan before the platform layer can be called release-ready.
Routine CI must use only public/authorized test assets and may validate the
current YouTube caption overlay, discovery, and lawful platform-download
handoffs when those features exist. It must never scrape cookies, copy
authorization headers, decipher protected URLs, or treat a browser
`blob:`/MSE/DRM source as a native-media URL. The current code has no YouTube
direct-media path, no Webex/Zoom acquisition path, and no
foreign-language-to-English caption translation.

Optional cloud-provider tests arrive in Phase 7 and must be separately gated by an explicit test secret and opt-in environment flag. They may upload only synthetic or explicitly licensed fixtures. The default test path must continue to prove that cloud processing is never selected silently.

## Benchmark relationship

Functional coverage tells us whether expected behavior holds. It does not establish speech quality or hardware performance. The planned corpus, measurement definitions, hardware matrix, and release targets are in [BENCHMARKS.md](BENCHMARKS.md). No accuracy, timing, memory, or ahead-of-playback performance claim should be made until that harness has produced a versioned result.
