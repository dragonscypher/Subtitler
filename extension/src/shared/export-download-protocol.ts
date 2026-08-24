/**
 * Shared, content-bound messages for the MV3 offscreen Blob bridge. The only
 * message carrying transcript text is sent from the background to the bundled
 * offscreen document after an explicit popup export action.
 */
export const MAX_TRANSCRIPT_EXPORT_BYTES = 16 * 1024 * 1024;
export const OFFSCREEN_EXPORT_DOCUMENT_PATH = "offscreen.html";
export const OFFSCREEN_EXPORT_BLOB_TTL_MS = 10 * 60 * 1_000;

export type ExportMimeType =
  | "text/plain;charset=utf-8"
  | "application/x-subrip;charset=utf-8"
  | "text/vtt;charset=utf-8"
  | "application/json;charset=utf-8";

export interface OffscreenCreateExportBlobRequest {
  target: "subtitler-offscreen";
  type: "offscreen.create-export-blob";
  requestId: string;
  content: string;
  mimeType: ExportMimeType;
}

export interface OffscreenRevokeExportBlobRequest {
  target: "subtitler-offscreen";
  type: "offscreen.revoke-export-blob";
  requestId: string;
}

export type OffscreenExportRequest = OffscreenCreateExportBlobRequest | OffscreenRevokeExportBlobRequest;

export type OffscreenExportResponse =
  | { type: "offscreen.export-blob-ready"; requestId: string; blobUrl: string }
  | { type: "offscreen.export-blob-revoked"; requestId: string }
  | { type: "offscreen.export-blob-error"; requestId: string; message: string };

/** Sent without transcript text when the offscreen document's safety TTL fires. */
export interface OffscreenExportBlobExpiredEvent {
  target: "subtitler-background";
  type: "offscreen.export-blob-expired";
  requestId: string;
}

export function isOffscreenExportRequest(value: unknown): value is OffscreenExportRequest {
  if (!isRecord(value) || value.target !== "subtitler-offscreen" || !isExportRequestId(value.requestId)) {
    return false;
  }
  if (value.type === "offscreen.revoke-export-blob") {
    return true;
  }
  return (
    value.type === "offscreen.create-export-blob" &&
    typeof value.content === "string" &&
    value.content.length <= MAX_TRANSCRIPT_EXPORT_BYTES &&
    utf8ByteLength(value.content) <= MAX_TRANSCRIPT_EXPORT_BYTES &&
    isExportMimeType(value.mimeType)
  );
}

export function isOffscreenExportResponse(value: unknown): value is OffscreenExportResponse {
  if (!isRecord(value) || !isExportRequestId(value.requestId) || typeof value.type !== "string") {
    return false;
  }
  switch (value.type) {
    case "offscreen.export-blob-ready":
      return typeof value.blobUrl === "string" && value.blobUrl.length > 0 && value.blobUrl.length <= 1_024;
    case "offscreen.export-blob-revoked":
      return true;
    case "offscreen.export-blob-error":
      return typeof value.message === "string" && value.message.length > 0 && value.message.length <= 512;
    default:
      return false;
  }
}

export function isOffscreenExportBlobExpiredEvent(value: unknown): value is OffscreenExportBlobExpiredEvent {
  return (
    isRecord(value) &&
    value.target === "subtitler-background" &&
    value.type === "offscreen.export-blob-expired" &&
    isExportRequestId(value.requestId)
  );
}

export function isExportMimeType(value: unknown): value is ExportMimeType {
  return (
    value === "text/plain;charset=utf-8" ||
    value === "application/x-subrip;charset=utf-8" ||
    value === "text/vtt;charset=utf-8" ||
    value === "application/json;charset=utf-8"
  );
}

export function isSafeExtensionBlobUrl(value: string, extensionBaseUrl: string): boolean {
  if (
    value.length > 1_024 ||
    /[\x00-\x1f\x7f]/.test(value) ||
    !value.startsWith("blob:") ||
    value.includes("?") ||
    value.includes("#")
  ) {
    return false;
  }
  try {
    const inner = new URL(value.slice("blob:".length));
    const extension = new URL(extensionBaseUrl);
    return (
      extension.protocol === "chrome-extension:" &&
      extension.pathname === "/" &&
      inner.protocol === "chrome-extension:" &&
      inner.host === extension.host &&
      inner.origin === extension.origin &&
      inner.pathname.length > extension.pathname.length &&
      inner.pathname.startsWith(extension.pathname) &&
      inner.search === "" &&
      inner.hash === ""
    );
  } catch {
    return false;
  }
}

function isExportRequestId(value: unknown): value is string {
  return typeof value === "string" && /^[0-9a-f-]{16,128}$/i.test(value);
}

function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
