/**
 * Caption-only YouTube integration helpers.
 *
 * This module intentionally looks only at `captions.playerCaptionsTracklistRenderer`
 * in a player response. It never discovers `streamingData`, media URLs, cookies,
 * authorization values, or other page-session state. A caption URL is still
 * sensitive request-scoped data, so callers must keep it in memory for the
 * current page/job and must not persist or log it.
 */

const MAX_CAPTION_TRACKS = 100;
const MAX_CAPTION_URL_LENGTH = 16_384;
const MAX_TIMEDTEXT_EVENTS = 20_000;
const MAX_SEGMENTS_PER_EVENT = 256;
const MAX_EVENT_TEXT_LENGTH = 16_384;

const YOUTUBE_PAGE_DOMAIN = "youtube.com";
const YOUTUBE_CAPTION_HOSTS = ["youtube.com", "youtube-nocookie.com"] as const;
const SENSITIVE_QUERY_PARAMETER = /(?:^|[_-])(?:access[_-]?token|auth(?:orization)?|bearer|cookie(?:s)?|credential(?:s)?|oauth|password|session(?:id)?|token)(?:$|[_-])/iu;

export type YoutubeCaptionKind = "manual" | "asr";

/**
 * A usable existing caption track. `captionBaseUrl` is deliberately the only
 * URL exposed by this adapter, and it is valid only for the current browser
 * session/page lifetime. Do not store it in extension storage or diagnostics.
 */
export interface YoutubeCaptionTrack {
  readonly id: string;
  readonly label: string;
  readonly language: string;
  readonly kind: YoutubeCaptionKind;
  readonly captionBaseUrl: string;
}

export interface YoutubeCaptionSelectionOptions {
  /** A BCP-47-ish language preference such as `en` or `en-US`. */
  readonly preferredLanguage?: string;
}

/** The normalized text shape extracted from a YouTube `fmt=json3` event. */
export interface YoutubeTimedTextSegment {
  readonly utf8: string;
}

/**
 * This intentionally mirrors the useful timedtext fields while containing
 * only cleaned caption text. A valid cue has one normalized UTF-8 segment so
 * consumers can concatenate `segs[].utf8` without needing to parse markup.
 */
export interface YoutubeTimedTextCueInput {
  readonly tStartMs: number;
  readonly dDurationMs: number;
  readonly segs: readonly YoutubeTimedTextSegment[];
}

/**
 * Recognize only video pages that this adapter knows how to inspect. The
 * extension should not treat a similarly named host, an arbitrary YouTube
 * page, or an authenticated URL as a media page.
 */
export function isYoutubeVideoPageUrl(input: string | URL): boolean {
  const url = parseUrl(input);
  if (!url || url.protocol !== "https:" || url.username || url.password) {
    return false;
  }

  const hostname = url.hostname.toLowerCase();
  if (hostname === "youtu.be") {
    return hasOnePathSegment(url.pathname);
  }
  if (!isSubdomainOf(hostname, YOUTUBE_PAGE_DOMAIN)) {
    return false;
  }

  if (url.pathname === "/watch") {
    return Boolean(url.searchParams.get("v")?.trim());
  }
  return hasSingleVideoIdPath(url.pathname, "embed") || hasSingleVideoIdPath(url.pathname, "shorts");
}

/**
 * Returns a safe, fragment-free YouTube timedtext endpoint, or `null`.
 * It accepts only the caption endpoint path; video/audio endpoints such as
 * `googlevideo.com/videoplayback` never pass this validator.
 */
export function sanitizeYoutubeCaptionBaseUrl(input: string | URL): string | null {
  const url = parseUrl(input);
  if (!url || url.href.length > MAX_CAPTION_URL_LENGTH) {
    return null;
  }
  if (url.protocol !== "https:" || url.username || url.password || (url.port !== "" && url.port !== "443")) {
    return null;
  }
  if (!isYoutubeCaptionHostname(url.hostname) || !isTimedTextPath(url.pathname)) {
    return null;
  }
  for (const parameter of url.searchParams.keys()) {
    if (SENSITIVE_QUERY_PARAMETER.test(parameter)) {
      return null;
    }
  }
  url.hash = "";
  return url.href;
}

/** True only for a safe HTTPS YouTube caption endpoint. */
export function isYoutubeCaptionEndpoint(input: string | URL): boolean {
  return sanitizeYoutubeCaptionBaseUrl(input) !== null;
}

/**
 * Extract existing YouTube caption tracks from a response-shaped unknown value.
 * The parser deliberately follows a small fixed set of caption-only paths and
 * does not recursively scan the response for URLs.
 */
export function extractYoutubeCaptionTracks(playerResponse: unknown): YoutubeCaptionTrack[] {
  const rawTracks = readCaptionTracks(playerResponse);
  if (!rawTracks) {
    return [];
  }

  const tracks: YoutubeCaptionTrack[] = [];
  const seenUrls = new Set<string>();
  const idCounts = new Map<string, number>();

  for (let index = 0; index < Math.min(rawTracks.length, MAX_CAPTION_TRACKS); index += 1) {
    const rawTrack = asRecord(rawTracks[index]);
    if (!rawTrack) {
      continue;
    }
    const captionBaseUrl = typeof rawTrack.baseUrl === "string" ? sanitizeYoutubeCaptionBaseUrl(rawTrack.baseUrl) : null;
    const language = readLanguage(rawTrack.languageCode);
    if (!captionBaseUrl || !language || seenUrls.has(captionBaseUrl)) {
      continue;
    }

    const kind: YoutubeCaptionKind = rawTrack.kind === "asr" ? "asr" : "manual";
    const label = readTrackLabel(rawTrack.name) ?? language;
    const baseId = readTrackId(rawTrack.vssId) ?? `youtube-caption-${index + 1}`;
    const id = allocateUniqueId(baseId, idCounts);

    tracks.push({ id, label, language, kind, captionBaseUrl });
    seenUrls.add(captionBaseUrl);
  }

  return tracks;
}

/**
 * Select only one of the provided existing tracks. Invalid or non-caption URLs
 * are revalidated here so a caller cannot accidentally pass a media endpoint
 * through this helper.
 */
export function chooseYoutubeCaptionTrack(
  tracks: readonly YoutubeCaptionTrack[],
  options: YoutubeCaptionSelectionOptions = {}
): YoutubeCaptionTrack | undefined {
  const preferredLanguage = normalizeLanguagePreference(options.preferredLanguage);
  const validTracks = tracks
    .map((track) => normalizeTrackForSelection(track))
    .filter((track): track is YoutubeCaptionTrack => track !== undefined);

  return validTracks.sort((left, right) => compareTracks(left, right, preferredLanguage))[0];
}

/**
 * Produces the in-memory URL a main-world fetch may request for the selected
 * existing caption track. It does not perform a fetch or attach cookies.
 */
export function createYoutubeTimedTextJson3Url(captionBaseUrl: string | URL): string | null {
  const safeBaseUrl = sanitizeYoutubeCaptionBaseUrl(captionBaseUrl);
  if (!safeBaseUrl) {
    return null;
  }
  const url = new URL(safeBaseUrl);
  url.searchParams.set("fmt", "json3");
  return url.href;
}

/**
 * Parse a decoded YouTube timedtext `fmt=json3` body into clean cue inputs.
 * Events without a positive, finite timestamp range or usable text are ignored.
 */
export function parseYoutubeTimedTextJson3(payload: unknown): YoutubeTimedTextCueInput[] {
  const root = asRecord(payload);
  const events = root && Array.isArray(root.events) ? root.events : undefined;
  if (!events) {
    return [];
  }

  const cues: Array<YoutubeTimedTextCueInput & { readonly sourceIndex: number }> = [];
  for (let index = 0; index < Math.min(events.length, MAX_TIMEDTEXT_EVENTS); index += 1) {
    const event = asRecord(events[index]);
    if (!event) {
      continue;
    }
    const timing = readTiming(event.tStartMs, event.dDurationMs);
    if (!timing) {
      continue;
    }
    const text = readEventText(event.segs);
    if (!text) {
      continue;
    }
    cues.push({ ...timing, segs: [{ utf8: text }], sourceIndex: index });
  }

  return cues
    .sort((left, right) => left.tStartMs - right.tStartMs || left.dDurationMs - right.dDurationMs || left.sourceIndex - right.sourceIndex)
    .map(({ tStartMs, dDurationMs, segs }) => ({ tStartMs, dDurationMs, segs }));
}

function parseUrl(input: string | URL): URL | null {
  try {
    return new URL(typeof input === "string" ? input : input.href);
  } catch {
    return null;
  }
}

function isSubdomainOf(hostname: string, domain: string): boolean {
  return hostname === domain || hostname.endsWith(`.${domain}`);
}

function hasOnePathSegment(pathname: string): boolean {
  const segments = pathname.split("/").filter(Boolean);
  return segments.length === 1 && isVideoIdSegment(segments[0]);
}

function hasSingleVideoIdPath(pathname: string, prefix: "embed" | "shorts"): boolean {
  const segments = pathname.split("/").filter(Boolean);
  return segments.length === 2 && segments[0] === prefix && isVideoIdSegment(segments[1]);
}

function isVideoIdSegment(value: string | undefined): boolean {
  return typeof value === "string" && /^[A-Za-z0-9_-]{1,128}$/u.test(value);
}

function isYoutubeCaptionHostname(hostname: string): boolean {
  const normalized = hostname.toLowerCase();
  return YOUTUBE_CAPTION_HOSTS.some((domain) => isSubdomainOf(normalized, domain));
}

function isTimedTextPath(pathname: string): boolean {
  return pathname === "/api/timedtext" || pathname === "/api/timedtext/";
}

function readCaptionTracks(value: unknown): unknown[] | undefined {
  const root = asRecord(value);
  if (!root) {
    return undefined;
  }
  const candidates = [root, asRecord(root.ytInitialPlayerResponse), asRecord(root.playerResponse)];
  for (const candidate of candidates) {
    const captions = candidate && asRecord(candidate.captions);
    const renderer = captions && asRecord(captions.playerCaptionsTracklistRenderer);
    if (renderer && Array.isArray(renderer.captionTracks)) {
      return renderer.captionTracks;
    }
  }
  return undefined;
}

function readLanguage(value: unknown): string | undefined {
  if (typeof value !== "string" || !isWellFormedUnicode(value)) {
    return undefined;
  }
  const language = value.trim();
  return /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/u.test(language) ? language : undefined;
}

function readTrackLabel(value: unknown): string | undefined {
  const name = asRecord(value);
  if (!name) {
    return undefined;
  }
  if (typeof name.simpleText === "string") {
    return normalizeCaptionText(name.simpleText, 256);
  }
  if (!Array.isArray(name.runs)) {
    return undefined;
  }
  let text = "";
  for (const run of name.runs.slice(0, 32)) {
    const candidate = asRecord(run)?.text;
    if (typeof candidate !== "string" || !isWellFormedUnicode(candidate)) {
      return undefined;
    }
    text += candidate;
    if (text.length > 512) {
      return undefined;
    }
  }
  return normalizeCaptionText(text, 256);
}

function readTrackId(value: unknown): string | undefined {
  if (typeof value !== "string" || !isWellFormedUnicode(value)) {
    return undefined;
  }
  const id = value.trim();
  return /^[A-Za-z0-9._:-]{1,128}$/u.test(id) ? id : undefined;
}

function allocateUniqueId(baseId: string, counts: Map<string, number>): string {
  const previousCount = counts.get(baseId) ?? 0;
  counts.set(baseId, previousCount + 1);
  return previousCount === 0 ? baseId : `${baseId}-${previousCount + 1}`;
}

function normalizeTrackForSelection(track: YoutubeCaptionTrack): YoutubeCaptionTrack | undefined {
  const captionBaseUrl = sanitizeYoutubeCaptionBaseUrl(track.captionBaseUrl);
  const id = readTrackId(track.id);
  const language = readLanguage(track.language);
  const label = normalizeCaptionText(track.label, 256);
  if (!captionBaseUrl || !id || !language || !label || (track.kind !== "manual" && track.kind !== "asr")) {
    return undefined;
  }
  return { id, label, language, kind: track.kind, captionBaseUrl };
}

function normalizeLanguagePreference(value: string | undefined): string | undefined {
  const language = readLanguage(value);
  return language?.toLowerCase().replace(/_/gu, "-");
}

function compareTracks(
  left: YoutubeCaptionTrack,
  right: YoutubeCaptionTrack,
  preferredLanguage: string | undefined
): number {
  return (
    languageScore(left.language, preferredLanguage) - languageScore(right.language, preferredLanguage) ||
    kindScore(left.kind) - kindScore(right.kind) ||
    left.label.localeCompare(right.label) ||
    left.id.localeCompare(right.id)
  );
}

function languageScore(language: string, preferredLanguage: string | undefined): number {
  if (!preferredLanguage) {
    return 0;
  }
  const normalizedLanguage = language.toLowerCase().replace(/_/gu, "-");
  if (normalizedLanguage === preferredLanguage) {
    return 0;
  }
  const primary = normalizedLanguage.split("-")[0];
  const preferredPrimary = preferredLanguage.split("-")[0];
  return primary === preferredPrimary ? 1 : 2;
}

function kindScore(kind: YoutubeCaptionKind): number {
  return kind === "manual" ? 0 : 1;
}

function readTiming(tStartMs: unknown, dDurationMs: unknown): Pick<YoutubeTimedTextCueInput, "tStartMs" | "dDurationMs"> | undefined {
  const valid =
    typeof tStartMs === "number" &&
    Number.isSafeInteger(tStartMs) &&
    tStartMs >= 0 &&
    typeof dDurationMs === "number" &&
    Number.isSafeInteger(dDurationMs) &&
    dDurationMs > 0 &&
    tStartMs <= Number.MAX_SAFE_INTEGER - dDurationMs;
  return valid ? { tStartMs, dDurationMs } : undefined;
}

function readEventText(value: unknown): string | undefined {
  if (!Array.isArray(value) || value.length === 0 || value.length > MAX_SEGMENTS_PER_EVENT) {
    return undefined;
  }
  let text = "";
  for (const segment of value) {
    const utf8 = asRecord(segment)?.utf8;
    if (typeof utf8 !== "string" || !isWellFormedUnicode(utf8)) {
      return undefined;
    }
    text += utf8;
    if (text.length > MAX_EVENT_TEXT_LENGTH) {
      return undefined;
    }
  }
  return normalizeCaptionText(text, MAX_EVENT_TEXT_LENGTH);
}

function normalizeCaptionText(value: string, maximumLength: number): string | undefined {
  if (!isWellFormedUnicode(value) || value.length > maximumLength) {
    return undefined;
  }
  const withoutFormatting = value
    .replace(/<\s*br\s*\/?\s*>/giu, " ")
    .replace(/<\/?[a-z][^>]{0,512}>/giu, "");
  const decoded = decodeHtmlEntities(withoutFormatting);
  const normalized = decoded
    .replace(/[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F\u200B-\u200D\uFEFF]/gu, " ")
    .replace(/\s+/gu, " ")
    .trim();
  return normalized.length > 0 && normalized.length <= maximumLength && isWellFormedUnicode(normalized) ? normalized : undefined;
}

function decodeHtmlEntities(value: string): string {
  const named: Readonly<Record<string, string>> = {
    amp: "&",
    apos: "'",
    gt: ">",
    lt: "<",
    nbsp: " ",
    quot: '"'
  };
  return value.replace(/&(#x[0-9a-f]+|#[0-9]+|[a-z]+);/giu, (entity, body: string): string => {
    const lowerBody = body.toLowerCase();
    if (lowerBody in named) {
      return named[lowerBody] ?? entity;
    }
    const codePoint = lowerBody.startsWith("#x")
      ? Number.parseInt(lowerBody.slice(2), 16)
      : lowerBody.startsWith("#")
        ? Number.parseInt(lowerBody.slice(1), 10)
        : Number.NaN;
    return isValidUnicodeCodePoint(codePoint) ? String.fromCodePoint(codePoint) : entity;
  });
}

function isValidUnicodeCodePoint(value: number): boolean {
  return Number.isInteger(value) && value >= 0 && value <= 0x10ffff && (value < 0xd800 || value > 0xdfff);
}

function isWellFormedUnicode(value: string): boolean {
  for (let index = 0; index < value.length; index += 1) {
    const codeUnit = value.charCodeAt(index);
    if (codeUnit >= 0xd800 && codeUnit <= 0xdbff) {
      const next = value.charCodeAt(index + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) {
        return false;
      }
      index += 1;
    } else if (codeUnit >= 0xdc00 && codeUnit <= 0xdfff) {
      return false;
    }
  }
  return true;
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
}
