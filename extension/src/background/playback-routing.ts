import type { JobRecord } from "../shared/domain";

export interface PlaybackRouteTarget {
  tabId: number;
  jobId: string;
  mediaId: string;
}

/**
 * A content-script snapshot is eligible only for the exact generated subtitle
 * job that owns its tab and media element. Keeping this pure makes the
 * least-privilege routing rule independently testable.
 */
export function findActiveGeneratedSubtitleJob(
  jobs: readonly JobRecord[],
  target: PlaybackRouteTarget
): JobRecord | undefined {
  return jobs.find(
    (candidate) =>
      candidate.id === target.jobId &&
      candidate.kind === "subtitle" &&
      candidate.usesExistingCaptions !== true &&
      candidate.nativeJobId !== undefined &&
      candidate.tabId === target.tabId &&
      candidate.mediaId === target.mediaId &&
      (candidate.status === "processing" || candidate.status === "buffering")
  );
}
