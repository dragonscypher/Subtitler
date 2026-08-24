import { describe, expect, it } from "vitest";
import {
  isOffscreenExportRequest,
  isSafeExtensionBlobUrl
} from "../src/shared/export-download-protocol";

const requestId = "11111111-1111-4111-8111-111111111111";

describe("offscreen export protocol", () => {
  it("admits only bounded create/revoke requests with fixed MIME types", () => {
    expect(
      isOffscreenExportRequest({
        target: "subtitler-offscreen",
        type: "offscreen.create-export-blob",
        requestId,
        content: "Private transcript text",
        mimeType: "text/plain;charset=utf-8"
      })
    ).toBe(true);
    expect(
      isOffscreenExportRequest({
        target: "subtitler-offscreen",
        type: "offscreen.create-export-blob",
        requestId,
        content: "Private transcript text",
        mimeType: "text/html"
      })
    ).toBe(false);
    expect(
      isOffscreenExportRequest({
        target: "subtitler-offscreen",
        type: "offscreen.revoke-export-blob",
        requestId: "not-an-export-request-id"
      })
    ).toBe(false);
  });

  it("accepts only a Blob URL owned by the current extension origin", () => {
    const extensionBase = "chrome-extension://subtitler-test-id/";
    expect(isSafeExtensionBlobUrl("blob:chrome-extension://subtitler-test-id/opaque-blob-id", extensionBase)).toBe(true);
    expect(isSafeExtensionBlobUrl("blob:chrome-extension://another-extension/opaque-blob-id", extensionBase)).toBe(false);
    expect(isSafeExtensionBlobUrl("blob:chrome-extension://subtitler-test-id/opaque-blob-id?x=1", extensionBase)).toBe(false);
    expect(isSafeExtensionBlobUrl("blob:https://example.test/opaque-blob-id", extensionBase)).toBe(false);
  });
});
