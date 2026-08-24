# Phase 4: Generated Subtitle Overlay

**Status:** implemented and verified with deterministic native and extension
tests. Phase 5 extends this delivery path so finalized cue pages can arrive
while a subtitle job is processing; the overlay mechanics described here
remain the foundation.

## Delivered behavior

1. A local `subtitle_generation` job follows the generic Phase 3 pipeline and
   keeps renderer-ready cues in the native host's in-memory runtime and final
   job outcome. Native export paths and transcript bodies stay private to the
   host.
2. The original implementation requested `get_subtitle_cues` after
   `completed`. Phase 5 also exposes append-only finalized pages during
   `processing`. Each page is limited to 200 cues and a 128 KiB serialized
   response budget, safely below Chrome Native Messaging's 1 MiB host-output
   maximum.
3. The TypeScript client validates the Rust wire DTO, converts millisecond
   timing to browser seconds, preserves subtitle line breaks, and assigns
   stable cue IDs from the native job/cursor.
4. Pages cross the extension/content-script boundary individually. The
   page-local Shadow DOM overlay merges and deduplicates their cues, renders
   against the HTML media clock, and keeps its existing play/pause/seek/rate
   and fullscreen behavior.

Transcript-derived cue text is never placed in `chrome.storage`, and it is
discarded from the extension service worker when the native port disconnects.

## Phase 4 boundary

Phase 4 owns page-local overlay rendering and bounded cue delivery. It does
not itself choose media ranges, measure processing speed, retain a durable cue
cache, or make a performance promise. Those concerns are now handled by the
bounded Phase 5 scheduler; see
[PHASE_5_AHEAD_OF_PLAYHEAD.md](PHASE_5_AHEAD_OF_PLAYHEAD.md).

## Next highest-value work

Harden the boundary with a durable native engine, browser integration tests,
real-media buffer/accuracy benchmarks, and cache/reconnect behavior. Keep the
bounded page protocol as the delivery channel for the near-playhead cue window.
