import type { NativeLocalProcessingAdvisory } from "../shared/protocol";

/**
 * Keep the normal popup intentionally non-technical. The detailed model and
 * backend advisory is never shown here; advanced settings can use it later.
 */
export function describeLocalPerformance(advisory: NativeLocalProcessingAdvisory, localProcessingAvailable: boolean): string {
  const prefix = localProcessingAvailable ? "Local processing:" : "Local plan when the engine is available:";
  switch (advisory.localPerformance) {
    case "excellent":
      return `${prefix} excellent performance expected.`;
    case "good":
      return `${prefix} good performance expected.`;
    case "may_be_slow":
    case "cloud_helpful":
      return `${prefix} this device may be slow.`;
  }
}
