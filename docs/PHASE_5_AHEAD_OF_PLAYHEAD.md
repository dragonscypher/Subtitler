# Phase 5: Adaptive Ahead-of-Playhead Subtitle Buffering

**Status:** implemented with deterministic native and extension coverage. This
phase adds a bounded local scheduler and progressive cue delivery. It is not a
benchmark result or a claim that every recording, browser player, or hardware
profile can stay ahead of playback.

## Delivered flow

```text
HTML media timeline
  -> content observer (metadata only)
  -> MV3 service worker / Native Messaging
  -> in-process subtitle scheduler
  -> controlled remote staging when the source is HTTPS
  -> bounded FFmpeg audio range
  -> local whisper.cpp transcription
  -> timestamp offset + subtitle segmentation
  -> append-only bounded cue pages
  -> page-local synchronized overlay
```

No browser audio is captured. The worker asks FFmpeg for audio from the
accessible prerecorded source and processes the selected range as fast as the
local decoder and ASR engine allow.

## Scheduler policy

The Phase 5 scheduler starts only for a `subtitle_generation` job that has a
positive media-duration hint. Its defaults are policy constants, not a
hardware-performance promise:

| Policy | Current default | Behavior |
| --- | ---: | --- |
| Minimum subtitle lead | 30 seconds | A smaller contiguous completed lead is under-buffered. |
| Preferred subtitle lead | 120 seconds | Normal target. `preferred_subtitle_buffer_ms` may adjust this value, clamped to the configured minimum/maximum. |
| Maximum useful lead | 300 seconds | Target while paused and used as extra cushion for very fast or slower-than-playback measured processing. |
| Source-audio chunk | 30 seconds | One FFmpeg/ASR scheduling lease. |
| Context before playhead | 5 seconds | Include the beginning of a cue that started just before a new playhead. |
| Media-layer hard range limit | 15 minutes | Rejects an unexpectedly broad single FFmpeg range even if a caller bypasses the 30-second scheduler policy. |

For the current playhead `p`, the scheduler chooses the first unprocessed,
non-leased interval in the window beginning at `max(0, p - 5 seconds)` and
ending at the adaptive target. It works in 30-second leases and records only
completed intervals. The reported `subtitle_buffer_ahead_ms` is therefore the
contiguous completed coverage after `p`—not queued or currently decoding work.

After each completed range, the host records:

```text
processing RTF = elapsed wall-clock milliseconds / source-audio milliseconds
effective RTF  = processing RTF × current playback rate
```

Below 1.0 effective RTF is faster than the user is consuming media. The
scheduler reports a measuring state until it has a sample, then keeping-up,
at-risk, cannot-keep-up, or pause-recommended status. When playback is paused,
or when the observed rate warrants more cushion, it targets the maximum lead.
The status uses safe UI text; it never contains a source URL, transcript body,
cookie, header, or command line.

When the current target is covered, the worker waits for a newer playback
observation instead of decoding arbitrary media far behind or ahead of the
viewer. A subtitle job consequently remains active until it has coverage for
the complete recording. If playback reaches the end while earlier sparse gaps
remain after seeks, it switches to filling those gaps so the requested export
bundle can finish instead of parking indefinitely. This is intentional in the
current bounded design.

## Playback updates and seeking

The content script sends an initial timeline snapshot, immediate updates for
play/pause, seeking/seeked, rate, metadata, and end events, plus a two-second
interval while the media plays. The extension retains only the newest pending
snapshot and forwards it to the host no more often than every 750 ms:

```json
{
  "command": "playback_update",
  "job_id": "<native-job-id>",
  "position_ms": 510000,
  "playback_rate_milli": 1000,
  "is_paused": false,
  "seek_generation": 4
}
```

Only timeline metadata crosses this boundary. The browser rate is clamped and
validated between 0.25x and 4.0x. A new seek generation preempts outstanding
subtitle leases. If the active FFmpeg or ASR task belongs to such a lease, the
host requests cooperative cancellation and schedules around the new playhead
at the next safe opportunity. A valid range that finishes after preemption can
still be recorded as useful completed coverage. A stale, lower seek generation
is safely ignored. The host also treats the first real page snapshot at a
nonzero position as preemptive: if the worker provisionally leased 00:00 before
the observer reported that the user is at 48:00, it cancels that lease and
starts around the actual playhead.

## Bounded local range processing

The media layer validates each half-open `[start_ms, end_ms)` range and uses
fixed FFmpeg arguments for a seek and duration. It outputs a private,
canonical mono 16 kHz WAV artifact. For a direct HTTPS source, the host first
uses a controlled, DNS-pinned downloader with manual redirect validation to
stage the recording into a capped job-private file. FFmpeg is then limited to
that local file (`-protocol_whitelist file`) and cannot independently resolve
or fetch a network URL. The existing cancellation, timeout, output-size
validation, bounded diagnostics, and automatic temporary-artifact cleanup
paths apply to range extraction as well as whole-media extraction.

whisper.cpp returns timestamps relative to the extracted chunk. The host
offsets them back to the source-media timeline, bounds them to the scheduled
range, runs the existing subtitle segmenter, and records the range as complete
only after that work succeeds.

This is time-bounded local FFmpeg extraction, not a claim that every remote
representation supports efficient HTTP byte ranges. The current controlled
path downloads the complete direct source before it schedules the first
subtitle range, so a large remote file can delay initial captions. HLS/DASH
manifests fail closed rather than allowing FFmpeg to fetch nested URLs.
Capability discovery, representation-specific range policy, and cache reuse
remain future work.

## Progressive subtitle pages

`get_subtitle_cues` accepts a cursor and a limit. The host may return finalized
cues during `processing` as well as after `completed`:

- A response has no more than 200 cues and stays within a 128 KiB serialized
  response budget.
- While processing, the host's cue list is append-only so native cursors stay
  stable. A page may be empty when the current range has not finalized cues
  yet; this is not a terminal signal.
- The extension leaves an exhausted processing cursor parked until its next
  status poll, then requests newer finalized pages. At completion it drains
  the remaining pages before marking the client job complete.
- A post-seek append can be out of chronological media order. The overlay
  validates, deduplicates, and sorts cues by time before rendering.
- Transcript-derived cue text stays page-local. It is never written to
  `chrome.storage`, and native export paths never cross the browser boundary.

## Full transcript independence

A `full_transcript` job keeps the existing complete-media workflow and does
not instantiate the subtitle scheduler. Playback updates do not reprioritize,
pause, or otherwise affect it. This preserves the product rule that a user
never has to play an entire recording to receive its full transcript.

## Deterministic verification coverage

The current automated suite covers the following behavior without needing an
installed model, a real recording, Chrome, or a registered native host:

- Core scheduler defaults and validation, contiguous coverage merging,
  non-overlapping leases, playback-rate/RTF pacing, stale updates, seek
  preemption, initial nonzero-playhead preemption, end-of-media gap completion,
  valid late completion, and full-transcript independence.
- Media range validation, FFmpeg seek/duration command plans, cancellation and
  timeout handling, output validation, and temporary-WAV cleanup.
- Native-host dispatch of `playback_update`, rate validation, seek cancellation
  of an active chunk, timestamp offset/bounding, safe partial cue pages, and
  full-transcript behavior.
- Extension playback observation, bounded newest-only forwarding, progressive
  cue-page handling, and page-local cue rendering behavior.

Run the routine checks from the repository root:

```powershell
npm --prefix extension run typecheck
npm --prefix extension test
npm --prefix extension run build

cargo fmt --manifest-path native/Cargo.toml --all -- --check
cargo clippy --manifest-path native/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path native/Cargo.toml --workspace --all-targets --locked
cargo build --manifest-path native/Cargo.toml --workspace --locked
```

These deterministic checks do not establish word accuracy, subtitle timing
quality on real recordings, real-time factor on a particular machine, or
network seeking efficiency. Those require the licensed real-media replay and
benchmark work described in [TESTING.md](TESTING.md) and
[BENCHMARKS.md](BENCHMARKS.md).

## Deliberate limits and next work

- **Known duration is required for buffered subtitles.** A durationless
  subtitle job uses the pre-Phase-5 whole-media path; it cannot safely choose a
  bounded future window.
- **No durable engine or cache yet.** The current in-process host owns
  scheduler/cue state only while it lives. There is no restart recovery,
  persistent range index, or retained near-playhead cue cache.
- **No performance guarantee or automatic alternative.** The status can
  recommend a short pause, but benchmark-based model selection, a user-facing
  smaller-model choice, and explicit cloud fallback remain future work.
- **No full browser/platform claim.** Chromium end-to-end overlay tests and
  direct-media acquisition under real sessions remain unimplemented. Phase 6
  adds only a caption-only YouTube overlay and Webex/Zoom route recognition;
  see [PHASE_6_PLATFORM_ADAPTERS.md](PHASE_6_PLATFORM_ADAPTERS.md). DRM,
  encryption, authentication bypass, and cookie/header extraction remain out
  of scope.
- **No cross-chunk revision policy yet.** The current path offsets and
  segments each finalized range. A durable cache and neighboring-chunk
  reconciliation are needed before making quality claims across every boundary.
- **No real-media buffer benchmark yet.** The opt-in real local-pipeline
  helper can validate an explicitly supplied licensed fixture, but it is not a
  controlled Phase 5 playhead replay and does not prove ahead-of-playback
  performance.

The next highest-value work is a licensed real-media replay with a controlled
playhead, then durable engine/cache and browser integration hardening. Those
results should set model-selection and adaptive-buffer rules rather than
assuming the current defaults fit every machine.
