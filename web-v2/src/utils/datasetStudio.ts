import type { DatasetSampleItem, DatasetSummaryResponse, ModelConfig } from "../types/nvr";

/**
 * Pure domain logic for the AI Dataset Studio flow:
 * Anotar (annotate + verify) → Prontidão (readiness) → Treinar → Implantar.
 */

export type DatasetQueueStatus = "pending" | "ai" | "manual" | "background";
export type DatasetQueueTab = "pending" | "ai" | "manual" | "all";
export type DatasetQueueKind = "all" | "objects" | "background";
export type DatasetStudioStep = "annotate" | "readiness" | "train";

export interface DatasetQueueFilter {
  tab: DatasetQueueTab;
  camera: string; // "all" or a camera name
  kind: DatasetQueueKind;
}

export interface DatasetQueueCounts {
  pending: number;
  ai: number;
  manual: number;
  background: number;
  objects: number;
  all: number;
}

export function isBackgroundSample(s: Pick<DatasetSampleItem, "sample_kind">): boolean {
  return Boolean(s && s.sample_kind === "hard_negative");
}

/** Where a sample sits in the queue: background, still pending, or verified by AI / an operator. */
export function datasetQueueStatus(
  s: Pick<DatasetSampleItem, "sample_kind" | "verified" | "verified_by">
): DatasetQueueStatus {
  if (!s) return "pending";
  if (isBackgroundSample(s)) return "background";
  if (!s.verified) return "pending";
  return s.verified_by === "ai" ? "ai" : "manual";
}

/**
 * A frame with objects can only be confirmed once it has at least one box;
 * confirming it empty would export an unlabeled positive. Background frames
 * are confirmed as "no objects" by definition.
 */
export function canConfirmSample(
  s: Pick<DatasetSampleItem, "sample_kind" | "annotations">
): boolean {
  return Boolean(
    s && (isBackgroundSample(s) || (Array.isArray(s.annotations) && s.annotations.length > 0))
  );
}

/** Lowest box confidence of a sample, or null when it has no scored boxes. */
export function minAnnotationScore(s: Pick<DatasetSampleItem, "annotations">): number | null {
  let min: number | null = null;
  for (const ann of Array.isArray(s?.annotations) ? s.annotations : []) {
    const score = ann?.score;
    if (typeof score !== "number" || !Number.isFinite(score)) continue;
    if (min === null || score < min) min = score;
  }
  return min;
}

export function computeDatasetQueueCounts(samples: DatasetSampleItem[]): DatasetQueueCounts {
  const counts: DatasetQueueCounts = {
    pending: 0,
    ai: 0,
    manual: 0,
    background: 0,
    objects: 0,
    all: 0,
  };
  for (const s of Array.isArray(samples) ? samples : []) {
    if (!s) continue;
    counts.all += 1;
    const status = datasetQueueStatus(s);
    counts[status] += 1;
    if (status !== "background") counts.objects += 1;
  }
  return counts;
}

/** Cameras present in the dataset with their sample count, alphabetically. */
export function datasetCameraCounts(samples: DatasetSampleItem[]): Array<[string, number]> {
  const map = new Map<string, number>();
  for (const s of Array.isArray(samples) ? samples : []) {
    if (!s || typeof s.camera !== "string" || !s.camera) continue;
    map.set(s.camera, (map.get(s.camera) ?? 0) + 1);
  }
  return Array.from(map.entries()).sort((a, b) => a[0].localeCompare(b[0]));
}

function safeSampleCreatedAt(s: DatasetSampleItem): number {
  return typeof s?.created_at === "number" && Number.isFinite(s.created_at)
    ? s.created_at
    : 0;
}

/**
 * Filters the queue. AI-verified samples are ordered by their lowest box
 * confidence first, so a spot-check starts where the Teacher was least sure;
 * every other view is newest first.
 */
export function filterDatasetQueue(
  samples: DatasetSampleItem[],
  filter: DatasetQueueFilter
): DatasetSampleItem[] {
  const rows = (Array.isArray(samples) ? samples : []).filter((s) => {
    if (!s) return false;
    const status = datasetQueueStatus(s);
    if (filter.tab !== "all" && status !== filter.tab) return false;
    if (filter.camera !== "all" && s.camera !== filter.camera) return false;
    if (filter.kind === "objects" && status === "background") return false;
    if (filter.kind === "background" && status !== "background") return false;
    return true;
  });
  if (filter.tab === "ai") {
    return rows.sort((a, b) => {
      const sa = minAnnotationScore(a);
      const sb = minAnnotationScore(b);
      const dt = safeSampleCreatedAt(b) - safeSampleCreatedAt(a);
      if (sa === null && sb === null) return dt || String(a.id ?? "").localeCompare(String(b.id ?? ""));
      if (sa === null) return 1;
      if (sb === null) return -1;
      return sa - sb || dt || String(a.id ?? "").localeCompare(String(b.id ?? ""));
    });
  }
  return rows.sort(
    (a, b) =>
      safeSampleCreatedAt(b) - safeSampleCreatedAt(a) ||
      String(a.id ?? "").localeCompare(String(b.id ?? ""))
  );
}

/** Next/previous id in the visible queue; clamps at both ends. */
export function stepDatasetQueue(
  list: DatasetSampleItem[],
  currentId: string | null,
  direction: 1 | -1
): string | null {
  if (!Array.isArray(list) || list.length === 0) return null;
  const idx = list.findIndex((s) => s.id === currentId);
  if (idx < 0) return list[0].id;
  const step = direction === -1 ? -1 : 1;
  const next = Math.max(0, Math.min(list.length - 1, idx + step));
  return list[next].id;
}

/**
 * Next/previous id when the sample on stage may have just left the filtered
 * queue (e.g. editing an AI-verified sample makes it manual). `lastIndex` is
 * where it sat in the queue, so "next" lands on the item that took its place.
 */
export function stepStickyDatasetQueue(
  list: DatasetSampleItem[],
  currentId: string | null,
  lastIndex: number,
  direction: 1 | -1
): string | null {
  if (!Array.isArray(list) || list.length === 0) return null;
  if (list.some((s) => s.id === currentId)) return stepDatasetQueue(list, currentId, direction);
  const safeLast =
    typeof lastIndex === "number" && Number.isFinite(lastIndex)
      ? Math.max(0, Math.floor(lastIndex))
      : 0;
  const target = direction === 1 ? safeLast : safeLast - 1;
  return list[Math.max(0, Math.min(list.length - 1, target))].id;
}

/**
 * Turns a pointer drag (normalized 0..1 corners) into an xyxy box, or null
 * when the drag is too small to be an intentional box (a stray click).
 */
export function normalizeDragBox(
  start: [number, number],
  end: [number, number],
  minSize = 0.01
): [number, number, number, number] | null {
  if (!Array.isArray(start) || !Array.isArray(end)) return null;
  const safeMin =
    typeof minSize === "number" && Number.isFinite(minSize) && minSize >= 0
      ? minSize
      : 0.01;
  const clamp = (v: number) => Math.max(0, Math.min(1, Number.isFinite(v) ? v : 0));
  const sx = clamp(start[0]);
  const sy = clamp(start[1]);
  const ex = clamp(end[0]);
  const ey = clamp(end[1]);
  const x1 = Math.min(sx, ex);
  const y1 = Math.min(sy, ey);
  const x2 = Math.max(sx, ex);
  const y2 = Math.max(sy, ey);
  if (x2 - x1 < safeMin || y2 - y1 < safeMin) return null;
  return [x1, y1, x2, y2];
}

// ---------------------------------------------------------------------------
// Readiness checks
// ---------------------------------------------------------------------------

export type ReadinessSeverity = "ok" | "warn" | "block" | "info";

export interface ReadinessCheck {
  id: "verification" | "minimum" | "balance" | "split" | "cameras" | "night" | "classes";
  severity: ReadinessSeverity;
  title: string;
  detail: string;
  value: string;
}

export interface DatasetReadinessReport {
  checks: ReadinessCheck[];
  verdict: "ready" | "warnings" | "blocked";
  warnCount: number;
  blockCount: number;
}

export const DATASET_MIN_SAMPLES = 20;
export const DATASET_MIN_PER_CAMERA = 15;
export const DATASET_MIN_BOXES_PER_CLASS = 10;

function safeCount(v: unknown): number {
  const n = Number(v ?? 0);
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
}

function pct(part: number, total: number): number {
  return total > 0 ? Math.round((part / total) * 100) : 0;
}

/**
 * Evaluates whether the dataset is ready to train. Blocks stop the export
 * from being meaningful; warnings only degrade quality.
 */
export function buildDatasetReadinessReport(
  summary: Partial<DatasetSummaryResponse> | null | undefined,
  isPt: boolean
): DatasetReadinessReport {
  const t = (pt: string, en: string) => (isPt ? pt : en);
  const tp = safeCount(summary?.true_positive_count);
  const ai = safeCount(summary?.ai_annotated_count);
  const bg = safeCount(summary?.hard_negative_count);
  const total = Math.max(safeCount(summary?.total_samples), tp + ai + bg);
  const objects = tp + ai;
  const pending = safeCount(summary?.pending_count);
  const aiVerified = safeCount(summary?.ai_verified_count);
  const manualVerified = safeCount(summary?.manual_verified_count);
  const train = safeCount(summary?.train_count);
  const val = safeCount(summary?.val_count);
  const night = safeCount(summary?.night_mode_count);
  const checks: ReadinessCheck[] = [];

  checks.push({
    id: "verification",
    severity: pending > 0 ? "block" : "ok",
    title: t("Verificação", "Verification"),
    detail:
      pending > 0
        ? t(
            `${pending} frame(s) ainda sem verificação. Rode a IA ou confira à mão.`,
            `${pending} frame(s) still unverified. Run the AI or check them by hand.`
          )
        : t(
            `${aiVerified} pela IA, ${manualVerified} conferidas à mão, ${bg} de fundo.`,
            `${aiVerified} by AI, ${manualVerified} checked by hand, ${bg} background.`
          ),
    value: `${Math.max(0, total - pending)}/${total}`,
  });

  checks.push({
    id: "minimum",
    severity: total < DATASET_MIN_SAMPLES ? "block" : "ok",
    title: t("Tamanho mínimo", "Minimum size"),
    detail:
      total < DATASET_MIN_SAMPLES
        ? t(
            `São necessários pelo menos ${DATASET_MIN_SAMPLES} frames para um treino útil.`,
            `At least ${DATASET_MIN_SAMPLES} frames are needed for a useful run.`
          )
        : t(`${total} frames no dataset.`, `${total} frames in the dataset.`),
    value: `${total}`,
  });

  const objectsPct = pct(objects, total);
  const balanced = total > 0 && objectsPct >= 70 && objectsPct <= 90;
  checks.push({
    id: "balance",
    severity: total === 0 ? "info" : balanced ? "ok" : "warn",
    title: t("Objetos vs fundo", "Objects vs background"),
    detail: balanced
      ? t(
          `${objects} frames com objetos, ${bg} de fundo. Alvo: cerca de 80/20.`,
          `${objects} frames with objects, ${bg} background. Target: about 80/20.`
        )
      : objectsPct > 90
      ? t(
          `Pouco fundo (${bg}). Marque falsos positivos como fundo para o modelo aprender a não disparar.`,
          `Too little background (${bg}). Mark false positives as background so the model learns not to fire.`
        )
      : t(
          `Fundo demais (${bg}) para ${objects} frames com objetos. Alvo: cerca de 80/20.`,
          `Too much background (${bg}) for ${objects} frames with objects. Target: about 80/20.`
        ),
    value: `${objectsPct} / ${total > 0 ? 100 - objectsPct : 0}`,
  });

  const valPct = pct(val, train + val);
  checks.push({
    id: "split",
    severity: total === 0 ? "info" : val === 0 ? "block" : valPct >= 10 && valPct <= 30 ? "ok" : "warn",
    title: t("Treino / validação", "Train / validation"),
    detail:
      val === 0
        ? t(
            "Nenhum frame de validação: não dá para medir o modelo.",
            "No validation frames: the model cannot be measured."
          )
        : t(
            `Dividido automaticamente por hash. ${train} treino, ${val} validação.`,
            `Split automatically by hash. ${train} train, ${val} validation.`
          ),
    value: `${100 - valPct} / ${valPct}`,
  });

  const cams = Object.entries(summary?.by_camera ?? {}).map(
    ([cam, n]) => [cam, safeCount(n)] as [string, number]
  );
  const thinCams = cams.filter(([, n]) => n < DATASET_MIN_PER_CAMERA).map(([c]) => c);
  checks.push({
    id: "cameras",
    severity: cams.length === 0 ? "info" : thinCams.length > 0 ? "warn" : "ok",
    title: t("Cobertura por câmera", "Per-camera coverage"),
    detail:
      thinCams.length > 0
        ? t(
            `${thinCams.join(", ")} com menos de ${DATASET_MIN_PER_CAMERA} frames. O modelo tende a errar nesse ângulo.`,
            `${thinCams.join(", ")} below ${DATASET_MIN_PER_CAMERA} frames. The model tends to miss at that angle.`
          )
        : t(`${cams.length} câmera(s) bem representadas.`, `${cams.length} camera(s) well covered.`),
    value: `${cams.length}`,
  });

  checks.push({
    id: "night",
    severity: total === 0 ? "info" : night === 0 ? "warn" : "ok",
    title: t("Frames noturnos (IR)", "Night frames (IR)"),
    detail:
      night === 0
        ? t(
            "Nenhum frame infravermelho. O modelo novo pode piorar à noite.",
            "No infrared frames. The new model may get worse at night."
          )
        : t(`${night} frame(s) em modo IR.`, `${night} frame(s) in IR mode.`),
    value: `${night}`,
  });

  const classes = Object.entries(summary?.by_class ?? {}).map(
    ([cls, n]) => [cls, safeCount(n)] as [string, number]
  );
  const weak = classes.filter(([, n]) => n < DATASET_MIN_BOXES_PER_CLASS).length;
  checks.push({
    id: "classes",
    severity: "info",
    title: t("Boxes por classe", "Boxes per class"),
    detail: t(
      `${weak} classe(s) com menos de ${DATASET_MIN_BOXES_PER_CLASS} boxes praticamente não são aprendidas.`,
      `${weak} class(es) under ${DATASET_MIN_BOXES_PER_CLASS} boxes are effectively not learned.`
    ),
    value: `${classes.length - weak}/${classes.length}`,
  });

  const blockCount = checks.filter((c) => c.severity === "block").length;
  const warnCount = checks.filter((c) => c.severity === "warn").length;
  return {
    checks,
    verdict: blockCount > 0 ? "blocked" : warnCount > 0 ? "warnings" : "ready",
    warnCount,
    blockCount,
  };
}

/** Classes ordered by box count (desc) for the coverage bars. */
export function datasetClassCoverage(
  summary: Partial<DatasetSummaryResponse> | null | undefined
): Array<{ label: string; count: number; pct: number }> {
  const rows = Object.entries(summary?.by_class ?? {})
    .map(([label, n]) => ({ label, count: safeCount(n) }))
    .sort((a, b) => b.count - a.count || a.label.localeCompare(b.label));
  const max = rows.reduce((m, r) => Math.max(m, r.count), 0);
  return rows.map((r) => ({ ...r, pct: max > 0 ? Math.round((r.count / max) * 100) : 0 }));
}

// ---------------------------------------------------------------------------
// Pipeline rail
// ---------------------------------------------------------------------------

export type PipelineTone = "done" | "warn" | "block" | "idle";

export interface PipelineStepState {
  id: DatasetStudioStep | "deploy";
  target: DatasetStudioStep;
  index: number;
  title: string;
  detail: string;
  tone: PipelineTone;
}

export function buildDatasetPipeline(
  counts: DatasetQueueCounts,
  report: DatasetReadinessReport,
  activeModelName: string,
  isPt: boolean
): PipelineStepState[] {
  const t = (pt: string, en: string) => (isPt ? pt : en);
  const verified = counts.ai + counts.manual + counts.background;
  return [
    {
      id: "annotate",
      target: "annotate",
      index: 1,
      title: t("ANOTAR", "ANNOTATE"),
      detail:
        counts.all === 0
          ? t("Nenhuma amostra ainda", "No samples yet")
          : t(
              `${counts.pending} pendentes · ${verified} verificadas`,
              `${counts.pending} pending · ${verified} verified`
            ),
      tone: counts.all === 0 ? "idle" : counts.pending > 0 ? "warn" : "done",
    },
    {
      id: "readiness",
      target: "readiness",
      index: 2,
      title: t("PRONTIDÃO", "READINESS"),
      detail: t(
        `${report.warnCount} avisos · ${report.blockCount} bloqueios`,
        `${report.warnCount} warnings · ${report.blockCount} blockers`
      ),
      tone: report.blockCount > 0 ? "block" : report.warnCount > 0 ? "warn" : "done",
    },
    {
      id: "train",
      target: "train",
      index: 3,
      title: t("TREINAR", "TRAIN"),
      detail: t("Exportar .zip · Colab ou GPU local", "Export .zip · Colab or local GPU"),
      tone: "idle",
    },
    {
      id: "deploy",
      target: "train",
      index: 4,
      title: t("IMPLANTAR", "DEPLOY"),
      detail: t(`Ativo: ${activeModelName}`, `Active: ${activeModelName}`),
      tone: "idle",
    },
  ];
}

export interface DatasetNextAction {
  kind: "batch-annotate" | "goto-readiness" | "goto-train" | "collect";
  label: string;
}

/** The single primary call to action shown next to the pipeline rail. */
export function resolveDatasetNextAction(
  counts: DatasetQueueCounts,
  report: DatasetReadinessReport,
  step: DatasetStudioStep,
  isPt: boolean
): DatasetNextAction {
  const t = (pt: string, en: string) => (isPt ? pt : en);
  if (counts.all === 0) {
    return { kind: "collect", label: t("Coletar amostras no Explore", "Collect samples in Explore") };
  }
  if (counts.pending > 0) {
    return {
      kind: "batch-annotate",
      label: t(`Verificar ${counts.pending} com IA`, `Verify ${counts.pending} with AI`),
    };
  }
  if (step === "annotate") {
    return { kind: "goto-readiness", label: t("Checar prontidão", "Check readiness") };
  }
  if (step === "readiness") {
    return report.blockCount > 0
      ? { kind: "collect", label: t("Coletar mais amostras", "Collect more samples") }
      : { kind: "goto-train", label: t("Ir para o treino", "Go to training") };
  }
  return { kind: "goto-train", label: t("Baixar dataset .zip", "Download dataset .zip") };
}

/** Basename of the active detector model, without extension. */
export function activeModelDisplayName(path: string | null | undefined): string {
  const base = String(path ?? "").split(/[/\\]/).pop() ?? "";
  const stem = base.replace(/\.(?:xml|onnx|bin)$/i, "");
  return stem || "—";
}

// ---------------------------------------------------------------------------------------------
// Model deploy feedback
// ---------------------------------------------------------------------------------------------

export interface ModelDeployOutcome {
  /** The new model is the configured model now (it may still have failed to load). */
  ok: boolean;
  /** The detector runtime actually loaded the model. */
  loaded: boolean;
  /** config.yml was updated, so the deploy survives a restart. */
  persisted: boolean;
  /** The model failed to load and the previous one was restored. */
  rolledBack: boolean;
  filename: string;
  modelPath: string;
  width: number;
  height: number;
  /** Epoch milliseconds of the deploy. */
  at: number;
  message: string;
}

function asRecord(v: unknown): Record<string, unknown> {
  return v && typeof v === "object" ? (v as Record<string, unknown>) : {};
}

/** Turns the /api/detectors/models/upload response into an outcome the UI can keep on screen. */
export function parseModelDeployResponse(
  status: number,
  body: unknown,
  nowMs: number
): ModelDeployOutcome {
  const d = asRecord(body);
  const num = (v: unknown, fallback: number) => (typeof v === "number" && Number.isFinite(v) ? v : fallback);
  const str = (v: unknown) => (typeof v === "string" ? v : "");
  const ok = status >= 200 && status < 300 && d.success === true;
  const deployedAt = num(d.deployed_at, 0);
  return {
    ok,
    // Older servers do not report "loaded"; treat an explicit false as the only failure signal.
    loaded: ok ? d.loaded !== false : false,
    persisted: ok ? d.persisted !== false : false,
    rolledBack: d.rolled_back === true,
    filename: str(d.filename),
    modelPath: str(d.model_path),
    width: num(d.width, 0),
    height: num(d.height, 0),
    at: deployedAt > 0 ? deployedAt * 1000 : nowMs,
    message: str(d.message) || (ok ? "" : `HTTP ${status}`),
  };
}

export type ActiveModelState = "loaded" | "not_loaded" | "pending" | "unknown";

/**
 * What the detector is really running, from /api/detectors/catalog, compared with the model
 * config.yml points at. "pending" means config and runtime disagree (saved but not reloaded).
 */
export function resolveActiveModelState(
  catalog: { loaded?: boolean; active_model_path?: string } | null,
  configModelPath: string
): ActiveModelState {
  if (!catalog) return "unknown";
  const runtimePath = (catalog.active_model_path || "").trim();
  if (runtimePath && configModelPath.trim() && runtimePath !== configModelPath.trim()) return "pending";
  return catalog.loaded ? "loaded" : "not_loaded";
}

/** Applies the fields a deploy reported onto a model config, ignoring fields it did not send. */
export function mergeDeployedModel(current: ModelConfig, patch: Partial<ModelConfig>): ModelConfig {
  const next: ModelConfig = { ...current };
  (Object.keys(patch) as Array<keyof ModelConfig>).forEach((k) => {
    const v = patch[k];
    if (v !== undefined && v !== null && v !== "") {
      (next as unknown as Record<string, unknown>)[k] = v;
    }
  });
  return next;
}
