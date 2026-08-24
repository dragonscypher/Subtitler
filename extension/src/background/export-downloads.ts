import {
  MAX_TRANSCRIPT_EXPORT_BYTES,
  OFFSCREEN_EXPORT_DOCUMENT_PATH,
  type ExportMimeType,
  type OffscreenCreateExportBlobRequest,
  type OffscreenExportRequest,
  type OffscreenRevokeExportBlobRequest,
  isExportMimeType,
  isOffscreenExportResponse,
  isSafeExtensionBlobUrl
} from "../shared/export-download-protocol";

const MAX_EARLY_TERMINAL_DOWNLOAD_IDS = 8;

export interface BrowserTranscriptDownload {
  content: string;
  mimeType: ExportMimeType;
  filename: "Transcript.txt" | "Transcript-timestamped.txt" | "Subtitles.srt" | "Subtitles.vtt" | "Transcript.json";
}

export type BrowserTranscriptDownloadResult =
  | { ok: true; downloadId: number }
  | { ok: false; message: string };

export interface DownloadChange {
  id: number;
  state?: "complete" | "interrupted" | "in_progress";
}

/** Narrow adapter makes the MV3 lifecycle deterministic and mockable in tests. */
export interface ExportDownloadApi {
  getExtensionUrl(path: string): string;
  getOffscreenContextCount(documentUrl: string): Promise<number>;
  createOffscreenDocument(path: string): Promise<void>;
  closeOffscreenDocument(): Promise<void>;
  sendToOffscreen(message: OffscreenExportRequest): Promise<unknown>;
  startDownload(options: { url: string; filename: BrowserTranscriptDownload["filename"]; saveAs: true; conflictAction: "uniquify" }): Promise<number>;
  onDownloadChanged(listener: (change: DownloadChange) => void): void;
  createRequestId(): string;
}

interface ActiveBlobDownload {
  requestId: string;
  downloadId: number;
}

/**
 * Coordinates one explicit browser export at a time. The service worker never
 * creates a Blob URL; the offscreen document owns that URL and revokes it only
 * after a terminal `downloads` event (or its independent short safety TTL).
 */
export class ExportDownloadCoordinator {
  private creation: Promise<void> | undefined;
  private active: ActiveBlobDownload | undefined;
  private starting = false;
  private releasing = false;
  /** Bounded only for the tiny start-call/event ordering window. */
  private readonly earlyTerminalDownloadIds = new Set<number>();

  constructor(private readonly api: ExportDownloadApi) {
    this.api.onDownloadChanged((change) => {
      if (change.state !== "complete" && change.state !== "interrupted") {
        return;
      }
      if (this.active?.downloadId === change.id) {
        void this.releaseActiveBlob(this.active);
      } else if (this.starting) {
        this.rememberEarlyTerminalDownload(change.id);
      }
    });
  }

  async start(download: BrowserTranscriptDownload): Promise<BrowserTranscriptDownloadResult> {
    if (!isSafeDownloadInput(download)) {
      return { ok: false, message: "Subtitler could not prepare this browser download." };
    }
    if (this.active || this.starting || this.releasing) {
      return {
        ok: false,
        message: "Another transcript export is still awaiting its save result. Finish or cancel it before starting another export."
      };
    }
    this.starting = true;
    let requestId: string | undefined;
    let createdOffscreenDocument = false;
    try {
      createdOffscreenDocument = await this.ensureOffscreenDocument();
      requestId = this.api.createRequestId();
      const blobResponse = await this.api.sendToOffscreen({
        target: "subtitler-offscreen",
        type: "offscreen.create-export-blob",
        requestId,
        content: download.content,
        mimeType: download.mimeType
      } satisfies OffscreenCreateExportBlobRequest);
      if (!isOffscreenExportResponse(blobResponse) || blobResponse.requestId !== requestId) {
        throw new Error("invalid offscreen response");
      }
      if (blobResponse.type !== "offscreen.export-blob-ready") {
        // An earlier service-worker lifetime may still own an offscreen Blob.
        // Leave it to its independent TTL rather than closing a live download.
        if (createdOffscreenDocument) {
          await this.closeOffscreenDocument();
        }
        return { ok: false, message: "Subtitler could not prepare this transcript download." };
      }
      const extensionBaseUrl = this.api.getExtensionUrl("");
      if (!isSafeExtensionBlobUrl(blobResponse.blobUrl, extensionBaseUrl)) {
        throw new Error("invalid offscreen Blob URL");
      }

      let downloadId: number;
      try {
        downloadId = await this.api.startDownload({
          url: blobResponse.blobUrl,
          filename: download.filename,
          saveAs: true,
          conflictAction: "uniquify"
        });
      } catch {
        await this.revokeBlob(requestId);
        await this.closeOffscreenDocument();
        return { ok: false, message: "Subtitler could not open the save dialog. Try the export again." };
      }
      if (!Number.isSafeInteger(downloadId) || downloadId < 0) {
        await this.revokeBlob(requestId);
        await this.closeOffscreenDocument();
        return { ok: false, message: "Subtitler could not start this browser download." };
      }
      this.active = { requestId, downloadId };
      // A terminal change can arrive before chrome.downloads.download resolves.
      // Consume only the matching bounded marker after assigning ownership so
      // the Blob cannot remain locked until its TTL.
      if (this.earlyTerminalDownloadIds.delete(downloadId)) {
        await this.releaseActiveBlob(this.active);
      }
      return { ok: true, downloadId };
    } catch {
      if (requestId) {
        await this.revokeBlob(requestId);
      }
      // Do not close a document discovered from an earlier service-worker
      // lifetime: it can still be keeping that lifetime's download alive.
      if (createdOffscreenDocument) {
        await this.closeOffscreenDocument();
      }
      return { ok: false, message: "Subtitler could not prepare this browser download." };
    } finally {
      this.starting = false;
      this.earlyTerminalDownloadIds.clear();
    }
  }

  /** Receives only an opaque request ID when the offscreen TTL releases a Blob. */
  handleOffscreenBlobExpired(requestId: string): void {
    if (this.active?.requestId === requestId) {
      this.active = undefined;
      void this.closeExpiredOffscreenDocument();
      return;
    }
    // A service worker can restart after it hands a download to Chrome. The
    // TTL event then proves there is no longer a Blob to retain; close only if
    // this worker does not currently own a different export.
    if (!this.active && !this.starting) {
      void this.closeOffscreenDocument();
    }
  }

  /** Returns true only when this service-worker invocation created the document. */
  private async ensureOffscreenDocument(): Promise<boolean> {
    const documentUrl = this.api.getExtensionUrl(OFFSCREEN_EXPORT_DOCUMENT_PATH);
    const existingContexts = await this.api.getOffscreenContextCount(documentUrl);
    if (existingContexts > 0) {
      return false;
    }
    if (!this.creation) {
      this.creation = this.api.createOffscreenDocument(OFFSCREEN_EXPORT_DOCUMENT_PATH).finally(() => {
        this.creation = undefined;
      });
    }
    await this.creation;
    return true;
  }

  private async releaseActiveBlob(active: ActiveBlobDownload): Promise<void> {
    if (this.active?.downloadId !== active.downloadId || this.active.requestId !== active.requestId) {
      return;
    }
    this.active = undefined;
    this.releasing = true;
    try {
      await this.revokeBlob(active.requestId);
      await this.closeOffscreenDocument();
    } finally {
      this.releasing = false;
    }
  }

  private async closeExpiredOffscreenDocument(): Promise<void> {
    this.releasing = true;
    try {
      await this.closeOffscreenDocument();
    } finally {
      this.releasing = false;
    }
  }

  private rememberEarlyTerminalDownload(downloadId: number): void {
    if (!Number.isSafeInteger(downloadId) || downloadId < 0 || this.earlyTerminalDownloadIds.has(downloadId)) {
      return;
    }
    if (this.earlyTerminalDownloadIds.size >= MAX_EARLY_TERMINAL_DOWNLOAD_IDS) {
      const oldest = this.earlyTerminalDownloadIds.values().next().value;
      if (typeof oldest === "number") {
        this.earlyTerminalDownloadIds.delete(oldest);
      }
    }
    this.earlyTerminalDownloadIds.add(downloadId);
  }

  private async revokeBlob(requestId: string): Promise<void> {
    try {
      const response = await this.api.sendToOffscreen({
        target: "subtitler-offscreen",
        type: "offscreen.revoke-export-blob",
        requestId
      } satisfies OffscreenRevokeExportBlobRequest);
      // A closed/offscreen-restarted document can legitimately report nothing.
      // Do not surface or log a transcript-specific error while cleaning up.
      if (!isOffscreenExportResponse(response) || response.requestId !== requestId) {
        return;
      }
    } catch {
      // The offscreen TTL remains an independent cleanup backstop.
    }
  }

  private async closeOffscreenDocument(): Promise<void> {
    try {
      await this.api.closeOffscreenDocument();
    } catch {
      // The document may have closed itself/been discarded. There is no
      // transcript payload in this error path.
    }
  }
}

/**
 * Chrome 120+ MV3 implementation. It deliberately uses `runtime` messaging
 * for the offscreen document because that is the only extension API exposed
 * in that context; `downloads` remains in the service worker.
 */
export function createChromeExportDownloadApi(): ExportDownloadApi | undefined {
  if (
    typeof chrome === "undefined" ||
    !chrome.runtime?.getContexts ||
    !chrome.runtime?.sendMessage ||
    !chrome.offscreen?.createDocument ||
    !chrome.offscreen?.closeDocument ||
    !chrome.downloads?.download ||
    !chrome.downloads?.onChanged
  ) {
    return undefined;
  }
  return {
    getExtensionUrl: (path) => chrome.runtime.getURL(path),
    async getOffscreenContextCount(documentUrl) {
      const contexts = await chrome.runtime.getContexts({
        contextTypes: ["OFFSCREEN_DOCUMENT"],
        documentUrls: [documentUrl]
      });
      return contexts.length;
    },
    createOffscreenDocument: (path) =>
      chrome.offscreen.createDocument({
        url: path,
        reasons: ["BLOBS"],
        justification: "Create a temporary Blob URL for an explicitly requested transcript download."
      }),
    closeOffscreenDocument: () => chrome.offscreen.closeDocument(),
    sendToOffscreen: (message) => chrome.runtime.sendMessage(message),
    startDownload: (options) => chrome.downloads.download(options),
    onDownloadChanged(listener) {
      chrome.downloads.onChanged.addListener((delta) => {
        const current = delta.state?.current;
        if (current === "complete" || current === "interrupted" || current === "in_progress") {
          listener({ id: delta.id, state: current });
        } else {
          listener({ id: delta.id });
        }
      });
    },
    createRequestId: () => crypto.randomUUID()
  };
}

function isSafeDownloadInput(value: BrowserTranscriptDownload): boolean {
  return (
    typeof value.content === "string" &&
    value.content.length <= MAX_TRANSCRIPT_EXPORT_BYTES &&
    new TextEncoder().encode(value.content).byteLength <= MAX_TRANSCRIPT_EXPORT_BYTES &&
    isExportMimeType(value.mimeType) &&
    isFixedFilename(value.filename)
  );
}

function isFixedFilename(value: string): value is BrowserTranscriptDownload["filename"] {
  return (
    value === "Transcript.txt" ||
    value === "Transcript-timestamped.txt" ||
    value === "Subtitles.srt" ||
    value === "Subtitles.vtt" ||
    value === "Transcript.json"
  );
}
