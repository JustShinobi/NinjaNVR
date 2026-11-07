/**
 * Real detector telemetry for the console. Every value here is either a measurement from the
 * server or null; the UI renders null as "—" instead of inventing a number.
 */
import type { DetectorRuntimeCatalog, NinjaStatsResponse } from "../types/nvr";

type StatsLike = Pick<NinjaStatsResponse, "detectors" | "primary_detector"> | null | undefined;

function finitePositive(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) && v > 0 ? v : null;
}

/**
 * Average inference latency (ms) of the detector cameras actually use, or null when it has not
 * run a loaded model yet. Servers older than `primary_detector` fall back to the first named
 * detector, which is what they reported before.
 */
export function resolvePrimaryInferenceMs(stats: StatsLike): number | null {
  if (!stats) return null;
  const primary = stats.primary_detector;
  if (primary) {
    return primary.measured ? finitePositive(primary.inference_speed) : null;
  }
  const first = Object.values(stats.detectors || {})[0];
  if (!first || first.measured === false) return null;
  return finitePositive(first.inference_speed);
}

/** Formats a measured value with fixed decimals and unit, or "—" when there is no measurement. */
export function formatMeasured(value: number | null | undefined, digits = 1, unit = ""): string {
  return typeof value === "number" && Number.isFinite(value) ? `${value.toFixed(digits)}${unit}` : "—";
}

/** Parses the server's numeric strings ("12.3") without substituting a fake default. */
export function parseMeasured(value: unknown): number | null {
  const n = typeof value === "number" ? value : typeof value === "string" ? Number(value) : NaN;
  return Number.isFinite(n) ? n : null;
}

/** Sparkline ceiling that fits the real series instead of clipping slow models at 20 ms. */
export function sparklineCeiling(values: Array<number | null>, minimum = 20): number {
  const peak = values.reduce<number>((m, v) => (typeof v === "number" && v > m ? v : m), 0);
  return Math.max(minimum, Math.ceil(peak * 1.2));
}

/**
 * Measured latency from /api/detectors/catalog, or null before the first real inference.
 * Older servers without `inference_samples` keep reporting their average.
 */
export function catalogInferenceMs(
  catalog: Pick<DetectorRuntimeCatalog, "avg_inference_ms" | "inference_samples" | "loaded"> | null | undefined
): number | null {
  if (!catalog || !catalog.loaded) return null;
  if (catalog.inference_samples === 0) return null;
  return finitePositive(catalog.avg_inference_ms);
}
