# Phase 6: Platform Adapters

**Status:** generated-subtitle paths are deliberately audio-first. This phase
does not claim that every YouTube, Webex, or Zoom recording can be lawfully
retrieved, transcribed, or exported.

## Scope and product boundary

Phase 6 adds these safe platform boundaries:

1. A strict, optional existing-caption-only YouTube overlay route.
2. A native YouTube page adapter that asks bundled `yt-dlp` for a private
   local audio artifact, never a caption track or browser credential.
3. A bounded Webex same-origin page request that discovers the normal signed
   recording representation in the already-authorized page and hands only
   that address to the controlled native downloader.
4. Continued use of the existing generic direct HTML5 media path when a page
   exposes a normal direct HTTPS media source.

It does not add a general page scraper, a network recorder, an authenticated
media proxy, or a platform-specific transcription runtime.

| Situation | Current result |
| --- | --- |
| YouTube video page | **Create Subtitles** uses native `yt-dlp` audio acquisition, then local FFmpeg/ASR. **Use Existing Captions** is separate and optional. If YouTube requires a proof token, fail clearly rather than falling back to captions. |
| Webex recording URL | The authorized page may return a normal signed recording representation through the fixed same-origin recording endpoint; the extension retains no cookie/header/body and the native engine never receives one. |
| Zoom recording URL with an opaque/protected player | Recognize the route and show platform-specific no-bypass guidance. |
| Any supported page exposing a safe direct HTTPS HTML5 source | Use the generic native pipeline: controlled staging, then local-only FFmpeg/ASR, subject to source/media policy. |
| DRM/media-keys/protected media | Do not acquire media. A visible existing-caption overlay remains allowed because it does not read the protected stream. |

## Strict YouTube existing-caption overlay

The adapter recognizes only HTTPS video routes on exact `youtube.com`
subdomains or `youtu.be`:

- `/watch?v=<id>`
- `/embed/<id>`
- `/shorts/<id>`
- `youtu.be/<id>`

Credentialed URLs, HTTP, lookalike domains, non-video pages, and malformed
paths fall back to generic handling. Recognition is not evidence that a
recording or captions are actually available.

On media detection, the extension asks Chrome to run a self-contained,
allowlisted function in the active tab's MAIN world. It copies only a bounded
set of fields from the fixed
`captions.playerCaptionsTracklistRenderer.captionTracks` path:

```text
baseUrl, vssId, languageCode, kind, and bounded display-name fields
```

The adapter does not inspect `streamingData`, recursively search the player
response for URLs, return a player/session object, discover video/audio URLs,
or read or return cookie values, authorization headers, or other browser
credentials.

If a caption track is selected, the extension re-reads it from the same active
page and creates a `fmt=json3` timed-text request. The MAIN-world fetch:

- accepts only an HTTPS `youtube.com`/`youtube-nocookie.com`
  `/api/timedtext` endpoint;
- rejects credentialed URLs, non-default ports, lookalike hosts, unsafe
  token/cookie-like query parameter names, redirects, and oversized responses;
- uses `credentials: "include"` only inside the page's normal authorized
  context, `redirect: "error"`, and `cache: "no-store"`;
- copies back only bounded `tStartMs`, `dDurationMs`, and `segs[].utf8`
  event fields.

The extension validates and normalizes that event data, then sends subtitle
cues directly to the current page's overlay in pages of at most 200 cues. The
request-scoped caption endpoint (which can contain a short-lived platform
signature), returned player fields, fetched body, and cue text are ephemeral.
The endpoint exists briefly in extension/page memory solely to revalidate and
invoke the bounded page fetch. None of that data is written to
`chrome.storage`, logs, native messaging, job records, or export files.

**Use Existing Captions** is the only action that uses this overlay. The
default **Create Subtitles** and **Generate with Subtitler** actions never
route through a platform caption fetch: they start the native audio/FFmpeg/ASR
pipeline instead.

The V1 fast path selects only an English (`en` primary-language) track, with
manual tracks winning ties over ASR tracks. If only another language is
available, this adapter does not present it as an English result; it leaves the
page on the normal generated/unsupported path until the later explicit
translation layer exists. Phase 6 does **not** translate a foreign-language
caption track to English and does not offer a target-language selector.

## Webex and Zoom recognition

The Webex and Zoom classifier accepts only HTTPS, uncredentialed, exact-domain
recording routes. Webex additionally has a fixed authorized-page adapter:

| Platform | Narrow recognized route family | Delivered behavior |
| --- | --- | --- |
| Webex | `*.webex.com/recordingservice/.../playback/<id>` or a bounded `webappng/.../recording/<id>/playback` variant | Fetch the page's fixed same-origin recording-stream metadata endpoint with `credentials: "include"`; validate and return only its ordinary HTTPS media URL. |
| Zoom | `*.zoom.us/rec/play/<id>`, `/rec/share/<id>`, or bounded `/recording/...` equivalent | Safe `Zoom` label plus opaque-source guidance. |

The classifier drops query data. The Webex adapter accepts only the recording
identifier/site parsed from the recognized page URL, performs one fixed
same-origin request in that page's normal authorized context, and validates
only an HTTPS result before native handoff. It does not return cookies,
headers, response bodies, page objects, or a general network capability. A
recognized URL still does not prove that the recording is accessible or
non-DRM.

If a Webex or Zoom page exposes a standard direct HTML5 media URL, Subtitler
uses the same generic direct-source validation/pipeline as any other page.
Zoom does not yet have a Webex-equivalent metadata adapter.

## Source and protection rules

The current startup policy is intentionally ordered:

1. **Use Existing Captions** alone uses the page-local caption overlay.
2. For **Create Subtitles**, **Generate with Subtitler**, or a full transcript,
   reject media marked protected
   before native acquisition.
3. Permit a validated direct HTTPS representation, a fixed Webex page
   representation, or a recognized YouTube page adapter to enter the controlled
   downloader and then the local native pipeline; FFmpeg never receives a
   remote URL.
4. Reject missing, `blob:`, MSE, unsupported-scheme, and other opaque sources
   with a platform-aware explanation.

The product wording is deliberate: Subtitler will not copy browser
credentials, scrape cookie databases, replay authorization headers, decipher
protected signatures, extract DRM keys, bypass encryption/access controls, or
capture screen/browser audio as a workaround.

An opaque YouTube/Webex/Zoom player therefore cannot currently produce a
native full transcript, TXT/SRT/VTT/JSON export, or generated subtitles. The
YouTube existing-caption overlay is a separate, page-local display path; it is
not an export or transcription job.

## Privacy and session handling

The extension has `activeTab`, `scripting`, `storage`, and `nativeMessaging`
permissions; it does not request the Chrome `cookies`, `debugger`, or
network-interception permissions. The current authorized Webex page may perform
one constrained same-origin metadata request. Cookie values and authorization
headers never leave that page; only the validated temporary representation is
handed to native messaging and it is never persisted. The YouTube adapter has
no browser-session handoff at all.

Only safe operational metadata may persist in a job record. A YouTube caption
descriptor contains a provider marker, bounded track identifier, label, and
language; it deliberately omits the caption URL and caption body. The overlay
receives cue text only in page memory. Navigation, overlay teardown, or
service-worker loss does not create a durable caption cache.

## Automated coverage

Run the extension suite:

```powershell
npm --prefix extension test
```

The Phase 6 unit coverage includes:

- `youtube-captions.test.ts` — exact page/endpoint allowlists, fixed-path
  extraction, track selection, sensitive-query rejection, and JSON3 cue
  normalization.
- `youtube-caption-bridge.test.ts` — safe provider/track metadata that cannot
  contain a timed-text endpoint.
- `recording-platforms.test.ts` — strict YouTube/Webex/Zoom route recognition,
  lookalike/credential rejection, no query retention, and no-bypass guidance.

These are deterministic parser and policy tests. They do not run a real
Chrome MAIN-world injection, authenticate to a platform, fetch a real
timed-text endpoint, verify captions against a live video, or prove any
platform's download/media route.

## Known gaps and next work

- **YouTube proof-token enforcement and packaging.** The isolated adapter can
  now opt into the maintained `bgutil-ytdlp-pot-provider` through yt-dlp's
  plugin API when the installer-owned plugin directory and matching Deno
  provider server are both present. It scopes the provider cache to the
  private job directory and does not import browser cookies, profiles, static
  tokens, or captions. The supplied public-video browser acceptance test and
  a packaged, integrity-verified provider install remain required before
  release.
- **No opaque-source full transcript or export.** YouTube caption overlay text
  is not converted into an export bundle, and opaque/protected platform players
  cannot enter native transcription.
- **No caption translation.** The English-only fast path intentionally skips
  non-English existing tracks; foreign-language-to-English output is not
  implemented.
- **No Zoom platform media adapter.** Zoom recognition does not implement
  captions, direct stream resolution, normal-download handoff, or authenticated
  retrieval. The Webex metadata adapter still needs repeatable real-browser
  validation on representative authorized recordings.
- **No remote progressive-media acquisition.** A direct remote source is fully
  staged before FFmpeg range extraction begins. Nested HLS/DASH manifests fail
  closed under the local-file protocol policy.
- **No durable caption cache or reconnect behavior.** The caption endpoint and
  text live only for the active page/job operation.
- **No real platform validation yet.** Browser integration tests with
  authorized, non-DRM fixtures and a privacy review of the MAIN-world boundary
  are required before release.

The next highest-value work is a controlled browser integration suite using
authorized/non-DRM fixtures, followed by explicitly scoped platform-download
handoffs where the page/platform exposes them without credential transfer or
protection bypass.
