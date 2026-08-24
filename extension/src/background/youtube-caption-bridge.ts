import type { CaptionTrackDescriptor, SubtitleCue } from "../shared/domain";
import {
  chooseYoutubeCaptionTrack,
  createYoutubeTimedTextJson3Url,
  extractYoutubeCaptionTracks,
  isYoutubeCaptionEndpoint,
  isYoutubeVideoPageUrl,
  parseYoutubeTimedTextJson3,
  type YoutubeCaptionTrack
} from "../platforms/youtube-captions";

const YOUTUBE_TRACK_ID_PREFIX = "youtube:";
const CAPTION_TRACK_CACHE_TTL_MS = 90_000;

/**
 * Page-local, one-use handoff between discovery and the immediate overlay
 * action. This never enters chrome.storage or native messaging. It avoids a
 * second read of YouTube's mutable player object, whose caption list can be
 * transiently unavailable after the page has already advertised a track.
 */
const discoveredCaptionTracks = new Map<string, CachedYoutubeCaptionTrack>();

interface CachedYoutubeCaptionTrack {
  readonly expiresAt: number;
  readonly track: YoutubeCaptionTrack;
}

/**
 * A deliberately non-sensitive outcome for a page-local caption request.
 * It never carries a URL, headers, cookies, or response content on failure.
 */
export type YoutubeCaptionCueLoadResult =
  | { readonly ok: true; readonly cues: SubtitleCue[] }
  | {
      readonly ok: false;
      readonly reason:
        | "invalid_caption_track"
        | "caption_metadata_unavailable"
        | "caption_endpoint_unavailable"
        | "caption_fetch_failed"
        | "caption_http_rejected"
        | "caption_redirect_rejected"
        | "caption_response_invalid"
        | "caption_cues_unavailable";
    };

/**
 * Discover existing YouTube tracks from the active page's own player state.
 * The injected function copies only caption fields; it never returns a player
 * response, media stream, cookie, header, or page/session object.
 */
export async function discoverYoutubeCaptionTracks(
  tabId: number,
  pageUrl: string | undefined
): Promise<CaptionTrackDescriptor[]> {
  if (!pageUrl || !isYoutubeVideoPageUrl(pageUrl)) {
    return [];
  }

  const playerResponse = await runYoutubeMainWorldFunction(tabId, captureYoutubeCaptionTracksInMainWorld);
  // V1's human-readable output is English. Do not silently substitute a
  // non-English source-language track just because it is the only one
  // available: translation belongs to the later language-provider path.
  const selected = chooseEnglishYoutubeCaptionTrack(extractYoutubeCaptionTracks(playerResponse));
  if (selected) {
    cacheYoutubeCaptionTrack(tabId, pageUrl, selected);
  }
  return selected ? [toCaptionTrackDescriptor(selected)] : [];
}

/**
 * Re-read and fetch one existing YouTube caption track using the page's
 * currently authorized browser session. The signed caption URL and fetched
 * body remain in ephemeral extension/page memory and are never persisted or
 * sent to native messaging.
 */
export async function loadYoutubeCaptionCues(
  tabId: number,
  pageUrl: string | undefined,
  descriptor: CaptionTrackDescriptor
): Promise<YoutubeCaptionCueLoadResult> {
  const trackId = youtubeTrackIdFromDescriptor(descriptor);
  if (!pageUrl || !trackId || !isYoutubeVideoPageUrl(pageUrl)) {
    return { ok: false, reason: "invalid_caption_track" };
  }

  const cached = takeCachedYoutubeCaptionTrack(tabId, pageUrl, trackId, descriptor.language);
  const playerResponse = cached ? undefined : await runYoutubeMainWorldFunction(tabId, captureYoutubeCaptionTracksInMainWorld);
  const track = cached ?? extractYoutubeCaptionTracks(playerResponse).find(
    (candidate) =>
      candidate.id === trackId &&
      candidate.language.toLowerCase() === descriptor.language?.trim().toLowerCase() &&
      isEnglishCaptionLanguage(candidate.language)
  );
  if (!track || !isEnglishCaptionLanguage(descriptor.language)) {
    return { ok: false, reason: "caption_metadata_unavailable" };
  }
  const timedTextUrl = createYoutubeTimedTextJson3Url(track.captionBaseUrl);
  if (!timedTextUrl) {
    return { ok: false, reason: "caption_endpoint_unavailable" };
  }

  const pageFetched = await runYoutubeMainWorldFunction(tabId, fetchYoutubeTimedTextJson3InMainWorld, timedTextUrl);
  // `MAIN` world fetch is preferred: it uses no extension host permission and
  // stays entirely in the page's own request context. Some YouTube CSP/network
  // policies reject that request. In that case use only the already-validated,
  // signed caption URL through the extension's narrowly scoped host permission.
  // This fallback explicitly omits cookies and does not persist or expose URLs.
  const fetched =
    isYoutubeTimedTextFetchResult(pageFetched) && pageFetched.status === "ok"
      ? pageFetched
      : await fetchYoutubeTimedTextJson3FromExtension(timedTextUrl);
  if (!isYoutubeTimedTextFetchResult(fetched)) {
    return { ok: false, reason: "caption_response_invalid" };
  }
  if (fetched.status !== "ok") {
    switch (fetched.status) {
      case "endpoint_unavailable":
        return { ok: false, reason: "caption_endpoint_unavailable" };
      case "fetch_failed":
        return { ok: false, reason: "caption_fetch_failed" };
      case "http_rejected":
        return { ok: false, reason: "caption_http_rejected" };
      case "redirect_rejected":
        return { ok: false, reason: "caption_redirect_rejected" };
      case "response_invalid":
        return { ok: false, reason: "caption_response_invalid" };
      case "cues_unavailable":
        return { ok: false, reason: "caption_cues_unavailable" };
    }
  }

  const parsed = parseYoutubeTimedTextJson3(fetched.payload);
  if (parsed.length === 0) {
    return { ok: false, reason: "caption_cues_unavailable" };
  }
  return {
    ok: true,
    cues: parsed.map((cue, index) => ({
    id: `${YOUTUBE_TRACK_ID_PREFIX}${track.id}:${index + 1}`,
    startSeconds: cue.tStartMs / 1_000,
    endSeconds: (cue.tStartMs + cue.dDurationMs) / 1_000,
    text: cue.segs.map((segment) => segment.utf8).join("")
    }))
  };
}

/**
 * Safe extension-context fallback for a short-lived signed caption endpoint.
 * Chrome may attach the current YouTube session directly to this same,
 * validated YouTube request. This code never reads, copies, serializes, or
 * persists cookies; it only needs the browser to satisfy YouTube's normal
 * consent/session check for a caption endpoint the page already exposed.
 * Redirects remain disabled and the endpoint never leaves this module. The
 * caller still applies the json3 field allowlist.
 */
export async function fetchYoutubeTimedTextJson3FromExtension(
  timedTextUrl: string
): Promise<YoutubeTimedTextFetchResult> {
  const maximumResponseCharacters = 2_000_000;
  const maximumResponseBytes = 4_000_000;
  if (!isYoutubeCaptionEndpoint(timedTextUrl)) {
    return { status: "endpoint_unavailable" };
  }

  let response: Response;
  try {
    response = await fetch(timedTextUrl, {
      // Use Chrome's existing YouTube session only for this strictly
      // validated first-party endpoint. Do not forward session data to the
      // native host, storage, logs, or any other origin.
      credentials: "include",
      redirect: "error",
      cache: "no-store"
    });
  } catch {
    return { status: "fetch_failed" };
  }
  if (!response.ok) {
    return { status: "http_rejected" };
  }
  const length = response.headers.get("content-length");
  if (length && (!/^\d+$/u.test(length) || Number(length) > maximumResponseBytes)) {
    return { status: "response_invalid" };
  }
  const reader = response.body?.getReader();
  if (!reader) {
    return { status: "response_invalid" };
  }

  try {
    const decoder = new TextDecoder();
    let receivedBytes = 0;
    let text = "";
    for (;;) {
      const chunk = await reader.read();
      if (chunk.done) {
        break;
      }
      receivedBytes += chunk.value.byteLength;
      if (receivedBytes > maximumResponseBytes) {
        await reader.cancel();
        return { status: "response_invalid" };
      }
      text += decoder.decode(chunk.value, { stream: true });
      if (text.length > maximumResponseCharacters) {
        await reader.cancel();
        return { status: "response_invalid" };
      }
    }
    text += decoder.decode();
    if (text.length > maximumResponseCharacters) {
      return { status: "response_invalid" };
    }
    const payload = parseYoutubeTimedTextPayload(text);
    return payload === undefined ? { status: "response_invalid" } : { status: "ok", payload };
  } catch {
    return { status: "response_invalid" };
  }
}

export function toCaptionTrackDescriptor(track: YoutubeCaptionTrack): CaptionTrackDescriptor {
  return {
    id: `${YOUTUBE_TRACK_ID_PREFIX}${track.id}`,
    kind: "captions",
    label: track.label,
    language: track.language,
    mode: "disabled",
    provider: "youtube"
  };
}

export function youtubeTrackIdFromDescriptor(descriptor: CaptionTrackDescriptor): string | undefined {
  if (descriptor.provider !== "youtube" || !descriptor.id.startsWith(YOUTUBE_TRACK_ID_PREFIX)) {
    return undefined;
  }
  const trackId = descriptor.id.slice(YOUTUBE_TRACK_ID_PREFIX.length);
  return /^[A-Za-z0-9._:-]{1,128}$/u.test(trackId) ? trackId : undefined;
}

/**
 * Picks a caption source that already meets the V1 English-output policy.
 * This is intentionally separate from the generic pure selector, whose
 * callers may have an explicit language plan in a later phase.
 */
export function chooseEnglishYoutubeCaptionTrack(
  tracks: readonly YoutubeCaptionTrack[]
): YoutubeCaptionTrack | undefined {
  return chooseYoutubeCaptionTrack(tracks.filter((track) => isEnglishCaptionLanguage(track.language)), {
    preferredLanguage: "en"
  });
}

function isEnglishCaptionLanguage(language: string | undefined): boolean {
  return typeof language === "string" && /^en(?:[-_][A-Za-z0-9]{1,16})?$/iu.test(language.trim());
}

function cacheYoutubeCaptionTrack(tabId: number, pageUrl: string, track: YoutubeCaptionTrack): void {
  clearExpiredCachedYoutubeCaptionTracks();
  discoveredCaptionTracks.set(captionCacheKey(tabId, pageUrl), {
    expiresAt: Date.now() + CAPTION_TRACK_CACHE_TTL_MS,
    track
  });
}

/** Consume rather than reuse a request-scoped endpoint. */
function takeCachedYoutubeCaptionTrack(
  tabId: number,
  pageUrl: string,
  trackId: string,
  language: string | undefined
): YoutubeCaptionTrack | undefined {
  clearExpiredCachedYoutubeCaptionTracks();
  const key = captionCacheKey(tabId, pageUrl);
  const cached = discoveredCaptionTracks.get(key);
  discoveredCaptionTracks.delete(key);
  if (
    !cached ||
    cached.track.id !== trackId ||
    cached.track.language.toLowerCase() !== language?.trim().toLowerCase() ||
    !isEnglishCaptionLanguage(cached.track.language)
  ) {
    return undefined;
  }
  return cached.track;
}

function clearExpiredCachedYoutubeCaptionTracks(): void {
  const now = Date.now();
  for (const [key, cached] of discoveredCaptionTracks) {
    if (cached.expiresAt <= now) {
      discoveredCaptionTracks.delete(key);
    }
  }
}

function captionCacheKey(tabId: number, pageUrl: string): string {
  return `${tabId}\u0000${pageUrl}`;
}

type YoutubeTimedTextFetchResult =
  | { readonly status: "ok"; readonly payload: unknown }
  | {
      readonly status:
        | "endpoint_unavailable"
        | "fetch_failed"
        | "http_rejected"
        | "redirect_rejected"
        | "response_invalid"
        | "cues_unavailable";
    };

function isYoutubeTimedTextFetchResult(value: unknown): value is YoutubeTimedTextFetchResult {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const status = (value as { status?: unknown }).status;
  if (
    status === "endpoint_unavailable" ||
    status === "fetch_failed" ||
    status === "http_rejected" ||
    status === "redirect_rejected" ||
    status === "response_invalid" ||
    status === "cues_unavailable"
  ) {
    return true;
  }
  return status === "ok" && "payload" in value;
}

/**
 * Accept YouTube's documented json3 body plus two harmless transport variants
 * seen on normal caption endpoints: an anti-XSSI prefix and legacy timedtext
 * XML. The latter is converted into the same narrow event shape before any
 * text reaches the normal json3 cue parser. This function does not fetch,
 * persist, log, or expose a caption endpoint.
 */
export function parseYoutubeTimedTextPayload(text: string): unknown | undefined {
  if (typeof text !== "string" || text.length > 2_000_000) {
    return undefined;
  }
  const normalized = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
  const xssiPrefix = ")]}'";
  const jsonText = normalized.startsWith(xssiPrefix)
    ? normalized.slice(normalized.indexOf("\n") + 1)
    : normalized;
  try {
    return JSON.parse(jsonText);
  } catch {
    return parseYoutubeTimedTextXml(normalized) ?? parseYoutubeTimedTextVtt(normalized);
  }
}

function parseYoutubeTimedTextXml(text: string): { readonly events: Array<Record<string, unknown>> } | undefined {
  const events: Array<Record<string, unknown>> = [];
  const textTag = /<text\b([^>]*)>([\s\S]*?)<\/text>/giu;
  const maximumEvents = 20_000;
  for (const match of text.matchAll(textTag)) {
    if (events.length >= maximumEvents) {
      break;
    }
    const attributes = match[1] ?? "";
    const startMs = parseCaptionSecondsAttribute(attributes, "start");
    const durationMs = parseCaptionSecondsAttribute(attributes, "dur") ?? parseCaptionSecondsAttribute(attributes, "duration");
    const value = match[2];
    if (startMs === undefined || durationMs === undefined || durationMs <= 0 || !value || value.length > 16_384) {
      continue;
    }
    events.push({ tStartMs: startMs, dDurationMs: durationMs, segs: [{ utf8: value }] });
  }
  return events.length > 0 ? { events } : undefined;
}

function parseCaptionSecondsAttribute(attributes: string, name: "start" | "dur" | "duration"): number | undefined {
  const match = new RegExp(`\\b${name}\\s*=\\s*["'](\\d{1,10}(?:\\.\\d{1,3})?)["']`, "iu").exec(attributes);
  if (!match?.[1]) {
    return undefined;
  }
  const seconds = Number(match[1]);
  const milliseconds = Math.round(seconds * 1_000);
  return Number.isSafeInteger(milliseconds) && milliseconds >= 0 ? milliseconds : undefined;
}

function parseYoutubeTimedTextVtt(text: string): { readonly events: Array<Record<string, unknown>> } | undefined {
  if (!/^\s*WEBVTT(?:\s|$)/iu.test(text)) {
    return undefined;
  }
  const events: Array<Record<string, unknown>> = [];
  const blocks = text.replace(/^\uFEFF/iu, "").split(/\r?\n\r?\n/gu);
  for (const block of blocks) {
    if (events.length >= 20_000) {
      break;
    }
    const lines = block.split(/\r?\n/gu);
    const timingIndex = lines.findIndex((line) => line.includes("-->"));
    if (timingIndex < 0) {
      continue;
    }
    const timing = /^\s*([^\s]+)\s+-->\s+([^\s]+)/u.exec(lines[timingIndex] ?? "");
    const startMs = timing ? parseVttTimestamp(timing[1]) : undefined;
    const endMs = timing ? parseVttTimestamp(timing[2]) : undefined;
    const value = lines.slice(timingIndex + 1).join("\n");
    if (startMs === undefined || endMs === undefined || endMs <= startMs || !value || value.length > 16_384) {
      continue;
    }
    events.push({ tStartMs: startMs, dDurationMs: endMs - startMs, segs: [{ utf8: value }] });
  }
  return events.length > 0 ? { events } : undefined;
}

function parseVttTimestamp(value: string | undefined): number | undefined {
  if (!value) {
    return undefined;
  }
  const match = /^(?:(\d{1,3}):)?(\d{2}):(\d{2})[.,](\d{3})$/u.exec(value);
  if (!match) {
    return undefined;
  }
  const hours = Number(match[1] ?? "0");
  const minutes = Number(match[2]);
  const seconds = Number(match[3]);
  const milliseconds = Number(match[4]);
  const total = (((hours * 60 + minutes) * 60 + seconds) * 1_000) + milliseconds;
  return minutes < 60 && seconds < 60 && Number.isSafeInteger(total) ? total : undefined;
}

async function runYoutubeMainWorldFunction<T>(
  tabId: number,
  func: (...args: string[]) => T | Promise<T>,
  ...args: string[]
): Promise<T | undefined> {
  try {
    const results = await chrome.scripting.executeScript({
      target: { tabId },
      world: "MAIN",
      func,
      args
    });
    return results[0]?.result as T | undefined;
  } catch {
    // Main-world inspection is best-effort. Do not expose page or session
    // details when a player does not permit the bounded caption operation.
    return undefined;
  }
}

/**
 * Serialized into the page's MAIN world by Chrome. Keep self-contained: an
 * injected function loses its extension closure and must not trust page data.
 */
function captureYoutubeCaptionTracksInMainWorld(): unknown {
  const maximumTracks = 100;
  const maximumStringLength = 16_384;
  const asRecord = (value: unknown): Record<string, unknown> | undefined =>
    typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
  const copyString = (
    source: Record<string, unknown>,
    target: Record<string, unknown>,
    key: string,
    maximumLength: number
  ): void => {
    const value = source[key];
    if (typeof value === "string" && value.length <= maximumLength) {
      target[key] = value;
    }
  };
  const copyName = (value: unknown): Record<string, unknown> | undefined => {
    const name = asRecord(value);
    if (!name) {
      return undefined;
    }
    if (typeof name.simpleText === "string" && name.simpleText.length <= 512) {
      return { simpleText: name.simpleText };
    }
    if (!Array.isArray(name.runs)) {
      return undefined;
    }
    const runs: Array<{ text: string }> = [];
    for (const rawRun of name.runs.slice(0, 32)) {
      const text = asRecord(rawRun)?.text;
      if (typeof text !== "string" || text.length > 512) {
        return undefined;
      }
      runs.push({ text });
    }
    return runs.length > 0 ? { runs } : undefined;
  };

  const root = globalThis as { ytInitialPlayerResponse?: unknown };
  const player = asRecord(root.ytInitialPlayerResponse);
  const captions = player && asRecord(player.captions);
  const renderer = captions && asRecord(captions.playerCaptionsTracklistRenderer);
  const tracks = renderer && Array.isArray(renderer.captionTracks) ? renderer.captionTracks : undefined;
  if (!tracks) {
    return null;
  }

  const copiedTracks: Array<Record<string, unknown>> = [];
  for (const rawTrack of tracks.slice(0, maximumTracks)) {
    const track = asRecord(rawTrack);
    if (!track) {
      continue;
    }
    const copied: Record<string, unknown> = {};
    copyString(track, copied, "baseUrl", maximumStringLength);
    copyString(track, copied, "vssId", 256);
    copyString(track, copied, "languageCode", 128);
    copyString(track, copied, "kind", 64);
    const name = copyName(track.name);
    if (name) {
      copied.name = name;
    }
    copiedTracks.push(copied);
  }

  return {
    captions: {
      playerCaptionsTracklistRenderer: {
        captionTracks: copiedTracks
      }
    }
  };
}

/**
 * Serialized into the page's MAIN world. It rechecks the strict endpoint
 * shape before using same-session `fetch`, disables redirects, bounds the
 * response, and returns only json3 event fields.
 */
async function fetchYoutubeTimedTextJson3InMainWorld(timedTextUrl: string): Promise<unknown> {
  const maximumStringLength = 16_384;
  const maximumResponseCharacters = 2_000_000;
  const maximumResponseBytes = 4_000_000;
  const maximumRedirects = 3;
  const maximumEvents = 20_000;
  const maximumSegments = 256;
  const asRecord = (value: unknown): Record<string, unknown> | undefined =>
    typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
  const isSafeNonNegativeInteger = (value: unknown): value is number =>
    typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
  const isSafePositiveInteger = (value: unknown): value is number =>
    typeof value === "number" && Number.isSafeInteger(value) && value > 0;
  const parseSafeUrl = (value: string): URL | undefined => {
    if (typeof value !== "string" || value.length > maximumStringLength) {
      return undefined;
    }
    try {
      const url = new URL(value);
      const hostname = url.hostname.toLowerCase();
      const allowedHost =
        hostname === "youtube.com" ||
        hostname.endsWith(".youtube.com") ||
        hostname === "youtube-nocookie.com" ||
        hostname.endsWith(".youtube-nocookie.com");
      if (
        url.protocol !== "https:" ||
        url.username ||
        url.password ||
        (url.port !== "" && url.port !== "443") ||
        !allowedHost ||
        (url.pathname !== "/api/timedtext" && url.pathname !== "/api/timedtext/")
      ) {
        return undefined;
      }
      for (const key of url.searchParams.keys()) {
        if (/(?:^|[_-])(?:access[_-]?token|auth(?:orization)?|bearer|cookie(?:s)?|credential(?:s)?|oauth|password|session(?:id)?|token)(?:$|[_-])/iu.test(key)) {
          return undefined;
        }
      }
      return url;
    } catch {
      return undefined;
    }
  };
  const copyEvents = (value: unknown): unknown => {
    const events = asRecord(value)?.events;
    if (!Array.isArray(events)) {
      return null;
    }
    const copied: Array<Record<string, unknown>> = [];
    for (const rawEvent of events.slice(0, maximumEvents)) {
      const event = asRecord(rawEvent);
      if (!event || !isSafeNonNegativeInteger(event.tStartMs) || !isSafePositiveInteger(event.dDurationMs)) {
        continue;
      }
      if (event.tStartMs > Number.MAX_SAFE_INTEGER - event.dDurationMs || !Array.isArray(event.segs)) {
        continue;
      }
      const segs: Array<{ utf8: string }> = [];
      for (const rawSegment of event.segs.slice(0, maximumSegments)) {
        const utf8 = asRecord(rawSegment)?.utf8;
        if (typeof utf8 !== "string" || utf8.length > maximumStringLength) {
          continue;
        }
        segs.push({ utf8 });
      }
      if (segs.length > 0) {
        copied.push({ tStartMs: event.tStartMs, dDurationMs: event.dDurationMs, segs });
      }
    }
    return { events: copied };
  };
  const decodePayload = (value: string): unknown => {
    const normalized = value.charCodeAt(0) === 0xfeff ? value.slice(1) : value;
    const jsonText = normalized.startsWith(")]}'") ? normalized.slice(normalized.indexOf("\n") + 1) : normalized;
    try {
      return JSON.parse(jsonText);
    } catch {
      const events: Array<Record<string, unknown>> = [];
      const textTag = /<text\b([^>]*)>([\s\S]*?)<\/text>/giu;
      const parseSeconds = (attributes: string, name: string): number | undefined => {
        const match = new RegExp(`\\b${name}\\s*=\\s*["'](\\d{1,10}(?:\\.\\d{1,3})?)["']`, "iu").exec(attributes);
        if (!match?.[1]) {
          return undefined;
        }
        const milliseconds = Math.round(Number(match[1]) * 1_000);
        return Number.isSafeInteger(milliseconds) && milliseconds >= 0 ? milliseconds : undefined;
      };
      for (const match of normalized.matchAll(textTag)) {
        if (events.length >= maximumEvents) {
          break;
        }
        const attributes = match[1] ?? "";
        const startMs = parseSeconds(attributes, "start");
        const durationMs = parseSeconds(attributes, "dur") ?? parseSeconds(attributes, "duration");
        const value = match[2];
        if (startMs === undefined || durationMs === undefined || durationMs <= 0 || !value || value.length > maximumStringLength) {
          continue;
        }
        events.push({ tStartMs: startMs, dDurationMs: durationMs, segs: [{ utf8: value }] });
      }
      if (events.length > 0) {
        return { events };
      }
      if (!/^\s*WEBVTT(?:\s|$)/iu.test(normalized)) {
        return { events: [] };
      }
      const parseVttTimestamp = (value: string | undefined): number | undefined => {
        if (!value) {
          return undefined;
        }
        const match = /^(?:(\d{1,3}):)?(\d{2}):(\d{2})[.,](\d{3})$/u.exec(value);
        if (!match) {
          return undefined;
        }
        const hours = Number(match[1] ?? "0");
        const minutes = Number(match[2]);
        const seconds = Number(match[3]);
        const milliseconds = Number(match[4]);
        const total = (((hours * 60 + minutes) * 60 + seconds) * 1_000) + milliseconds;
        return minutes < 60 && seconds < 60 && Number.isSafeInteger(total) ? total : undefined;
      };
      for (const block of normalized.replace(/^\uFEFF/iu, "").split(/\r?\n\r?\n/gu)) {
        if (events.length >= maximumEvents) {
          break;
        }
        const lines = block.split(/\r?\n/gu);
        const timingIndex = lines.findIndex((line) => line.includes("-->"));
        if (timingIndex < 0) {
          continue;
        }
        const timing = /^\s*([^\s]+)\s+-->\s+([^\s]+)/u.exec(lines[timingIndex] ?? "");
        const startMs = timing ? parseVttTimestamp(timing[1]) : undefined;
        const endMs = timing ? parseVttTimestamp(timing[2]) : undefined;
        const value = lines.slice(timingIndex + 1).join("\n");
        if (startMs === undefined || endMs === undefined || endMs <= startMs || !value || value.length > maximumStringLength) {
          continue;
        }
        events.push({ tStartMs: startMs, dDurationMs: endMs - startMs, segs: [{ utf8: value }] });
      }
      return { events };
    }
  };

  const parsed = parseSafeUrl(timedTextUrl);
  if (!parsed) {
    return { status: "endpoint_unavailable" };
  }
  // Some authorized YouTube caption endpoints issue a short same-origin
  // redirect before serving json3. Do not use Fetch's automatic redirect:
  // validate each hop and keep the request on the original page origin, so
  // credentials can never be forwarded to another host.
  let requestUrl = parsed;
  let response: Response | undefined;
  try {
    for (let redirectCount = 0; redirectCount <= maximumRedirects; redirectCount += 1) {
      response = await fetch(requestUrl.href, {
        credentials: "include",
        redirect: "manual",
        cache: "no-store"
      });
      if (![301, 302, 303, 307, 308].includes(response.status)) {
        break;
      }
      if (redirectCount === maximumRedirects) {
        return { status: "redirect_rejected" };
      }
      const location = response.headers.get("location");
      if (!location) {
        return { status: "redirect_rejected" };
      }
      let nextUrl: URL;
      try {
        nextUrl = new URL(location, requestUrl);
      } catch {
        return { status: "redirect_rejected" };
      }
      const validatedNextUrl = parseSafeUrl(nextUrl.href);
      if (!validatedNextUrl || validatedNextUrl.origin !== requestUrl.origin) {
        return { status: "redirect_rejected" };
      }
      requestUrl = validatedNextUrl;
    }
  } catch {
    return { status: "fetch_failed" };
  }

  try {
    if (!response) {
      return { status: "fetch_failed" };
    }
    if (!response.ok) {
      return { status: "http_rejected" };
    }
    const length = response.headers.get("content-length");
    if (length && (!/^\d+$/u.test(length) || Number(length) > maximumResponseBytes)) {
      return { status: "response_invalid" };
    }
    const reader = response.body?.getReader();
    if (!reader) {
      return { status: "response_invalid" };
    }
    const decoder = new TextDecoder();
    let receivedBytes = 0;
    let text = "";
    for (;;) {
      const chunk = await reader.read();
      if (chunk.done) {
        break;
      }
      receivedBytes += chunk.value.byteLength;
      if (receivedBytes > maximumResponseBytes) {
        await reader.cancel();
        return { status: "response_invalid" };
      }
      text += decoder.decode(chunk.value, { stream: true });
      if (text.length > maximumResponseCharacters) {
        await reader.cancel();
        return { status: "response_invalid" };
      }
    }
    text += decoder.decode();
    if (text.length > maximumResponseCharacters) {
      return { status: "response_invalid" };
    }
    const payload = copyEvents(decodePayload(text)) as { events?: unknown[] } | null;
    if (!payload || !Array.isArray(payload.events) || payload.events.length === 0) {
      return { status: "cues_unavailable" };
    }
    return { status: "ok", payload };
  } catch {
    return { status: "response_invalid" };
  }
}
