import type { JobRecord } from "../shared/domain";

/**
 * Select completed native transcripts that need their private, in-memory
 * result cache rebuilt for this service-worker lifetime. The caller records a
 * selected ID before changing its persisted status, making repeated popup
 * polls idempotent.
 */
export function completedTranscriptJobsToHydrate(
  jobs: readonly JobRecord[],
  activeTabId: number | undefined,
  hydratedIds: ReadonlySet<string>
): JobRecord[] {
  if (activeTabId === undefined) {
    return [];
  }
  return jobs.filter(
    (job) =>
      job.tabId === activeTabId &&
      job.kind === "transcript" &&
      (job.status === "completed" || (job.status === "failed" && job.error?.code === "NATIVE_ERROR")) &&
      job.nativeJobId !== undefined &&
      !hydratedIds.has(job.id) &&
      !job.usesExistingCaptions
  );
}
