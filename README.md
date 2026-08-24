# Subtitler

Subtitler is a local-first browser companion for prerecorded media. It creates
full transcripts without requiring playback and renders timestamped subtitles
ahead of a video playhead when the underlying media is accessible without
bypassing DRM, encryption, authentication, or platform protections.

## Current implementation

This repository contains the Phase 1 design, Phase 2 extension/native-host
foundation, a configuration-gated Phase 3 generic-transcription path, the
Phase 4 generated-overlay handoff, and a bounded Phase 5
ahead-of-playhead implementation, a deliberately narrow Phase 6
platform-adapter layer, Phase 7 local-first planning/consent contracts, and a
Phase 8 completed-transcript delivery/export slice:

- Chrome Manifest V3 extension with media/caption detection, a minimal popup,
  a synchronized overlay, and a typed native-messaging client.
- Rust workspace for jobs, HTTPS/local-source validation, a controlled
  DNS-pinned direct-media downloader, FFmpeg audio-only normalization,
  whisper.cpp CLI integration, subtitle segmentation/export, and the framed
  native-messaging host.
- A conservative local-model planner that uses logical CPU count, available
  memory, and explicitly declared compiled backends to make a local-only plan.
  Its non-sensitive advisory reaches the popup only while the native host is
  connected; it never claims that an uninstalled model is ready or silently
  starts cloud processing.
- Provider-agnostic OpenAI, OpenRouter, and OpenAI-compatible routing and
  consent contracts. They currently perform no HTTP upload and have no normal
  UI setting; any future cloud route requires a fresh per-job disclosure and
  matching explicit consent.
- A native job worker that creates private temporary WAV audio, produces timed
  transcripts when local FFmpeg/model assets are configured, and atomically
  writes TXT, timestamped TXT, SRT, VTT, and JSON exports.
- For a completed full-transcript job, the host exposes chronological,
  completion-only transcript pages with a deliberately small display DTO
  (timing, text, and an optional speaker label). The extension drains the final
  cue pages as well, then exposes **View Transcript** through a bounded,
  lazy-paged popup reader. Transcript text and cues live only in a bounded
  service-worker-memory cache: they are never put in `chrome.storage`, and an
  incomplete result is discarded after stop or native-host disconnect.
- From that completed view, an explicit user selection can create one browser
  save dialog for TXT, timestamped TXT, SRT, VTT, or JSON. The browser-export
  path uses fixed product filenames and a temporary offscreen Blob only; it
  does not receive a local media path, native-host access, or persistent
  storage. A rendered browser export over 16 MiB fails with a clear error
  rather than being truncated. `Transcript.json` is intentionally a
  display-safe segment JSON document, not the native engine's full canonical
  word-level artifact.
- A direct remote source is resolved and checked on every redirect hop, then
  staged in a capped job-private file before decoding. FFmpeg accepts only
  local paths and has its protocol whitelist restricted to `file`; browser
  cookies and authorization headers are never transferred to this downloader.
- A local HTML5 media file already open in Chrome can use the native
  `local_file` route. The extension accepts it only from a local, non-UNC
  `file:` tab; file paths remain transient and are not stored in extension job
  state. Pasted file paths and network/device paths are rejected.
- A generated-subtitle job with a positive media-duration hint has a local,
  deterministic scheduler. It processes 30-second source-audio windows around
  the playhead, reports only contiguous completed subtitle lead, maintains a
  30-second minimum / 120-second preferred / 300-second maximum lead policy,
  and recommends a pause before a slow local job silently falls behind.
- The extension forwards only lossy playhead metadata (position, rate, paused
  state, and seek generation). It retains one newest unsent update, so timeline
  reporting never becomes a media or transcript channel.
- For scheduled subtitle jobs, generated cues are available in bounded pages
  while processing as well as after completion. The extension validates, maps,
  deduplicates, and sorts them in the page-local overlay; it does not put
  transcript content or export paths in `chrome.storage`.
- **Create Subtitles** and **Generate with Subtitler** always choose the local
  audio → FFmpeg → ASR path. Existing captions are available only through the
  explicit **Use Existing Captions** fast path.
- A recognized YouTube page uses the bundled `yt-dlp` adapter to acquire a
  private local audio artifact; it never routes generated subtitles through
  timed-text captions or transfers browser cookies. The adapter supplies its
  private Deno/EJS challenge runtime when installed. Some YouTube videos now
  require a short-lived proof-of-origin token for media bytes; Subtitler fails
  clearly rather than inventing one, copying browser credentials, or falling
  back to captions.
- The Webex adapter uses a bounded same-origin request in the authorized page
  only to discover its normal signed recording representation. It retains no
  cookie/header/body and passes only the temporary media address to the
  controlled downloader. Zoom remains recognition plus a generic direct-HTML5
  route until its equivalent safe adapter is implemented.
- Tests cover deterministic scheduler priorities and pacing, safe bounded
  FFmpeg range plans, seek cancellation, progressive cue paging, extension
  playback observation, strict YouTube caption parsing, Webex/Zoom route
  recognition, native protocol/job transitions, artifact cleanup, ASR
  timestamp parsing, completed-transcript paging/cache behavior, and native as
  well as browser-export formatting/lifecycle behavior. They do not yet launch
  Chrome to verify a real download/save dialog or run a manually supplied media
  fixture by default.

The architecture and phased delivery plan are in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The first supported acquisition
path is an accessible direct HTTPS generic HTML5 media source or a safe local
HTML5 file page. See [Phase 5: Adaptive Ahead-of-Playhead Subtitle
Buffering](docs/PHASE_5_AHEAD_OF_PLAYHEAD.md) for the delivered behavior and
its deliberate limits, and [Phase 6: Platform
Adapters](docs/PHASE_6_PLATFORM_ADAPTERS.md) for the current platform
boundaries. Packaged model management, a durable native
engine, full platform acquisition adapters, and measured real-media
performance remain later milestones. See [Phase 7: Local-first Processing
Plans](docs/PHASE_7_LOCAL_FIRST_PROCESSING.md) for the hardware and cloud
contract boundary. The exact current transcript-page contract is in
[Native Messaging Protocol](docs/NATIVE_MESSAGING_PROTOCOL.md); installation
and developer-host registration guidance is in
[Installation](docs/INSTALLATION.md).

## Build and test

Prerequisites:

- Node.js 20 or later
- Rust stable with the MSVC build tools on Windows
- FFmpeg plus a compatible whisper.cpp CLI and model are needed for actual
  transcription (the deterministic unit suite does not require them)

```powershell
npm --prefix extension install
npm --prefix extension run build
npm --prefix extension test

cargo test --manifest-path native/Cargo.toml --workspace
cargo build --manifest-path native/Cargo.toml --release -p subtitler-native-host
```

Load `extension/dist` as an unpacked extension after building. Native-host
registration is deliberately handled by the installer scripts rather than by
the extension; see `docs/INSTALLATION.md`. Development execution is explicitly
opt-in through local engine configuration; the project does not yet ship
bundled binaries, model assets, or a signed installer. The developer
registration scripts create only an exact, per-user development host mapping;
they are not an end-user installer.

For an explicitly local, developer-supplied media smoke run, see the bounded
`local file -> FFmpeg -> whisper.cpp -> timestamped exports` helper in
[docs/TESTING.md](docs/TESTING.md#opt-in-real-local-pipeline-check). It neither
contacts a media platform nor proves benchmark-grade accuracy or
ahead-of-playback performance. It is opt-in and requires developer-supplied,
authorized local media plus local FFmpeg, whisper.cpp, and model assets.

## Privacy and security

Subtitler defaults to local processing. It does not log audio, transcripts,
tokens, or private URLs; it does not extract browser cookies; and it never
attempts to defeat media protections. The current cloud layer is a
non-uploading, explicit-consent contract only; a future cloud transcription
implementation remains a per-job opt-in through that provider abstraction.

## Project layout

```text
extension/    Chrome MV3 UI, media integration, overlay, native client
native/       Rust job/media/ASR/subtitle/native-host workspace
docs/         Architecture, security model, installation, development plan
scripts/      Packaging and development helpers
tests/        Cross-component fixtures and integration plans
third_party/  Vendored or pinned third-party source metadata (when introduced)
```
