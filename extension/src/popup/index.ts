import type { JobKind, JobRecord, MediaDetectionResult, Result, TranscriptSegment } from "../shared/domain";
import {
  parseEngineConnectionStateUpdate,
  type EngineConnectionState,
  type PopupExportStarted,
  type PopupRequest,
  type PopupResponse,
  type PopupTranscriptPage,
  type TranscriptExportFormat
} from "../shared/protocol";
import { describeLocalPerformance } from "./local-performance";

interface TranscriptViewState {
  jobId: string;
  segments: TranscriptSegment[];
  nextCursor?: number;
  loading: boolean;
}

interface PopupState {
  detection: MediaDetectionResult | undefined;
  jobs: JobRecord[];
  engine: EngineConnectionState;
  status: string;
  busy: boolean;
  exportFormat: TranscriptExportFormat;
  /** Page-memory only; it is discarded when the popup closes or the user closes the view. */
  transcript?: TranscriptViewState;
}

const appElement = document.querySelector<HTMLElement>("#app");
if (!appElement) {
  throw new Error("Subtitler popup root is missing.");
}
const app: HTMLElement = appElement;
const ACTIVE_JOB_REFRESH_INTERVAL_MS = 1_000;

const state: PopupState = {
  detection: undefined,
  jobs: [],
  engine: { connected: false, localProcessingAvailable: false },
  status: "Checking this page…",
  busy: false,
  exportFormat: "txt"
};
let activeJobRefreshTimer: number | undefined;
let activeJobRefreshInFlight = false;

window.addEventListener("unload", () => {
  if (activeJobRefreshTimer !== undefined) {
    window.clearInterval(activeJobRefreshTimer);
    activeJobRefreshTimer = undefined;
  }
});

chrome.runtime.onMessage.addListener((message) => {
  const update = parseEngineConnectionStateUpdate(message);
  if (!update) {
    return;
  }
  state.engine = update.payload;
  render();
});

void refresh();

async function refresh(): Promise<void> {
  state.busy = true;
  render();
  const [detection, jobs, engine] = await Promise.all([
    sendMessage<Result<MediaDetectionResult>>({ type: "popup.detect-media" }),
    sendMessage<Result<JobRecord[]>>({ type: "popup.get-jobs" }),
    sendMessage<Result<EngineConnectionState>>({ type: "popup.get-engine-state" })
  ]);
  state.busy = false;
  if (detection.ok) {
    state.detection = detection.data;
    state.status = detection.data.state === "detected" ? "Ready" : "Paste an accessible recording URL to continue.";
  } else {
    state.status = detection.error.message;
  }
  if (jobs.ok) {
    state.jobs = jobs.data;
  }
  if (engine.ok) {
    state.engine = engine.data;
  }
  render();
  syncActiveJobRefresh();
}

/**
 * The popup is an ephemeral document and does not receive every service-worker
 * event. While it is open, refresh only inexpensive job/engine state for an
 * active job so its status tracks native acceptance and subtitle buffering.
 */
function syncActiveJobRefresh(): void {
  if (latestOpenJob(state.jobs)) {
    if (activeJobRefreshTimer === undefined) {
      activeJobRefreshTimer = window.setInterval(() => {
        void refreshActiveJobState();
      }, ACTIVE_JOB_REFRESH_INTERVAL_MS);
    }
    return;
  }
  if (activeJobRefreshTimer !== undefined) {
    window.clearInterval(activeJobRefreshTimer);
    activeJobRefreshTimer = undefined;
  }
}

async function refreshActiveJobState(): Promise<void> {
  if (activeJobRefreshInFlight) {
    return;
  }
  activeJobRefreshInFlight = true;
  try {
    const [jobs, engine] = await Promise.all([
      sendMessage<Result<JobRecord[]>>({ type: "popup.get-jobs" }),
      sendMessage<Result<EngineConnectionState>>({ type: "popup.get-engine-state" })
    ]);
    if (jobs.ok) {
      state.jobs = jobs.data;
    }
    if (engine.ok) {
      state.engine = engine.data;
    }
    render();
    syncActiveJobRefresh();
  } finally {
    activeJobRefreshInFlight = false;
  }
}

function render(): void {
  app.replaceChildren();
  const detected = state.detection?.state === "detected" ? state.detection.media : undefined;

  const header = document.createElement("header");
  const title = document.createElement("h1");
  title.textContent = "SUBTITLER";
  header.append(title);
  app.append(header);

  const mediaSummary = document.createElement("p");
  mediaSummary.className = "media-summary";
  if (detected) {
    mediaSummary.textContent = `${detected.mediaKind === "video" ? "Video" : "Audio"} detected · ${formatDuration(
      detected.durationSeconds
    )}`;
  } else {
    mediaSummary.textContent = "No compatible media detected";
  }
  app.append(mediaSummary);

  if (state.engine.connected && state.engine.localProcessingAdvisory) {
    const localPerformance = document.createElement("p");
    localPerformance.className = "local-performance-note";
    localPerformance.textContent = describeLocalPerformance(
      state.engine.localProcessingAdvisory,
      state.engine.localProcessingAvailable
    );
    app.append(localPerformance);
  }

  if (detected?.captionTracks.length) {
    const captionNote = document.createElement("p");
    captionNote.className = "caption-note";
    captionNote.textContent = "Existing captions are available as an optional fast path.";
    app.append(captionNote);
  }

  if (!detected) {
    const input = document.createElement("input");
    input.id = "recording-url";
    input.type = "url";
    input.inputMode = "url";
    input.placeholder = "Paste recording URL";
    input.autocomplete = "off";
    input.setAttribute("aria-label", "Recording URL");
    app.append(input);
  }

  const actions = document.createElement("div");
  actions.className = "actions";
  actions.append(
    actionButton("Create Subtitles", () => startJob("subtitle", true), state.busy),
    actionButton("Get Full Transcript", () => startJob("transcript"), state.busy)
  );
  app.append(actions);

  if (detected?.captionTracks.length) {
    const existingCaptions = actionButton("Use Existing Captions", () => startJob("subtitle"), state.busy);
    existingCaptions.classList.add("context-action");
    app.append(existingCaptions);
  }

  const divider = document.createElement("hr");
  app.append(divider);

  const status = document.createElement("section");
  status.className = "status";
  const statusLabel = document.createElement("span");
  statusLabel.textContent = "Status:";
  const statusValue = document.createElement("strong");
  const currentJob = latestOpenJob(state.jobs);
  const latestTerminalJob = latestTerminalJobForDisplay(state.jobs);
  const completedTranscript = latestCompletedTranscript(state.jobs);
  // A retained popup-local status (for example "Ready") must never conceal
  // an active native job. Showing the live job state makes buffering visible
  // instead of looking like a delayed or stalled action.
  statusValue.textContent = currentJob
    ? humanizeStatus(currentJob.status)
    : latestTerminalJob?.status === "completed"
      ? latestTerminalJob.kind === "subtitle"
        ? "Subtitles ready"
        : "Transcript ready"
      : latestTerminalJob?.error?.message ??
        (latestTerminalJob ? "Subtitler could not complete this job." : state.status);
  status.append(statusLabel, statusValue);
  if (currentJob) {
    const progress = document.createElement("small");
    progress.textContent = describeJob(currentJob);
    status.append(progress);
    status.append(actionButton("Stop", () => stopJob(currentJob.id), state.busy));
  }
  app.append(status);

  if (completedTranscript && state.transcript?.jobId !== completedTranscript.id) {
    const viewTranscript = actionButton("View Transcript", () => viewTranscriptForJob(completedTranscript.id), state.busy);
    viewTranscript.classList.add("context-action");
    app.append(viewTranscript);
  }

  if (state.transcript) {
    app.append(renderTranscript(state.transcript));
  }
}

function actionButton(label: string, onClick: () => void, disabled: boolean): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.textContent = label;
  button.disabled = disabled;
  button.addEventListener("click", onClick);
  return button;
}

async function startJob(jobKind: JobKind, forceGenerate = false): Promise<void> {
  const urlInput = document.querySelector<HTMLInputElement>("#recording-url");
  const pastedUrl = urlInput?.value.trim();
  if (!state.detection || state.detection.state === "none") {
    if (!pastedUrl) {
      state.status = "Paste an accessible recording URL first.";
      render();
      return;
    }
  }
  state.busy = true;
  state.status = jobKind === "subtitle" ? "Starting subtitles…" : "Starting transcript…";
  render();

  const request: Extract<PopupRequest, { type: "popup.start-job" }> = { type: "popup.start-job", jobKind };
  if (pastedUrl) {
    request.pastedUrl = pastedUrl;
  }
  if (forceGenerate) {
    request.forceGenerate = true;
  }
  const response = await sendMessage<Result<JobRecord>>(request);
  state.busy = false;
  if (response.ok) {
    state.status = response.data.usesExistingCaptions ? "Using existing captions." : "Subtitler is working in the background.";
    state.jobs = [response.data, ...state.jobs.filter((job) => job.id !== response.data.id)];
  } else {
    state.status = response.error.message;
  }
  render();
  syncActiveJobRefresh();
  if (response.ok) {
    void refreshActiveJobState();
  }
}

async function stopJob(jobId: string): Promise<void> {
  state.busy = true;
  state.status = "Stopping…";
  render();
  const response = await sendMessage<Result<JobRecord>>({ type: "popup.stop-job", jobId });
  state.busy = false;
  if (response.ok) {
    if (state.transcript?.jobId === jobId) {
      delete state.transcript;
    }
    state.status = "Stopped.";
    state.jobs = [response.data, ...state.jobs.filter((job) => job.id !== response.data.id)];
  } else {
    state.status = response.error.message;
  }
  render();
  syncActiveJobRefresh();
}

/** Explicit user action begins a bounded, transient transcript-view session. */
async function viewTranscriptForJob(jobId: string): Promise<void> {
  if (state.busy) {
    return;
  }
  const view: TranscriptViewState = { jobId, segments: [], loading: true };
  state.transcript = view;
  state.busy = true;
  state.status = "Loading transcript…";
  render();
  const loaded = await loadTranscriptPage(view, undefined);
  state.busy = false;
  if (!loaded && state.transcript === view) {
    delete state.transcript;
  }
  render();
}

/** Loads at most one background-owned transcript page at a time. */
async function loadTranscriptPage(view: TranscriptViewState, cursor: number | undefined): Promise<boolean> {
  const request: Extract<PopupRequest, { type: "popup.get-transcript" }> = {
    type: "popup.get-transcript",
    jobId: view.jobId,
    limit: 100
  };
  if (cursor !== undefined) {
    request.cursor = cursor;
  }
  const response = await sendMessage<Result<PopupTranscriptPage>>(request);
  if (state.transcript !== view) {
    return false;
  }
  view.loading = false;
  if (!response.ok) {
    state.status = response.error.message;
    return false;
  }
  view.segments.push(...response.data.segments);
  if (response.data.nextCursor !== undefined) {
    view.nextCursor = response.data.nextCursor;
  } else {
    delete view.nextCursor;
  }
  state.status = view.nextCursor === undefined ? "Transcript ready." : "Transcript ready — scroll to load more.";
  return true;
}

async function loadMoreTranscript(view: TranscriptViewState): Promise<void> {
  if (view.loading || view.nextCursor === undefined || state.transcript !== view) {
    return;
  }
  const cursor = view.nextCursor;
  view.loading = true;
  render();
  const loaded = await loadTranscriptPage(view, cursor);
  if (!loaded && state.transcript === view) {
    delete state.transcript;
  }
  render();
}

function renderTranscript(view: TranscriptViewState): HTMLElement {
  const section = document.createElement("section");
  section.className = "transcript";

  const heading = document.createElement("div");
  heading.className = "transcript-heading";
  const title = document.createElement("h2");
  title.textContent = "Transcript";
  const close = actionButton("Close", () => {
    if (state.transcript === view) {
      delete state.transcript;
      state.status = "Ready";
      render();
    }
  }, false);
  close.classList.add("transcript-close");
  heading.append(title, close);
  section.append(heading);

  // The background has drained both transcript and cue pages before a
  // completed job becomes viewable. This control still appears only after the
  // user explicitly opens that completed transcript; no export is automatic.
  if (!view.loading) {
    section.append(renderExportControls(view.jobId));
  }

  const scroll = document.createElement("div");
  scroll.className = "transcript-scroll";
  scroll.tabIndex = 0;
  scroll.setAttribute("aria-label", "Completed transcript");
  for (const segment of view.segments) {
    const item = document.createElement("p");
    item.className = "transcript-segment";
    const timing = document.createElement("time");
    timing.className = "transcript-timing";
    timing.textContent = formatTranscriptTimestamp(segment.startSeconds);
    item.append(timing);
    if (segment.speaker) {
      const speaker = document.createElement("strong");
      speaker.className = "transcript-speaker";
      speaker.textContent = `${segment.speaker}:`;
      item.append(speaker);
    }
    const text = document.createElement("span");
    text.className = "transcript-text";
    // Use a text node rather than HTML so transcript content cannot affect the
    // extension popup's DOM or execute page-provided markup.
    text.textContent = segment.text;
    item.append(text);
    scroll.append(item);
  }
  if (view.segments.length === 0 && view.loading) {
    const loading = document.createElement("p");
    loading.className = "transcript-note";
    loading.textContent = "Loading transcript…";
    scroll.append(loading);
  } else if (view.segments.length === 0) {
    const empty = document.createElement("p");
    empty.className = "transcript-note";
    empty.textContent = "No spoken transcript segments were returned.";
    scroll.append(empty);
  }
  if (view.loading && view.segments.length > 0) {
    const loading = document.createElement("p");
    loading.className = "transcript-note";
    loading.textContent = "Loading more…";
    scroll.append(loading);
  } else if (view.nextCursor !== undefined) {
    const more = document.createElement("p");
    more.className = "transcript-note";
    more.textContent = "Scroll to load more of the transcript.";
    scroll.append(more);
  }
  scroll.addEventListener("scroll", () => {
    const nearBottom = scroll.scrollTop + scroll.clientHeight >= scroll.scrollHeight - 80;
    if (nearBottom) {
      void loadMoreTranscript(view);
    }
  });
  section.append(scroll);
  // A short transcript page can be smaller than the viewport and therefore
  // cannot generate a scroll event. Continue only until the region becomes
  // scrollable (or the native result is exhausted), still one bounded page at
  // a time.
  if (!view.loading && view.nextCursor !== undefined) {
    queueMicrotask(() => {
      if (state.transcript === view && scroll.scrollHeight <= scroll.clientHeight + 8) {
        void loadMoreTranscript(view);
      }
    });
  }
  return section;
}

function renderExportControls(jobId: string): HTMLElement {
  const controls = document.createElement("div");
  controls.className = "export-controls";

  const label = document.createElement("label");
  label.htmlFor = "transcript-export-format";
  label.textContent = "Export";

  const select = document.createElement("select");
  select.id = "transcript-export-format";
  select.setAttribute("aria-label", "Transcript export format");
  const formats: Array<{ value: TranscriptExportFormat; label: string }> = [
    { value: "txt", label: "Text (.txt)" },
    { value: "timestamped_txt", label: "Timestamped text (.txt)" },
    { value: "srt", label: "SubRip (.srt)" },
    { value: "vtt", label: "WebVTT (.vtt)" },
    { value: "json", label: "JSON (.json)" }
  ];
  for (const format of formats) {
    const option = document.createElement("option");
    option.value = format.value;
    option.textContent = format.label;
    option.selected = format.value === state.exportFormat;
    select.append(option);
  }
  select.disabled = state.busy;
  select.addEventListener("change", () => {
    const selected = formats.find((format) => format.value === select.value);
    if (selected) {
      state.exportFormat = selected.value;
    }
  });

  const exportButton = actionButton("Export", () => exportTranscript(jobId), state.busy);
  exportButton.classList.add("export-button");
  controls.append(label, select, exportButton);
  return controls;
}

/** The popup click is the sole initiation point for a browser download. */
async function exportTranscript(jobId: string): Promise<void> {
  if (state.busy || state.transcript?.jobId !== jobId || state.transcript.loading) {
    return;
  }
  state.busy = true;
  state.status = "Preparing download…";
  render();
  const response = await sendMessage<Result<PopupExportStarted>>({
    type: "popup.export-transcript",
    jobId,
    format: state.exportFormat
  });
  state.busy = false;
  state.status = response.ok
    ? `Choose where to save ${response.data.filename}.`
    : response.error.message;
  render();
}

function sendMessage<T extends PopupResponse>(message: PopupRequest): Promise<T> {
  return new Promise<T>((resolve) => {
    chrome.runtime.sendMessage(message, (response: T) => {
      const error = chrome.runtime.lastError;
      if (error) {
        resolve({
          ok: false,
          error: { code: "UNKNOWN", message: "Subtitler could not reach its background service." }
        } as T);
        return;
      }
      resolve(response);
    });
  });
}

function latestOpenJob(jobs: readonly JobRecord[]): JobRecord | undefined {
  return jobs.find((job) => ["queued", "connecting", "processing", "buffering", "using-existing-captions", "recovering"].includes(job.status));
}

function latestCompletedTranscript(jobs: readonly JobRecord[]): JobRecord | undefined {
  return jobs.find((job) => job.kind === "transcript" && job.status === "completed");
}

/** Jobs arrive newest-first. Show that terminal result instead of a stale older failure. */
function latestTerminalJobForDisplay(jobs: readonly JobRecord[]): JobRecord | undefined {
  return jobs.find((job) => job.status === "completed" || job.status === "failed" || job.status === "stale");
}

function describeJob(job: JobRecord): string {
  const percent = job.progress?.percent;
  const buffer = job.progress?.subtitleBufferSeconds;
  const engineMessage = job.progress?.statusMessage;
  const progressText =
    typeof percent === "number"
      ? `${Math.round(percent)}% processed${typeof buffer === "number" ? ` · ${formatDuration(buffer)} ahead` : ""}`
      : job.status === "using-existing-captions"
        ? "Existing captions active"
        : humanizeStatus(job.status);
  return engineMessage ? `${engineMessage} · ${progressText}` : progressText;
}

function humanizeStatus(status: JobRecord["status"]): string {
  return status.replace(/-/g, " ").replace(/^./, (character) => character.toUpperCase());
}

function formatDuration(seconds: number | null): string {
  if (seconds === null || !Number.isFinite(seconds)) {
    return "Unknown duration";
  }
  const total = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(total / 3_600);
  const minutes = Math.floor((total % 3_600) / 60);
  const remaining = total % 60;
  if (hours > 0) {
    return `${hours}h ${minutes.toString().padStart(2, "0")}m`;
  }
  return `${minutes}:${remaining.toString().padStart(2, "0")}`;
}

function formatTranscriptTimestamp(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(total / 3_600);
  const minutes = Math.floor((total % 3_600) / 60);
  const remaining = total % 60;
  return hours > 0
    ? `${hours}:${minutes.toString().padStart(2, "0")}:${remaining.toString().padStart(2, "0")}`
    : `${minutes}:${remaining.toString().padStart(2, "0")}`;
}
