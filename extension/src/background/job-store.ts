import type { JobFailure, JobKind, JobProgress, JobRecord, JobStatus } from "../shared/domain";

const STORAGE_KEY = "subtitler.jobs.v1";
const MAX_STORED_JOBS = 50;
const IN_FLIGHT_STATUSES = new Set<JobStatus>(["queued", "connecting", "processing", "buffering"]);

export interface NewJob {
  id: string;
  kind: JobKind;
  tabId?: number;
  mediaId?: string;
  mediaDurationSeconds?: number;
  usesExistingCaptions?: boolean;
}

export interface JobUpdate {
  status?: JobStatus;
  nativeJobId?: string;
  progress?: JobProgress;
  error?: JobFailure;
}

/**
 * Stores only sanitized operational metadata. Source URLs and all user content
 * remain transient and are intentionally excluded from chrome.storage.
 */
export class JobStore {
  private readonly jobs = new Map<string, JobRecord>();
  private initialized = false;
  private writeChain: Promise<void> = Promise.resolve();

  async initialize(): Promise<void> {
    if (this.initialized) {
      return;
    }
    const stored = await storageGet<unknown>(STORAGE_KEY);
    for (const record of parseStoredJobs(stored)) {
      if (IN_FLIGHT_STATUSES.has(record.status)) {
        record.status = "recovering";
        record.updatedAt = new Date().toISOString();
      }
      this.jobs.set(record.id, record);
    }
    this.initialized = true;
    await this.persist();
  }

  list(): JobRecord[] {
    this.requireInitialized();
    return [...this.jobs.values()]
      .sort((left, right) => right.updatedAt.localeCompare(left.updatedAt))
      .map(cloneJob);
  }

  get(id: string): JobRecord | undefined {
    this.requireInitialized();
    const job = this.jobs.get(id);
    return job ? cloneJob(job) : undefined;
  }

  async create(input: NewJob): Promise<JobRecord> {
    this.requireInitialized();
    const now = new Date().toISOString();
    const job: JobRecord = {
      id: input.id,
      kind: input.kind,
      status: input.usesExistingCaptions ? "using-existing-captions" : "queued",
      createdAt: now,
      updatedAt: now
    };
    if (input.tabId !== undefined) {
      job.tabId = input.tabId;
    }
    if (input.mediaId) {
      job.mediaId = input.mediaId;
    }
    if (input.mediaDurationSeconds !== undefined) {
      job.mediaDurationSeconds = input.mediaDurationSeconds;
    }
    if (input.usesExistingCaptions) {
      job.usesExistingCaptions = true;
    }
    this.jobs.set(job.id, job);
    await this.persist();
    return cloneJob(job);
  }

  async update(id: string, update: JobUpdate): Promise<JobRecord | undefined> {
    this.requireInitialized();
    const job = this.jobs.get(id);
    if (!job) {
      return undefined;
    }
    // A user stop is terminal. Native Messaging can deliver a late acceptance,
    // progress, or failure after the UI has already requested cancellation; do
    // not let any of those messages revive the job or attach fresh metadata.
    if (job.status === "stopped") {
      return cloneJob(job);
    }
    if (update.status) {
      job.status = update.status;
    }
    if (update.nativeJobId !== undefined) {
      job.nativeJobId = update.nativeJobId;
    }
    if (update.progress) {
      job.progress = { ...update.progress };
    }
    if (update.error) {
      job.error = { ...update.error };
    }
    job.updatedAt = new Date().toISOString();
    await this.persist();
    return cloneJob(job);
  }

  private async persist(): Promise<void> {
    const records = [...this.jobs.values()]
      .sort((left, right) => right.updatedAt.localeCompare(left.updatedAt))
      .slice(0, MAX_STORED_JOBS)
      .map(cloneJob);
    const idsToKeep = new Set(records.map((record) => record.id));
    for (const id of this.jobs.keys()) {
      if (!idsToKeep.has(id)) {
        this.jobs.delete(id);
      }
    }
    this.writeChain = this.writeChain.catch(() => undefined).then(() => storageSet({ [STORAGE_KEY]: records }));
    await this.writeChain;
  }

  private requireInitialized(): void {
    if (!this.initialized) {
      throw new Error("JobStore must be initialized before use.");
    }
  }
}

/** Returns only the jobs attached to the current browser tab. */
export function jobsForTab(jobs: readonly JobRecord[], tabId: number): JobRecord[] {
  return jobs.filter((job) => job.tabId === tabId).map(cloneJob);
}

function cloneJob(job: JobRecord): JobRecord {
  const clone: JobRecord = {
    id: job.id,
    kind: job.kind,
    status: job.status,
    createdAt: job.createdAt,
    updatedAt: job.updatedAt
  };
  if (job.nativeJobId !== undefined) clone.nativeJobId = job.nativeJobId;
  if (job.tabId !== undefined) clone.tabId = job.tabId;
  if (job.mediaId !== undefined) clone.mediaId = job.mediaId;
  if (job.mediaDurationSeconds !== undefined) clone.mediaDurationSeconds = job.mediaDurationSeconds;
  if (job.progress) clone.progress = { ...job.progress };
  if (job.error) clone.error = { ...job.error };
  if (job.usesExistingCaptions) clone.usesExistingCaptions = true;
  return clone;
}

function parseStoredJobs(value: unknown): JobRecord[] {
  if (!Array.isArray(value)) {
    return [];
  }
  return value.flatMap((item) => {
    const parsed = parseJobRecord(item);
    return parsed ? [parsed] : [];
  });
}

function parseJobRecord(value: unknown): JobRecord | undefined {
  if (!isRecord(value) || !isJobKind(value.kind) || !isJobStatus(value.status)) {
    return undefined;
  }
  if (typeof value.id !== "string" || typeof value.createdAt !== "string" || typeof value.updatedAt !== "string") {
    return undefined;
  }
  const job: JobRecord = {
    id: value.id,
    kind: value.kind,
    status: value.status,
    createdAt: value.createdAt,
    updatedAt: value.updatedAt
  };
  if (isNativeJobId(value.nativeJobId)) job.nativeJobId = value.nativeJobId;
  if (typeof value.tabId === "number" && Number.isInteger(value.tabId)) job.tabId = value.tabId;
  if (typeof value.mediaId === "string") job.mediaId = value.mediaId;
  if (isNonNegativeNumber(value.mediaDurationSeconds)) job.mediaDurationSeconds = value.mediaDurationSeconds;
  const progress = parseProgress(value.progress);
  if (progress) job.progress = progress;
  const error = parseFailure(value.error);
  if (error) job.error = error;
  if (value.usesExistingCaptions === true) job.usesExistingCaptions = true;
  return job;
}

function parseProgress(value: unknown): JobProgress | undefined {
  if (!isRecord(value)) return undefined;
  const progress: JobProgress = {};
  if (isNonNegativeNumber(value.processedSeconds)) progress.processedSeconds = value.processedSeconds;
  if (isNonNegativeNumber(value.durationSeconds)) progress.durationSeconds = value.durationSeconds;
  if (isNonNegativeNumber(value.percent)) progress.percent = value.percent;
  if (isNonNegativeNumber(value.subtitleBufferSeconds)) progress.subtitleBufferSeconds = value.subtitleBufferSeconds;
  if (typeof value.statusMessage === "string" && value.statusMessage.length <= 8_000) {
    progress.statusMessage = value.statusMessage;
  }
  const phases = ["resolving", "acquiring", "decoding", "transcribing", "segmenting", "finalizing", "complete", "failed", "cancelled", "stale"] as const;
  const phase = value.phase;
  if (phases.includes(phase as (typeof phases)[number])) progress.phase = phase as NonNullable<JobProgress["phase"]>;
  if (typeof value.lastProgressAt === "string" && value.lastProgressAt.length <= 64) progress.lastProgressAt = value.lastProgressAt;
  if (isNonNegativeNumber(value.mediaBytesProcessed)) progress.mediaBytesProcessed = value.mediaBytesProcessed;
  if (isNonNegativeNumber(value.audioSecondsDecoded)) progress.audioSecondsDecoded = value.audioSecondsDecoded;
  if (isNonNegativeNumber(value.audioSecondsTranscribed)) progress.audioSecondsTranscribed = value.audioSecondsTranscribed;
  if (isNonNegativeNumber(value.completedIntervals)) progress.completedIntervals = value.completedIntervals;
  if (isNonNegativeNumber(value.workerPid) && Number.isInteger(value.workerPid)) progress.workerPid = value.workerPid;
  const workerStatuses = ["not_started", "active", "waiting", "finished", "unavailable"] as const;
  const workerStatus = value.workerStatus;
  if (workerStatuses.includes(workerStatus as (typeof workerStatuses)[number])) {
    progress.workerStatus = workerStatus as NonNullable<JobProgress["workerStatus"]>;
  }
  return Object.keys(progress).length > 0 ? progress : undefined;
}

function parseFailure(value: unknown): JobFailure | undefined {
  if (!isRecord(value) || typeof value.message !== "string" || typeof value.retryable !== "boolean") {
    return undefined;
  }
  const codes: JobFailure["code"][] = [
    "COMPANION_UNAVAILABLE",
    "UNSUPPORTED_MEDIA",
    "PROTECTED_MEDIA",
    "NATIVE_ERROR",
    "USER_STOPPED",
    "UNKNOWN"
  ];
  return codes.includes(value.code as JobFailure["code"])
    ? { code: value.code as JobFailure["code"], message: value.message, retryable: value.retryable }
    : undefined;
}

function isJobKind(value: unknown): value is JobKind {
  return value === "subtitle" || value === "transcript";
}

function isJobStatus(value: unknown): value is JobStatus {
  return (
    value === "queued" ||
    value === "connecting" ||
    value === "processing" ||
    value === "buffering" ||
    value === "using-existing-captions" ||
    value === "completed" ||
    value === "stopped" ||
    value === "failed" ||
    value === "recovering" ||
    value === "stale"
  );
}

function isNonNegativeNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

function isNativeJobId(value: unknown): value is string {
  return (
    typeof value === "string" &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value)
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function storageGet<T>(key: string): Promise<T | undefined> {
  return new Promise((resolve, reject) => {
    chrome.storage.local.get(key, (items) => {
      const error = chrome.runtime.lastError;
      if (error) {
        reject(new Error(error.message));
        return;
      }
      resolve(items[key] as T | undefined);
    });
  });
}

function storageSet(items: Record<string, unknown>): Promise<void> {
  return new Promise((resolve, reject) => {
    chrome.storage.local.set(items, () => {
      const error = chrome.runtime.lastError;
      if (error) {
        reject(new Error(error.message));
        return;
      }
      resolve();
    });
  });
}
