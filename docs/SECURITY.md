# Subtitler Security and Privacy Model

**Status:** target V1 security design, with current-foundation notes called out explicitly.
**Security stance:** local-first, least privilege, no DRM/access-control bypass, and no browser-cookie or authentication-header extraction.

## Security commitments

Subtitler must uphold these product-level rules:

1. It does not bypass DRM, EME, encryption, login controls, platform signature protections, or other access restrictions.
2. It does not copy Chrome cookies, bearer tokens, or authorization headers into the native engine. An already-authorized page may expose a safe direct representation or platform download; if it cannot, Subtitler says the media is inaccessible.
3. Local processing is the default. A cloud provider never receives recording data unless the user explicitly chooses that provider for that job after a clear disclosure.
4. It never logs recording audio, transcript content, source query tokens, signed URLs, cookies, tokens, API keys, or unredacted FFmpeg command lines.
5. The extension is not a general-purpose native command runner, browser-network proxy, or credential-export mechanism.

## Current foundation versus target controls

| Area | Present in the foundation | Required before production release |
| --- | --- | --- |
| Extension privilege | MV3 manifest uses `activeTab`, `scripting`, `storage`, `nativeMessaging`, plus narrowly scoped `downloads`/`offscreen` for an explicit completed-transcript save flow | optional host permission UX, CSP review, end-to-end sender validation tests |
| Media policy | HTTPS-by-default parsing, rejection of embedded credentials/browser-only schemes/private-IP literals, per-hop DNS validation/pinning, bounded manual redirects, proxy-free capped staging, file-only FFmpeg, DRM hint rejection, reliable-captions-first policy | live acquisition tests, configurable cache budgets/retention, and representation-specific streaming/range policy |
| Native protocol | typed/validated initial protocol types and framed-host design | exact host allowlist registration, durable engine authentication, payload size limits, reconnect/fuzz tests |
| Transcript result delivery | completed-only bounded native segment pages; final cue-page drain; bounded transient service-worker cache; explicit fixed-name browser download route with a temporary Blob | durable reopenable result store, Chrome browser/save-dialog integration coverage, retention UX, and security review of packaged browser behavior |
| Local processing | fixed FFmpeg/whisper.cpp argument vectors, configured local binary/model validation, bounded diagnostics, cancellation/timeouts, private temporary WAV/JSON cleanup, atomic exports | signed/pinned binary/model packages, cache reaper after abnormal termination, hardware/model packages, and real-media hardening |
| Cloud | no automatic cloud behavior | provider disclosures, OS secret storage, upload minimization, provider integration/privacy review |

“Target” controls are design requirements, not evidence that an unimplemented component is already secure.

## Assets and trust boundaries

```text
Untrusted web page
  -> content script (page data is untrusted)
  -> MV3 service worker (extension privilege boundary)
  -> Native Messaging host (Chrome allowlisted-origin boundary)
  -> private local engine IPC (OS user + installation-secret boundary)
  -> media/decoder/model/cache (untrusted bytes and sensitive local data)

Optional explicit cloud provider
  <- selected normalized audio only, over TLS
```

| Asset | Sensitivity | Persistence policy |
| --- | --- | --- |
| Raw recording, decoded audio, VAD chunks | highly sensitive | stream where possible; otherwise job-private temp cache deleted on completion/cancel/failure unless explicitly retained |
| Transcript, word timings, speaker labels | sensitive | target: retained locally only while user retention policy permits in app-private database/files. Current Phase 8 extension: receives only bounded display segments/cues in service-worker memory after completion; never writes them to `chrome.storage`, and loses them on worker restart. |
| Direct/signed media URL | secret-like and short-lived | in memory for acquisition; redacted from all logs; never used as a persistent source identifier |
| Browser cookies, auth headers, OAuth tokens | credential | never requested/accepted by the engine and never persisted by the extension |
| Cloud API key | credential | OS protected secret store only; never extension storage, source tree, CLI argument, log, or export |
| Model/FFmpeg binaries | executable supply-chain asset | package-managed/pinned, checksum/signature verified, license recorded |
| Job IDs, coarse progress, model selection | low-to-moderate sensitivity | minimal local SQLite metadata; no transcript contents in job summaries |

The user account is the operating-system trust boundary. User-only ACLs and a per-installation local secret reduce accidental cross-process access, but same-user malware can often access user data and is not a boundary an application can completely solve on its own.

## Extension permissions and web-page boundary

The current manifest asks only for:

| Permission | Purpose | Constraint |
| --- | --- | --- |
| `activeTab` | inspect/inject only after the user invokes Subtitler | temporary active-tab access, not permanent all-site access |
| `scripting` | inject the approved content/overlay scripts | injected code is packaged with the extension; no remote code |
| `storage` | small non-secret preferences and UI/job routing state | no transcript, credentials, or raw URLs |
| `nativeMessaging` | control-plane connection to the local host | only the allowlisted native host name |
| `downloads` | start one user-requested browser export after a completed transcript | fixed product filename, `saveAs`, one active temporary Blob download; no page-provided path or automatic export |
| `offscreen` | create/revoke the temporary extension Blob URL used by that one download | created only with the `BLOBS` reason; bundled document has no Subtitler route to storage, page media, native messaging, or the downloads API |

When V1 needs a site beyond the active tab, it requests an `optional_host_permissions` grant at the point of use and explains why. It does **not** request `cookies`, `webRequest`, `debugger`, `declarativeNetRequest`, broad `<all_urls>` content-script access, microphone capture, or tab-audio capture for normal operation. Chrome's guidance is to minimize permissions and use `activeTab`/optional permissions where feasible. [Chrome extension privacy guidance](https://developer.chrome.com/docs/extensions/develop/security-privacy/user-privacy) [Permission declaration guidance](https://developer.chrome.com/docs/extensions/develop/concepts/declare-permissions) [Chrome downloads API](https://developer.chrome.com/docs/extensions/reference/api/downloads) [Chrome offscreen API](https://developer.chrome.com/docs/extensions/reference/api/offscreen)

All page-originated data is adversarial, including DOM attributes, title, duration, `<track>` URLs, caption text, player events, and messages forwarded by a content script. The extension:

- accepts only a discriminated, runtime-validated message schema;
- binds page messages to the expected tab/frame and a per-tab random nonce;
- permits a native operation only after a recent user gesture and source validation;
- uses `textContent`/safe DOM creation for subtitle and transcript text, never `innerHTML` from media/page/provider data;
- maintains a restrictive extension Content Security Policy and contains no `eval`, dynamic script injection, or remotely hosted executable code.

### Current Phase 8 transcript and browser-export boundary

The current native host admits `get_transcript_segments` only after job
completion. A page contains the minimum display DTO—segment timing, text, and
an optional speaker label—and is bounded to 100 segments and 120 KiB, with
per-field size limits. It omits word timestamps, language/translation metadata,
media details, source URLs, native paths, and native export locations. The
extension revalidates each page, drains the separately bounded final cue pages,
and keeps the result only in a capped service-worker-memory cache. It never
writes transcript text or cues to `chrome.storage`; stopped or disconnected
incomplete jobs are discarded.

The only browser-visible export route starts from an explicit popup action on a
completed transcript. The background renderer accepts only the five fixed
formats and fixed product filenames, creates at most one active export, and
fails rather than truncating a rendered output over 16 MiB. TXT and
timestamped TXT use display segments; SRT and VTT use the final native cue
pages; and browser `Transcript.json` contains only display-safe segment
records. It is not the native engine's canonical word-level JSON artifact.

The transcript text crosses to a bundled offscreen document only to construct
and revoke a temporary extension Blob URL. That document does not receive page
media, local media paths, cookies, credentials, native messages, or persistent
storage access, and it cannot initiate the download itself. The service worker
starts the Chrome download with `saveAs`; it revokes the Blob on a terminal
download state, with a short independent TTL as a cleanup backstop. This is
unit-tested at the message, formatting, and lifecycle boundaries, but has not
yet been exercised with a real Chrome save dialog.

## Media-access and authentication policy

The permitted acquisition order is existing captions, direct accessible media URL, local file, and a platform's ordinary authorized download. A platform adapter may use the fact that the user is already signed in to discover an accessible direct/signed URL or start that platform's normal download flow, but it must not export the user's browser session into the engine.

The engine validates every remote descriptor before it fetches:

- allow only `https` in release builds (explicitly documented development exceptions may allow `http` for local fixtures);
- require a hostname and reject URL-embedded username/password;
- strip fragments before persistence; redacts query values in diagnostic paths;
- bind the requested URL to the selected media candidate and intended origin/representation;
- resolve each hostname immediately before its outbound request; reject every
  private/link-local/loopback/reserved answer and pin the fresh client to only
  that approved address set, preventing a second decoder-side DNS lookup;
- follow at most three redirects manually, revalidating scheme, hostname, and
  every DNS result before the next connection; automatic redirects, system
  proxies, cookies, compression, HTTP/2, and HTTP/3 are disabled for this
  direct-media path;
- stream the response to a capped private temporary file (currently 4 GiB by
  default) with cancellation checks, then give FFmpeg only that local file
  with `-protocol_whitelist file`; it never receives a remote URL;
- impose additional input duration, cache, retry, process, and concurrency
  budgets as the durable engine is introduced;
- reject `blob:`, `data:`, `filesystem:`, `javascript:`, `chrome:`, and equivalent browser-only schemes at the native boundary.

Media bytes, containers, codecs, subtitles, and model files are untrusted input. Decoder invocation uses a fixed executable path and direct argument vector, never a shell. Extraction receives audio only; it does not decode video merely to transcribe it. A remote media source is currently fully staged before its first scheduled subtitle range; nested HLS/DASH manifest fetching fails closed under the file-only decoder policy. Decoder crashes/timeouts become job failures with cleanup, not application-wide crashes.

Browser CORS, MSE `blob:` sources, service-worker state, cookie partitioning, and EME intentionally limit what a native engine can fetch. Those limits are not a reason to use DevTools network interception, screen capture, signature deciphering, or cookie-store scraping. Chrome distinguishes extension cross-origin privileges from content-script/page privileges; the product accepts this limitation rather than treating it as an exploit target. [Chrome cross-origin request model](https://developer.chrome.com/docs/extensions/develop/concepts/network-requests)

## Native host and engine boundary

The released native-host name is `com.subtitler.native_host`. The packaged native-host manifest has an exact production extension ID in `allowed_origins`; Chrome does not permit wildcards. Chrome launches the host over stdio, so all host `stdout` bytes must be correctly framed Native Messaging JSON; diagnostics belong on `stderr` or a redacted local log. [Chrome Native Messaging](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)

The target durable engine accepts commands only over a per-user named pipe (Windows) or filesystem-permissioned Unix domain socket (macOS/Linux). It does not expose a localhost HTTP/TCP server. The endpoint uses both OS user permissions and a locally protected installation secret passed by the native host. Requests are schema-validated, size-bounded, and authorization-aware before work is created.

The engine must never accept:

- a shell command, executable path, FFmpeg option string, or arbitrary environment variable from a page/extension message;
- an arbitrary user-provided destination path for an export without an explicit native save-location action and canonical-path checks. The current browser export accepts no destination path at all: it supplies a fixed product filename and leaves the user-visible save location to Chrome;
- a request to fetch another origin using a media URL/session intended for a different source;
- browser cookies or HTTP authorization headers;
- unauthenticated data from a local TCP/HTTP client because normal operation has no such listener.

## Local storage, logging, and support data

Temporary job directories are created in an app-private data directory with random names, fixed ownership, and an explicit cleanup deadline. The cleanup reaper runs after abnormal termination as well as normal job completion. Cache quotas are enforced before decode/download work begins. “Keep media” is off by default and requires an explicit advanced setting.

Logs carry only timestamp, component, opaque job ID, stable event/error code, duration/byte buckets, and redacted diagnostic categories. They must not contain user media text or location. Telemetry is off by default; any future opt-in telemetry must be aggregation-only and independently reviewed.

A support bundle is an explicit action that previews included files. Its default contents exclude media, transcript, source URLs/query strings, cookies, API keys, local database, and model/cache data. Sanitization has regression tests using known secret-bearing fixtures.

## Cloud processing controls

**Current implementation:** the Rust workspace has a provider-agnostic routing
and consent contract only. It has no HTTP uploader, extension setting, host
job route, or API-key persistence implementation, so it cannot transmit audio
to a cloud provider. The contract defaults to local selection and rejects a
cloud request without a matching non-serializable per-job consent. That
consent is bound to the job, provider, model, exact internally validated
endpoint, redacted source identity, and audio scope; its serializable
disclosure excludes API keys, endpoint paths, source URLs, local paths, query
strings, and session data.

The target cloud capability is optional, never an automatic failure fallback.
Before upload, the UI identifies the selected provider and model, what audio
range will leave the device, stated data-retention/privacy link,
destination/region when known, estimated cost when available, and the
local-performance reason for offering it. The user chooses it per job.

The provider implementation:

- uses TLS and validates the provider endpoint against a configured allowlist,
  then resolves/pins every connection and revalidates bounded redirects before
  upload;
- uploads only audio required for the chosen job/range when provider capability allows;
- stores API keys only in OS protected storage and zeroizes short-lived copies where practical;
- never writes API keys to `chrome.storage`, source configuration, logs, crash reports, or exports;
- normalizes cloud results into the same local transcript/cue data model;
- records a non-content audit event that cloud was selected, so the user can understand the job's privacy mode.

## Supply chain and update controls

- TypeScript packages and Rust crates are lockfile-resolved; release builds generate an SBOM and run dependency/vulnerability review.
- `whisper.cpp`, FFmpeg, diarization models, and model weights are pinned by immutable version/source plus SHA-256 (and publisher signature when available). The engine rejects a hash mismatch before activation.
- Engine/native-host installers are code signed. macOS builds are notarized before release. The extension package is built reproducibly from the same tagged source.
- No remote configuration can supply executable JavaScript, native binaries, FFmpeg flags, or model URLs without a signed release manifest.
- A security update supports a safe in-place replacement, verifies signatures before install, and maintains database migration backups.

## Security verification requirements

Before V1 release, automated checks must cover:

- Native Messaging framing: malformed length, malformed UTF-8/JSON, >256 KiB application message, unknown field, duplicate request ID, and protocol downgrade.
- Extension sender checks: spoofed page message, wrong tab/frame nonce, stale user gesture, untrusted caption text, and attempted arbitrary URL request.
- Transcript/export boundary: completed-only page admission, transcript/cue and UTF-8 byte limits, cache discard on stop/disconnect, fixed export format/name validation, offscreen sender/origin validation, Blob revocation on terminal/TTL cleanup, and a real Chrome save-dialog check before release.
- Acquisition policy: private-network/redirect abuse, embedded credentials, unsupported schemes, DRM hint, expired URL, quotas, corrupt media, and decoder timeout/crash.
- Retention/redaction: cancellation cleanup, startup orphan reaping, no fixture transcript/token/URL appears in a log or support bundle, and cache limit enforcement.
- Provider controls: no cloud fallback without explicit selection, secret-store use, endpoint allowlist, cancellation, and provider error redaction.
- Packaging: manifest exact extension allowlist, signed binary/model verification, upgrade/downgrade behavior, host not registered after uninstall.

Dynamic/fuzz testing targets parser boundaries in the native protocol, subtitle parsers, media metadata, and URL policy. Threat-model review is required before introducing a new platform adapter, cloud provider, browser permission, or media-acquisition technique.

## Reporting and response

Before public distribution, the project must publish a monitored security-reporting channel, supported-version policy, disclosure handling process, and signed advisory procedure. Do not place a security contact address in a release until the organization can actually monitor it. A report containing media, transcripts, tokens, or real recording URLs must be handled as sensitive information and minimized immediately.

For broader system design, see [ARCHITECTURE.md](ARCHITECTURE.md), the exact target [NATIVE_MESSAGING_PROTOCOL.md](NATIVE_MESSAGING_PROTOCOL.md), and [INSTALLATION.md](INSTALLATION.md).
