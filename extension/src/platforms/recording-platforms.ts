/**
 * Pure, URL-only classification for the recording platforms supported by V1.
 *
 * This module does not inspect a document, make a network request, discover a
 * media URL, or read browser/session data. Classification is deliberately not
 * proof that a recording is accessible; it only says that a page URL matches a
 * narrowly recognized recording route on an exact platform-domain boundary.
 */

export type RecordingPlatformId = "youtube" | "webex" | "zoom" | "generic";

export type RecordingPageKind =
  | "youtube-watch"
  | "youtube-embed"
  | "youtube-shorts"
  | "youtube-short-link"
  | "webex-recording"
  | "zoom-recording"
  | "generic";

export type CaptionAvailabilityHint = "youtube-existing-captions" | "platform-captions-unknown" | "generic-html5";

/**
 * Safe metadata suitable for a MediaSnapshot or background job record. It has
 * no page URL, media URL, cookie, token, transcript, or other session data.
 */
export interface RecordingPlatformMetadata {
  readonly id: RecordingPlatformId;
  readonly displayName: string;
  readonly pageKind: RecordingPageKind;
  /** True only for a narrowly recognized recording/video route. */
  readonly knownRecordingPath: boolean;
  /** A UI/workflow hint, never a promise that captions or media are accessible. */
  readonly captionAvailability: CaptionAvailabilityHint;
}

export interface RecordingPlatformClassification extends RecordingPlatformMetadata {
  /** Ready-to-display explanation for an opaque source on the matched platform. */
  readonly opaqueSourceGuidance: string;
}

const GENERIC_GUIDANCE =
  "Subtitler can process only media the current page makes available to your browser. It will not copy credentials. It will not bypass DRM, authentication, encryption, or other access protections.";

const PLATFORM_GUIDANCE: Readonly<Record<Exclude<RecordingPlatformId, "generic">, string>> = {
  youtube:
    "Subtitler may use existing YouTube captions or media the current page makes accessible in this browser. It will not copy credentials. It will not bypass DRM, login, or other access protections.",
  webex:
    "Subtitler can use only recording media your current Webex page makes accessible in this browser. It will not copy credentials. It will not bypass recording protections, authentication, encryption, or access controls.",
  zoom:
    "Subtitler can use only recording media your current Zoom page makes accessible in this browser. It will not copy credentials. It will not bypass recording protections, authentication, encryption, or access controls."
};

/**
 * Classify a page URL without retaining it. Invalid, non-HTTPS, credentialed,
 * lookalike, or unrecognized URLs deliberately fall back to `generic`.
 */
export function classifyRecordingPageUrl(input: string | URL): RecordingPlatformClassification {
  const url = parseSafePageUrl(input);
  if (!url) {
    return genericClassification();
  }

  const youtubeKind = classifyYoutubePath(url);
  if (youtubeKind) {
    return knownClassification("youtube", "YouTube", youtubeKind, "youtube-existing-captions");
  }
  if (isWebexRecordingPath(url)) {
    return knownClassification("webex", "Webex", "webex-recording", "platform-captions-unknown");
  }
  if (isZoomRecordingPath(url)) {
    return knownClassification("zoom", "Zoom", "zoom-recording", "platform-captions-unknown");
  }
  return genericClassification();
}

/** User-safe opaque-source text for UI code that already knows the platform. */
export function opaqueSourceGuidanceFor(platform: RecordingPlatformId): string {
  return platform === "generic" ? GENERIC_GUIDANCE : PLATFORM_GUIDANCE[platform];
}

/**
 * Returns safe metadata only. This is useful when a caller wants a stable
 * platform label without carrying the original page URL into job state.
 */
export function recordingPlatformMetadataFor(platform: RecordingPlatformId): Omit<RecordingPlatformMetadata, "pageKind" | "knownRecordingPath"> {
  switch (platform) {
    case "youtube":
      return { id: "youtube", displayName: "YouTube", captionAvailability: "youtube-existing-captions" };
    case "webex":
      return { id: "webex", displayName: "Webex", captionAvailability: "platform-captions-unknown" };
    case "zoom":
      return { id: "zoom", displayName: "Zoom", captionAvailability: "platform-captions-unknown" };
    case "generic":
      return { id: "generic", displayName: "This page", captionAvailability: "generic-html5" };
  }
}

function genericClassification(): RecordingPlatformClassification {
  return {
    ...recordingPlatformMetadataFor("generic"),
    pageKind: "generic",
    knownRecordingPath: false,
    opaqueSourceGuidance: GENERIC_GUIDANCE
  };
}

function knownClassification(
  id: Exclude<RecordingPlatformId, "generic">,
  displayName: string,
  pageKind: Exclude<RecordingPageKind, "generic">,
  captionAvailability: Exclude<CaptionAvailabilityHint, "generic-html5">
): RecordingPlatformClassification {
  return {
    id,
    displayName,
    pageKind,
    knownRecordingPath: true,
    captionAvailability,
    opaqueSourceGuidance: PLATFORM_GUIDANCE[id]
  };
}

function parseSafePageUrl(input: string | URL): URL | undefined {
  try {
    const url = new URL(typeof input === "string" ? input : input.href);
    if (url.protocol !== "https:" || url.username || url.password || (url.port !== "" && url.port !== "443")) {
      return undefined;
    }
    return url;
  } catch {
    return undefined;
  }
}

function classifyYoutubePath(url: URL): Extract<RecordingPageKind, `youtube-${string}`> | undefined {
  const hostname = url.hostname.toLowerCase();
  if (hostname === "youtu.be") {
    return hasExactlyOneIdentifier(url.pathname) ? "youtube-short-link" : undefined;
  }
  if (!isDomainOrSubdomain(hostname, "youtube.com")) {
    return undefined;
  }
  if (url.pathname === "/watch" && hasIdentifier(url.searchParams.get("v"))) {
    return "youtube-watch";
  }
  if (hasNamedRecordingPath(url.pathname, "embed")) {
    return "youtube-embed";
  }
  return hasNamedRecordingPath(url.pathname, "shorts") ? "youtube-shorts" : undefined;
}

function isWebexRecordingPath(url: URL): boolean {
  if (!isDomainOrSubdomain(url.hostname.toLowerCase(), "webex.com")) {
    return false;
  }
  const segments = pathSegments(url.pathname);
  if (segments[0]?.toLowerCase() === "recordingservice") {
    return hasWebexRecordingIdentifier(segments);
  }
  return isWebexWebAppPlaybackRoute(segments);
}

/**
 * Current Webex playback pages commonly use this ordinary, site-scoped route:
 * `/webappng/sites/<site>/recording/<recording-id>/playback`.
 *
 * This is classification only. It never treats the page route as a media URL,
 * discovers a representation, or changes the no-credential-transfer policy.
 */
function isWebexWebAppPlaybackRoute(segments: readonly string[]): boolean {
  return (
    segments.length === 6 &&
    segments[0]?.toLowerCase() === "webappng" &&
    segments[1]?.toLowerCase() === "sites" &&
    hasIdentifier(segments[2]) &&
    segments[3]?.toLowerCase() === "recording" &&
    hasIdentifier(segments[4]) &&
    segments[5]?.toLowerCase() === "playback"
  );
}

function isZoomRecordingPath(url: URL): boolean {
  if (!isDomainOrSubdomain(url.hostname.toLowerCase(), "zoom.us")) {
    return false;
  }
  const segments = pathSegments(url.pathname);
  const root = segments[0]?.toLowerCase();
  const action = segments[1]?.toLowerCase();
  return (root === "rec" || root === "recording") && (action === "play" || action === "share") && hasIdentifier(segments[2]);
}

function isDomainOrSubdomain(hostname: string, domain: string): boolean {
  return hostname === domain || hostname.endsWith(`.${domain}`);
}

function pathSegments(pathname: string): string[] {
  return pathname.split("/").filter(Boolean);
}

function hasExactlyOneIdentifier(pathname: string): boolean {
  const segments = pathSegments(pathname);
  return segments.length === 1 && hasIdentifier(segments[0]);
}

function hasNamedRecordingPath(pathname: string, name: "embed" | "shorts"): boolean {
  const segments = pathSegments(pathname);
  return segments.length === 2 && segments[0] === name && hasIdentifier(segments[1]);
}

function hasWebexRecordingIdentifier(segments: readonly string[]): boolean {
  for (let index = 0; index < segments.length - 1; index += 1) {
    const name = segments[index]?.toLowerCase();
    const identifier = segments[index + 1];
    if (name === "playback" && hasIdentifier(identifier)) {
      return true;
    }
    if (name === "recording" && identifier?.toLowerCase() !== "playback" && hasIdentifier(identifier)) {
      return true;
    }
  }
  return false;
}

function hasIdentifier(value: string | null | undefined): boolean {
  return typeof value === "string" && /^[A-Za-z0-9._~=-]{1,512}$/u.test(value);
}
