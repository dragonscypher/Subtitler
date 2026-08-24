# Phase 7: Local-first processing plans and consent-gated cloud contracts

**Status:** bounded implementation foundation. The native workspace now has a
conservative local-model planner and a provider-agnostic cloud-routing
contract. It does not yet ship model assets, a model manager, a cloud uploader,
or a diarization package.

## Delivered local-model planning

`subtitler-asr` turns a small `HardwareProfile` into a deterministic local
recommendation. The recommendation is deliberately advisory rather than a
throughput promise:

- it considers logical CPU count, currently available system memory, and only
  backends that the host explicitly says its installed build supports;
- it prefers CUDA, then Metal, then Vulkan, then CPU when more than one
  backend is explicitly available;
- it chooses the highest conservatively feasible local model and a
  quality-first quantization (`q5_k_m` by default, with limited safe `q8_0`
  cases); it never auto-selects `f16` because dedicated accelerator memory is
  not yet measured;
- it reports one coarse state: `excellent`, `good`, `may_be_slow`, or
  `cloud_helpful`. The last state still retains a `tiny` CPU local plan and
  never initiates an upload.

The current native host obtains logical CPU count from the operating system and
available memory through `sysinfo`. It deliberately does **not** scan GPU
names, drivers, or VRAM. A non-CPU backend is considered only when an
installer/development environment explicitly declares a compiled backend via
`SUBTITLER_COMPILED_BACKENDS`; an absent declaration means CPU-only planning.
This avoids treating a visible device name as proof that the installed
whisper.cpp build can use it.

The handshake can expose an optional, non-sensitive
`local_processing_advisory`. It includes only selection source, model,
quantization, backend, and coarse performance state—never memory totals,
device names, operating system, model paths, source media, or credentials.
The extension keeps it in memory only. The separate `local_asr_available` and
`ffmpeg_available` capability flags remain authoritative: an advisory can be a
plan for a missing engine/model, not evidence that local transcription is
ready.

### Current development-runtime constraint

The present development host accepts a caller-provided whisper.cpp model path.
Because that path has no signed model manifest proving its model family or
quantization, a runnable development configuration must set all three advanced
selection variables together:

```text
SUBTITLER_LOCAL_MODEL
SUBTITLER_MODEL_QUANTIZATION
SUBTITLER_COMPUTE_BACKEND
```

Partial values fail closed. This prevents the host from advertising an
automatically chosen `medium/q5` plan while actually launching an unrelated
file. A packaged model manager may remove this constraint only after it owns a
verified asset-to-metadata mapping.

## Cloud contract: explicit by construction

The cloud types are routing and disclosure contracts only. They define
`OpenAIProvider`, `OpenRouterProvider`, and `OpenAICompatibleProvider` behind a
common provider interface, but contain no HTTP client, upload method, host
wiring, or extension setting. Consequently, the current product has no cloud
fallback path at runtime and cannot silently transmit media.

Before a future uploader can select cloud, it must create a redacted
`CloudProcessingDisclosure` and receive a non-serializable per-job
`CloudProcessingConsent`. Route selection compares the same job, provider
kind/label, model, exact internally validated endpoint, source category,
redacted source host, and exact audio scope. The UI disclosure serializes only
the provider identity, model, origin-level endpoint identity, source category,
and range; it excludes endpoint paths, recording URLs, local paths, query
strings, browser credentials, and API keys.

Provider endpoints are restricted to HTTPS with a host, no embedded
credentials, query, or fragment, and no local/private literal or well-known
local domain. This is not a claim of full network safety for a future uploader:
that uploader must also resolve and validate DNS immediately before connecting,
pin the connection, manually validate bounded redirects, and keep its own
response/error redaction. The controlled direct-media acquirer is the model
for that boundary.

API keys use an opaque `ApiKeyProvider` callback and redacted `Debug`/`Display`
implementations. They have no serde representation. OS-protected key storage
and a real upload lifecycle remain packaging/runtime work.

## Speaker behavior

The requested `speaker_diarization` option remains a progressive enhancement.
The current whisper.cpp adapter advertises no lightweight diarization engine,
so it returns an accurate unlabeled transcript rather than fabricated speaker
names. Existing source-provided speaker metadata can remain attached when a
future adapter supplies it. A future local diarization component must pass a
licensing, speed, and quality gate; it will emit only `Speaker 1`, `Speaker 2`,
and so on unless the source itself provides reliable names. No general-purpose
LLM is used for identity inference.

## What remains before release

- Ship a signed model manifest/manager and actual compatible CPU/GPU model
  artifacts; validate selected asset/backend compatibility.
- Add benchmark-backed model thresholds using real low-end CPU, modern laptop,
  Apple Silicon, and CUDA test machines. The current planner is conservative,
  not a measured performance guarantee.
- Build a provider upload implementation only behind the exact disclosure and
  consent flow, with DNS/redirect pinning, cancellation, quota, error
  redaction, and OS credential storage.
- Add a licensed lightweight diarization engine only after it can run without
  starving cursor-protecting subtitle ASR.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the target data flow,
[SECURITY.md](SECURITY.md) for the privacy requirements, and
[BENCHMARKS.md](BENCHMARKS.md) for the required measurement corpus.
