# Browser Source Validation — 2026-08-17

**Scope:** read-only validation of the two recordings supplied for the Subtitler
demo. This record deliberately excludes recording URLs, participant names,
caption bodies, media URLs, and session data.

## Result summary

| Source class | Browser evidence | Current Subtitler outcome |
| --- | --- | --- |
| Public YouTube music video | HTML5 player loaded; the player explicitly reported closed captions unavailable; no `<track>` elements were present. | Existing-caption overlay cannot start. **Create Subtitles** requires a configured local engine. |
| Authorized Webex recording | Recording page loaded and playback started; video duration reported 35:05; no visible transcript control or `<track>` elements; the media element exposed only a `blob:` source. | Safely unsupported for direct native transcription. The current host must not copy browser credentials or follow a page-owned `blob:` source. |

## What this proves

- The supplied YouTube page is reachable and contains an HTML5 video player.
- The supplied Webex page is reachable in the supplied browser context and can
  play its recording.
- Neither target exposed existing caption tracks usable by the Phase 6
  existing-caption path.
- The Webex target is not a direct-media source for the current local-first
  host. Rejecting it is correct: Subtitler does not turn browser session state
  or MediaSource `blob:` URLs into a native download channel.

## What remains before a true extension demo

1. Load the unpacked Chrome extension and register the developer Native
   Messaging host with that exact development extension ID.
2. Configure local FFmpeg, whisper.cpp, and a permitted local model for the
   native transcription route.
3. For this Webex form, use a platform-provided ordinary download/direct-media
   representation or implement a dedicated, authorized adapter that preserves
   the no-cookie/no-DRM/no-protection-bypass boundary.
4. Run the extension on a source with existing captions or a configured local
   ASR path, then make a screen recording. A failure/unsupported message is not
   represented as a successful subtitle or transcript demo.

## Test integrity

No media bytes, transcript text, cookies, credentials, direct/signed URLs, or
participant metadata were retained by this validation.
