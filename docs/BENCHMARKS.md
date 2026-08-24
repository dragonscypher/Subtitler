# Benchmarks

**Status:** Phase 8 includes an opt-in local-pipeline smoke helper that emits a
single wall-clock real-time-factor calculation for a developer-supplied local
fixture. It has no pinned corpus, reference transcript, hardware matrix,
repeat runs, or result store, so it is not a benchmark suite and Subtitler has
no publishable word-error-rate, real-time-factor, memory, or subtitle-timing
result yet.

Current unit tests and build checks are described in [TESTING.md](TESTING.md). They validate deterministic logic and compilation; they are not a proxy for ASR accuracy or media-pipeline performance.

## Current local smoke measurement

With a locally authorized, licensed fixture and already-installed FFmpeg,
whisper.cpp CLI, and model, the manual command in
[TESTING.md](TESTING.md#opt-in-real-local-pipeline-check) runs:

```text
local media file -> FFmpeg normalized WAV -> whisper.cpp -> timestamps -> exports
```

Its `endToEndRealTimeFactor` is `wall elapsed time / developer-declared media
duration`, including host launch, decode, ASR, subtitle segmentation, and export
for that one run. The helper does not upload media or contact platforms, and it
does not print transcript content. It may be useful for setup diagnosis or an
early local sanity comparison, but it must not be compared across machines or
used to label a hardware plan as playback-capable. In particular, its subtitle
mode uses a synthetic completion playhead rather than a real player clock.

The helper's four-hour maximum run time and private temporary output are safety
bounds, not a benchmark protocol. Keep output only with its explicit
`-KeepArtifacts` switch, and do not retain customer recordings or transcripts
as benchmark evidence.

## Benchmark goals

The benchmark suite will answer four release questions:

1. Does a pinned local model produce sufficiently accurate English output for the selected quality tier?
2. Can the complete local pipeline produce final subtitle cues ahead of playback on a named hardware/model profile?
3. Are cue timing, reading constraints, and SRT/VTT output valid when output originates from real media?
4. Does the product detect a machine that cannot keep up and move to a clear fallback state instead of presenting delayed captions as synchronized?

Benchmarks must measure the whole relevant path, not just raw model inference. Decode, VAD, chunking, timestamp reconciliation, subtitle segmentation, and persistence can all affect time to a usable cue.

## Corpus and fixture rules

The planned corpus has two tiers:

| Tier | Contents | Use |
| --- | --- | --- |
| Public CI fixtures | Short, redistributable local MP4/AAC, WebM/Opus, WAV, and MP3 samples with checked reference transcripts and metadata. | Deterministic real-media regression tests. |
| Controlled benchmark corpus | Longer, licensed samples representing clear English speech, meetings with turn changes, accents, moderate noise, silence/music, and supported non-English-to-English translation. | Scheduled/manual accuracy and performance evaluation. |

Each corpus entry must record a stable ID, license/source, SHA-256, duration, container/codecs, language, reference transcript, reference cue/word timings where available, and known conditions such as overlap or noise. The repository may store small redistributable fixtures; large assets should be fetched from a versioned, access-controlled test-data location and checksum-verified.

Never add real customer recordings, meeting transcripts, browser-session data, signed URLs, cloud API keys, or personally sensitive media to a fixture corpus. Benchmark reports use opaque fixture IDs and aggregate metrics; they do not contain transcript text or media URLs.

## Planned benchmark commands

There is no corpus-backed benchmark command in the repository today. When that
harness lands, it should expose a stable, explicit interface similar to the
following:

```powershell
# Planned commands; unavailable until the benchmark harness is implemented.
.\scripts\benchmark.ps1 -Profile quick -Model small.en -Output .\artifacts\benchmarks\quick.json
.\scripts\benchmark.ps1 -Profile full -Model medium.en -Corpus controlled -Output .\artifacts\benchmarks\full.json
.\scripts\benchmark.ps1 -Profile replay -Model small.en -PlaybackRate 1.0 -Output .\artifacts\benchmarks\replay.json
```

The harness must make model download/setup a separate, visible step. A measurement run uses an already-verified model and fixture set, records the model/configuration hash, and fails rather than substituting a different model silently.

## Measurement definitions

| Metric | Definition |
| --- | --- |
| Word error rate (WER) | Word-level Levenshtein distance after a documented, language-aware normalization. The report preserves raw output for local reviewer use only; aggregate reports contain error counts and fixture IDs. |
| Translation quality | For non-English input translated to English, a fixed human-reference set plus blinded reviewer adequacy scores. English WER alone is not a translation-quality measure. |
| Real-time factor (RTF) | `elapsed processing time / media duration`, measured from decoder start to finalized transcript/cue persistence. Model download and one-time installation are excluded and reported separately. An RTF below 1.0 is faster than playback. |
| Cue-boundary error | Absolute difference between generated and reference cue/word boundary in milliseconds. Report median and 95th percentile rather than only an average. |
| Subtitle readability | Parse validity, monotonic positive cue intervals, number of lines, characters per line, characters per second, and boundary quality around punctuation/pauses. |
| Subtitle buffer lead | The contiguous finalized subtitle range after the active playhead. Report minimum, median, and lowest percentile during a 1.0x replay. |
| Resource use | Wall time, CPU time, peak working set, GPU backend/device when used, GPU memory/utilization when measurable, disk/cache bytes, and model startup time. |
| Fallback correctness | Time and lead depth at which a slower-than-playback run is detected, and whether it exposes pause-to-buffer, smaller-local-model, or explicit-cloud options without auto-uploading media. |

Every result must include the Subtitler commit, build mode, OS/version, CPU/GPU, memory, Rust/compiler/model/runtime version, model quantization/hash, backend, thread/concurrency setting, fixture-manifest hash, and whether it is a cold or warm run.

## Proposed V1 acceptance targets

These are proposed release targets, not current CI assertions. They must be recalibrated only through a documented corpus/model change, never by replacing a difficult fixture after seeing a result.

| Measure | Proposed qualifying target | Scope |
| --- | --- | --- |
| Clean English transcription | Median WER at or below 10%; 90th-percentile WER at or below 18%. | Pinned release model on the clear-speech English corpus. |
| Meeting-style English transcription | Median WER at or below 20%; 90th-percentile WER at or below 35%. | Pinned release model on the meeting/noise corpus. Speaker overlap is separately labeled, not hidden. |
| Non-English to English output | At least 90% of reviewed samples receive an adequacy score of 4/5 or higher from two blinded reviewers. | Supported language/reference subset; publish reviewer agreement. |
| Timestamp quality | Median cue-boundary error at or below 250 ms and 95th percentile at or below 750 ms. | Fixtures with approved reference timings. |
| Subtitle-file correctness | 100% of generated SRT/VTT outputs parse in independent parsers; 0 reversed/overlapping cue intervals; every cue satisfies the configured line limit. | All real-media regression fixtures. |
| Ahead-of-playback eligibility | A hardware/model plan may be labeled able to keep up only when 95th-percentile end-to-end RTF is at or below 0.8 during a 1.0x replay. | Named hardware profile and pinned model/backend. |
| Slow-device behavior | A run that exceeds the keep-up threshold surfaces a degradation state before presenting delayed captions as synchronized, with no cloud upload unless explicitly selected. | Deliberately throttled replay/integration test. |

No universal memory or CPU percentage target is set before hardware baselines exist. Instead, every supported hardware/model profile must publish peak working set and RTF. A profile that cannot meet the ahead-of-playback threshold remains usable for full transcripts or pause-to-buffer, but it must not be advertised as real-time-capable.

## Hardware matrix and run policy

The release matrix must include at least:

- a low-end x64 CPU laptop;
- a representative current x64 laptop;
- an Apple Silicon machine;
- a CUDA-capable desktop when that backend is distributed; and
- Windows, the primary V1 operating system.

GitHub-hosted Windows runners are suitable for compilation and deterministic unit tests but are not stable enough to enforce raw speed or memory regressions. Run performance profiles on named, controlled hardware through a scheduled or manually dispatched workflow, and store result artifacts outside the source tree according to the project's retention policy.

For each named profile, take at least one cold-start and three warm-start runs. Report median and spread, investigate a repeatable regression greater than 10% in RTF or peak working set, and never compare a cold run to a warm baseline.

## Ahead-of-playback replay test

The Phase 5 replay profile should use a real, local fixture and a controlled media clock:

1. Start subtitle generation at a known playhead and record finalized coverage over time.
2. Play at 1.0x, then seek to a covered timestamp and verify immediate cue selection.
3. Seek to an uncovered later timestamp and verify that scheduling moves to the new window before irrelevant earlier ranges.
4. Run a deliberately constrained model/hardware configuration that cannot achieve RTF 0.8 and verify the early fallback state.
5. Confirm that a separate full-transcript job continues complete-file work independently of the subtitle-priority job.

The result should report lead depth and cue correctness at each event, not just final transcript completion time.

## Benchmark promotion and regression policy

A benchmark run becomes a candidate baseline only when its fixture/model manifest is immutable, its environment metadata is complete, and it passes the functional real-media suite. Any intentional model, runtime, decoder, subtitle-segmentation, or hardware-policy change requires a before/after result against the same corpus.

Do not turn raw benchmark results into automatic PR gates until repeated controlled runs establish a stable baseline. Until then, CI continues to enforce the deterministic unit/smoke checks in [TESTING.md](TESTING.md), while benchmark regressions are reviewed from versioned scheduled/manual artifacts.
