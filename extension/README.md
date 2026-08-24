# Subtitler Chrome Extension

This is the Manifest V3 browser surface for Subtitler. It detects top-level
HTML5 audio/video only after the user opens the extension, presents the two
primary actions, renders timestamped cues in-page, and talks to the local
Subtitler Engine through Chrome Native Messaging.

It is intentionally not a transcription runtime. The native companion owns
media acquisition, audio decoding, ASR, buffering, and exports.

## Privacy and permission model

The manifest requests only:

- `activeTab` — temporary access to the tab the user invokes Subtitler on.
- `scripting` — inject the content controller into that active tab on demand.
- `storage` — persist sanitized job metadata across MV3 service-worker restarts.
- `nativeMessaging` — communicate with the locally installed engine.

There are no persistent host permissions, no broad `content_scripts` match,
and no `cookies`, `webRequest`, `downloads`, or `tabs` permission. The
extension does not retrieve browser cookies, forward authentication tokens, or
attempt DRM/encryption/access-control bypasses. Direct media URLs are transient
native-job input; they are never placed in `chrome.storage` or extension logs.

MSE/blob sources and media with an attached EME `MediaKeys` instance are treated
as non-direct/protected for generated transcription. Existing browser text
tracks can still be displayed locally.

### Local media files

Chrome requires the user to enable **Allow access to file URLs** for this
extension before it can inspect local HTML5 media. Subtitler accepts a local
media path only when the active tab is itself a safe local file document; the
path is derived transiently from that media URL and is never persisted or
logged. UNC/remote file hosts, ambiguous paths, and pasted paths or file URLs
are rejected.

## Native-messaging contract

The registered host name is `com.subtitler.native_host`, with protocol version
`1`. Every inbound native message is schema-checked before it can affect page
UI or persisted job state.

The extension sends either a direct HTTPS media URL without embedded
credentials or a validated local_file source from a local file tab. It does not
send browser cookies or session tokens. Authenticated platform adapters that
need browser-mediated acquisition belong in a later, explicitly user-approved
design rather than this foundation.

## Development

From this directory:

```powershell
npm install
npm run validate
```

Individual commands:

```powershell
npm run typecheck
npm test
npm run build
```

Load `dist/` as an unpacked extension from `chrome://extensions` after a build.
The companion native host must be installed separately and registered under the
host name above before generated jobs can start. Existing caption overlays can
run without it.

## Layout

```text
src/background/  MV3 service worker, persisted job state, native port
src/content/     on-demand HTML5 detection and existing-caption bridge
src/overlay/     pure cue timeline plus page/fullscreen overlay controller
src/popup/       deliberately small action-first popup
src/shared/      serializable domain and native-messaging protocol types
tests/           pure detector, cue-timeline, and protocol tests
```

`content.js` is injected on demand using `activeTab`; the service worker is the
only component that opens a native-messaging connection. Subtitle cues are kept
only in page memory, while persisted jobs retain operational metadata such as
progress and failure state—not transcript or recording contents.
