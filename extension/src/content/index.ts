import { ContentController } from "./controller";

declare global {
  interface Window {
    __subtitlerContentController?: ContentController;
  }
}

if (!window.__subtitlerContentController) {
  const controller = new ContentController();
  window.__subtitlerContentController = controller;

  chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
    try {
      sendResponse(controller.handleMessage(message));
    } catch {
      sendResponse({
        ok: false,
        error: {
          code: "PAGE_UNAVAILABLE",
          message: "Subtitler could not communicate with the media on this page."
        }
      });
    }
    return true;
  });
}
