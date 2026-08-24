import {
  OFFSCREEN_EXPORT_BLOB_TTL_MS,
  type OffscreenExportBlobExpiredEvent,
  type OffscreenExportResponse,
  isOffscreenExportRequest
} from "../shared/export-download-protocol";

interface BlobEntry {
  url: string;
  timer: ReturnType<typeof setTimeout>;
}

/**
 * The only document with DOM Blob-URL APIs. It has no access to downloads,
 * storage, native messaging, or page media. Each Blob is revoked on the
 * service worker's terminal download event, with a ten-minute independent TTL
 * as a service-worker-restart backstop.
 */
const blobs = new Map<string, BlobEntry>();

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (sender.id !== chrome.runtime.id || !isOffscreenExportRequest(message)) {
    return false;
  }
  if (message.type === "offscreen.create-export-blob") {
    void createBlob(message.requestId, message.content, message.mimeType).then(sendResponse);
    return true;
  }
  sendResponse(revokeBlob(message.requestId));
  return false;
});

async function createBlob(
  requestId: string,
  content: string,
  mimeType: string
): Promise<OffscreenExportResponse> {
  if (blobs.size > 0) {
    return { type: "offscreen.export-blob-error", requestId, message: "Another export is still active." };
  }
  try {
    const url = URL.createObjectURL(new Blob([content], { type: mimeType }));
    const timer = setTimeout(() => expireBlob(requestId), OFFSCREEN_EXPORT_BLOB_TTL_MS);
    blobs.set(requestId, { url, timer });
    return { type: "offscreen.export-blob-ready", requestId, blobUrl: url };
  } catch {
    return { type: "offscreen.export-blob-error", requestId, message: "Subtitler could not create a temporary download." };
  }
}

function revokeBlob(requestId: string): OffscreenExportResponse {
  const entry = blobs.get(requestId);
  if (entry) {
    clearTimeout(entry.timer);
    URL.revokeObjectURL(entry.url);
    blobs.delete(requestId);
  }
  return { type: "offscreen.export-blob-revoked", requestId };
}

function expireBlob(requestId: string): void {
  if (!blobs.has(requestId)) {
    return;
  }
  revokeBlob(requestId);
  const event: OffscreenExportBlobExpiredEvent = {
    target: "subtitler-background",
    type: "offscreen.export-blob-expired",
    requestId
  };
  try {
    chrome.runtime.sendMessage(event, () => {
      void chrome.runtime.lastError;
    });
  } catch {
    // The Blob has already been revoked; no payload survives this error path.
  }
}
