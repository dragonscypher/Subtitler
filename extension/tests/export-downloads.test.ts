import { describe, expect, it } from "vitest";
import {
  ExportDownloadCoordinator,
  type DownloadChange,
  type ExportDownloadApi
} from "../src/background/export-downloads";
import type { OffscreenExportRequest } from "../src/shared/export-download-protocol";

const extensionBaseUrl = "chrome-extension://subtitler-test-id/";
const validDownload = {
  content: "A private transcript.",
  mimeType: "text/plain;charset=utf-8" as const,
  filename: "Transcript.txt" as const
};

interface MockHarness {
  api: ExportDownloadApi;
  sent: OffscreenExportRequest[];
  started: Array<{ url: string; filename: string; saveAs: true; conflictAction: "uniquify" }>;
  emit(change: DownloadChange): void;
  setBlobUrl(value: string): void;
  setStartDownload(implementation: (options: { url: string; filename: string; saveAs: true; conflictAction: "uniquify" }) => Promise<number>): void;
  readonly created: number;
  readonly closed: number;
}

function createHarness(): MockHarness {
  let contexts = 0;
  let created = 0;
  let closed = 0;
  let requestCounter = 0;
  let listener: ((change: DownloadChange) => void) | undefined;
  let blobUrl = `${"blob:"}${extensionBaseUrl}opaque-blob-id`;
  let startDownload: (options: {
    url: string;
    filename: string;
    saveAs: true;
    conflictAction: "uniquify";
  }) => Promise<number> = async () => 77;
  const sent: OffscreenExportRequest[] = [];
  const started: Array<{ url: string; filename: string; saveAs: true; conflictAction: "uniquify" }> = [];

  return {
    api: {
      getExtensionUrl: (path) => `${extensionBaseUrl}${path}`,
      getOffscreenContextCount: async () => contexts,
      createOffscreenDocument: async () => {
        created += 1;
        contexts = 1;
      },
      closeOffscreenDocument: async () => {
        closed += 1;
        contexts = 0;
      },
      sendToOffscreen: async (message) => {
        sent.push(message);
        if (message.type === "offscreen.create-export-blob") {
          return { type: "offscreen.export-blob-ready", requestId: message.requestId, blobUrl };
        }
        return { type: "offscreen.export-blob-revoked", requestId: message.requestId };
      },
      startDownload: async (options) => {
        started.push(options);
        return startDownload(options);
      },
      onDownloadChanged: (registered) => {
        listener = registered;
      },
      createRequestId: () => {
        requestCounter += 1;
        return `11111111-1111-4111-8111-${requestCounter.toString(16).padStart(12, "0")}`;
      }
    },
    sent,
    started,
    emit: (change) => listener?.(change),
    setBlobUrl: (value) => {
      blobUrl = value;
    },
    setStartDownload: (implementation) => {
      startDownload = implementation;
    },
    get created() {
      return created;
    },
    get closed() {
      return closed;
    }
  };
}

async function flushAsyncWork(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

describe("ExportDownloadCoordinator", () => {
  it("starts a saveAs download with a fixed name and revokes its Blob on terminal state", async () => {
    const harness = createHarness();
    const coordinator = new ExportDownloadCoordinator(harness.api);

    await expect(coordinator.start(validDownload)).resolves.toEqual({ ok: true, downloadId: 77 });
    expect(harness.created).toBe(1);
    expect(harness.started).toEqual([
      {
        url: "blob:chrome-extension://subtitler-test-id/opaque-blob-id",
        filename: "Transcript.txt",
        saveAs: true,
        conflictAction: "uniquify"
      }
    ]);

    harness.emit({ id: 77, state: "complete" });
    await flushAsyncWork();
    expect(harness.sent.some((message) => message.type === "offscreen.revoke-export-blob")).toBe(true);
    expect(harness.closed).toBe(1);
  });

  it("does not leave a stale export lock when terminal state arrives before download() resolves", async () => {
    const harness = createHarness();
    const coordinator = new ExportDownloadCoordinator(harness.api);
    harness.setStartDownload(async () => {
      harness.emit({ id: 77, state: "interrupted" });
      return 77;
    });

    await expect(coordinator.start(validDownload)).resolves.toEqual({ ok: true, downloadId: 77 });
    expect(harness.closed).toBe(1);
    await expect(coordinator.start(validDownload)).resolves.toEqual({ ok: true, downloadId: 77 });
  });

  it("allows only one active export and refuses an offscreen Blob from another origin", async () => {
    const harness = createHarness();
    const coordinator = new ExportDownloadCoordinator(harness.api);

    await expect(coordinator.start(validDownload)).resolves.toEqual({ ok: true, downloadId: 77 });
    await expect(coordinator.start(validDownload)).resolves.toMatchObject({ ok: false });
    expect(harness.started).toHaveLength(1);

    const unsafeHarness = createHarness();
    unsafeHarness.setBlobUrl("blob:chrome-extension://other-extension/opaque-blob-id");
    const unsafeCoordinator = new ExportDownloadCoordinator(unsafeHarness.api);
    await expect(unsafeCoordinator.start(validDownload)).resolves.toEqual({
      ok: false,
      message: "Subtitler could not prepare this browser download."
    });
    expect(unsafeHarness.started).toHaveLength(0);
    expect(unsafeHarness.sent.some((message) => message.type === "offscreen.revoke-export-blob")).toBe(true);
    expect(unsafeHarness.closed).toBe(1);
  });

  it("releases the active state when the offscreen safety TTL reports only its opaque request ID", async () => {
    const harness = createHarness();
    const coordinator = new ExportDownloadCoordinator(harness.api);
    await coordinator.start(validDownload);
    const createRequest = harness.sent.find(
      (message): message is Extract<OffscreenExportRequest, { type: "offscreen.create-export-blob" }> =>
        message.type === "offscreen.create-export-blob"
    );
    if (!createRequest) throw new Error("Expected create request.");

    coordinator.handleOffscreenBlobExpired(createRequest.requestId);
    await flushAsyncWork();
    expect(harness.closed).toBe(1);
    await expect(coordinator.start(validDownload)).resolves.toEqual({ ok: true, downloadId: 77 });
  });
});
