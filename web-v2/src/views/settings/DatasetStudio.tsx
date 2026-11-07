import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Copy, ExternalLink, Sparkles, Trash2, Upload, X } from "lucide-react";
import { computeContainedBox } from "../../components/VideoPlayers";
import { useI18n } from "../../i18n";
import { catalogInferenceMs, formatMeasured } from "../../utils/detectorTelemetry";
import type {
  DetectorRuntimeCatalog,
  ModelConfig,
  DatasetAnnotationBox,
  DatasetSampleItem,
  DatasetSummaryResponse,
  NinjaConfigResponse,
} from "../../types/nvr";
import { formatDateTime, formatTs } from "../../utils/format";
import {
  activeModelDisplayName,
  parseModelDeployResponse,
  resolveActiveModelState,
  type ModelDeployOutcome,
  buildDatasetPipeline,
  canConfirmSample,
  buildDatasetReadinessReport,
  computeDatasetQueueCounts,
  datasetCameraCounts,
  datasetClassCoverage,
  datasetQueueStatus,
  DATASET_MIN_BOXES_PER_CLASS,
  DATASET_MIN_PER_CAMERA,
  DATASET_MIN_SAMPLES,
  filterDatasetQueue,
  minAnnotationScore,
  normalizeDragBox,
  resolveDatasetNextAction,
  stepDatasetQueue,
  stepStickyDatasetQueue,
  type DatasetQueueFilter,
  type DatasetQueueKind,
  type DatasetQueueTab,
  type DatasetStudioStep,
  type ReadinessCheck,
} from "../../utils/datasetStudio";
import {
  applySecurity15ClassPreset,
  buildLocalFinetuneCliCommand,
  buildModelUploadPayload,
  COLAB_MODEL_FACTORY_URL,
  computeBatchProgressMetrics,
  isSafeDatasetSampleId,
  NINJANVR_SECURITY_15CL_LABELMAP_PATH,
  NINJANVR_SECURITY_CLASSES,
  selectBatchAutoAnnotateCandidates,
  xyxyToYoloNormalized,
  yoloNormalizedToXyxy,
  type BatchAutoAnnotateProgress,
} from "./VisionAndAiPanels";

const LOW_CONFIDENCE = 0.5;
/** Quick class buttons, bound to keys 1..7 in this order. */
const QUICK_CLASS_IDS = [0, 1, 2, 3, 7, 8, 11] as const;

type Point = [number, number];

function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA" || target.isContentEditable;
}

function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return "0 B";
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

async function readSampleList(res: Response): Promise<DatasetSampleItem[]> {
  const data = await res.json().catch(() => null);
  const raw: unknown[] = Array.isArray(data)
    ? data
    : Array.isArray(data?.items)
    ? data.items
    : [];
  return raw.filter((item: unknown): item is DatasetSampleItem =>
    Boolean(item && typeof item === "object" && isSafeDatasetSampleId((item as { id?: unknown }).id))
  );
}

export const DatasetStudioSection: React.FC<{
  draftConfig?: NinjaConfigResponse | null;
  updateDraft?: (updater: (next: NinjaConfigResponse) => void) => void;
  notify: (msg: string) => void;
  /** Called after a deploy so the saved config and the draft follow the new model. */
  onModelDeployed?: (model: Partial<ModelConfig>) => void;
}> = ({ draftConfig, updateDraft, notify, onModelDeployed }) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";
  const t = useCallback((pt: string, en: string) => (isPt ? pt : en), [isPt]);

  const [summary, setSummary] = useState<DatasetSummaryResponse | null>(null);
  const [samples, setSamples] = useState<DatasetSampleItem[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [step, setStep] = useState<DatasetStudioStep>("annotate");
  const [filter, setFilter] = useState<DatasetQueueFilter>({
    tab: "all",
    camera: "all",
    kind: "all",
  });
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [selectedBox, setSelectedBox] = useState<number | null>(null);
  const [activeClassId, setActiveClassId] = useState<number>(0);
  const [draft, setDraft] = useState<{ start: Point; end: Point } | null>(null);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [batchProgress, setBatchProgress] = useState<BatchAutoAnnotateProgress | null>(null);
  const batchCancelRef = useRef(false);
  const batchRunningRef = useRef(false);
  const batchAbortRef = useRef<AbortController | null>(null);
  const uploadFileReaderRef = useRef<FileReader | null>(null);
  const [trainTarget, setTrainTarget] = useState<"colab" | "local">("colab");
  const [upload, setUpload] = useState<{ name: string; size: number; base64: string } | null>(null);
  const [uploadFilename, setUploadFilename] = useState("ninjanvr_yolo11s.onnx");
  const [uploadDims, setUploadDims] = useState("640x640");
  const [dragOver, setDragOver] = useState(false);
  const [catalog, setCatalog] = useState<DetectorRuntimeCatalog | null>(null);
  const [deployOutcome, setDeployOutcome] = useState<ModelDeployOutcome | null>(null);
  const [stageSize, setStageSize] = useState<{ w: number; h: number }>({ w: 0, h: 0 });
  const [naturalSize, setNaturalSize] = useState<{ w: number; h: number } | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const loadSeqRef = useRef(0);
  const isMountedRef = useRef(true);
  const tabInitializedRef = useRef(false);

  const modelCfg = draftConfig?.model || {
    path: "/openvino-model/ssdlite_mobilenet_v2.xml",
    labelmap_path: "/openvino-model/coco_91cl_bkgr.txt",
    width: 300,
    height: 300,
    input_tensor: "nhwc",
    input_pixel_format: "bgr",
    model_type: "ssd",
  };

  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
      batchCancelRef.current = true;
      if (batchAbortRef.current) {
        try {
          batchAbortRef.current.abort();
        } catch {
          // ignore
        }
        batchAbortRef.current = null;
      }
      if (uploadFileReaderRef.current) {
        try {
          uploadFileReaderRef.current.abort();
        } catch {
          // ignore
        }
        uploadFileReaderRef.current = null;
      }
    };
  }, []);

  const loadDatasetState = useCallback(async () => {
    const seq = ++loadSeqRef.current;
    try {
      const [sumRes, listRes] = await Promise.all([
        fetch("/api/dataset/summary", { credentials: "include" }),
        fetch("/api/dataset/samples", { credentials: "include" }),
      ]);
      if (!isMountedRef.current || seq !== loadSeqRef.current) return;
      if (sumRes.ok) {
        const sumData = await sumRes.json().catch(() => null);
        if (isMountedRef.current && seq === loadSeqRef.current && sumData) setSummary(sumData);
      }
      if (listRes.ok) {
        const rows = await readSampleList(listRes);
        if (!isMountedRef.current || seq !== loadSeqRef.current) return;
        setSamples(rows);
      }
    } catch {
      if (isMountedRef.current) notify(t("Falha ao carregar o dataset", "Failed to load the dataset"));
    } finally {
      if (isMountedRef.current && seq === loadSeqRef.current) setLoaded(true);
    }
  }, [notify, t]);

  useEffect(() => {
    void loadDatasetState();
  }, [loadDatasetState]);

  const counts = useMemo(() => computeDatasetQueueCounts(samples), [samples]);
  const cameras = useMemo(() => datasetCameraCounts(samples), [samples]);
  const queue = useMemo(() => filterDatasetQueue(samples, filter), [samples, filter]);
  const report = useMemo(() => buildDatasetReadinessReport(summary, isPt), [summary, isPt]);
  const pipeline = useMemo(
    () => buildDatasetPipeline(counts, report, activeModelDisplayName(modelCfg.path), isPt),
    [counts, report, modelCfg.path, isPt]
  );
  const nextAction = resolveDatasetNextAction(counts, report, step, isPt);
  // The sample on stage stays put even after an edit moves it out of the
  // current tab; navigation then resumes from where it sat in the queue.
  const selected =
    queue.find((s) => s.id === selectedId) ??
    (selectedId ? samples.find((s) => s.id === selectedId) : undefined) ??
    queue[0] ??
    null;
  const selectedQueueIdx = selected ? queue.findIndex((s) => s.id === selected.id) : -1;
  const lastQueueIdxRef = useRef(0);
  if (selectedQueueIdx >= 0) lastQueueIdxRef.current = selectedQueueIdx;
  const stepQueue = (direction: 1 | -1) =>
    stepStickyDatasetQueue(queue, selected?.id ?? null, lastQueueIdxRef.current, direction);
  const batchMetrics = computeBatchProgressMetrics(batchProgress);
  const batchRunning = busyAction === "batch-auto";

  // Land on the tab that has work: pending first, then the AI spot-check.
  useEffect(() => {
    if (tabInitializedRef.current || !loaded) return;
    tabInitializedRef.current = true;
    setFilter((f) => ({ ...f, tab: counts.pending > 0 ? "pending" : counts.ai > 0 ? "ai" : "all" }));
  }, [loaded, counts.pending, counts.ai]);

  useEffect(() => {
    setDraft(null);
    setSelectedBox(null);
    setNaturalSize(null);
  }, [selected?.id]);

  // Pin the fallback selection (queue[0]) so later edits keep it on stage.
  useEffect(() => {
    if (selected && selected.id !== selectedId) setSelectedId(selected.id);
  }, [selected, selectedId]);

  useEffect(() => {
    const el = stageRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver((entries) => {
      const r = entries[0]?.contentRect;
      if (r) setStageSize({ w: r.width, h: r.height });
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [step]);

  /** Changing the filter starts again from the top of the new queue. */
  const changeFilter = (patch: Partial<DatasetQueueFilter>) => {
    setFilter((f) => ({ ...f, ...patch }));
    setSelectedId(null);
  };

  const replaceSample = (item: DatasetSampleItem) =>
    setSamples((prev) => prev.map((s) => (s.id === item.id ? item : s)));

  /** Id to land on once `id` leaves the visible queue (it changed tab, or was deleted). */
  const neighbourAfter = (id: string): string | null => {
    const idx = queue.findIndex((s) => s.id === id);
    if (idx < 0) return stepStickyDatasetQueue(queue, id, lastQueueIdxRef.current, 1);
    return queue[idx + 1]?.id ?? queue[idx - 1]?.id ?? null;
  };

  const patchSample = async (
    sample: DatasetSampleItem,
    patch: {
      annotations?: DatasetAnnotationBox[];
      verified?: boolean;
      verified_by?: "ai" | "manual";
      sample_kind?: string;
    }
  ): Promise<boolean> => {
    if (!isSafeDatasetSampleId(sample?.id)) return false;
    try {
      const res = await fetch(`/api/dataset/samples/${encodeURIComponent(sample.id)}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(patch),
      });
      const data = await res.json().catch(() => null);
      if (!isMountedRef.current) return false;
      if (res.ok && data) {
        replaceSample(data.sample || data);
        void loadDatasetState();
        return true;
      }
      notify(data?.message || t("Falha ao atualizar a amostra", "Failed to update the sample"));
    } catch {
      if (isMountedRef.current) notify(t("Erro de rede ao atualizar a amostra", "Network error updating the sample"));
    }
    return false;
  };

  const runAutoAnnotate = async (sampleId: string) => {
    if (!isSafeDatasetSampleId(sampleId)) return;
    setBusyAction(`auto-${sampleId}`);
    try {
      const res = await fetch(`/api/dataset/samples/${encodeURIComponent(sampleId)}/auto-annotate`, {
        method: "POST",
        credentials: "include",
      });
      const data = await res.json().catch(() => null);
      if (!isMountedRef.current) return;
      if (res.ok && data) {
        const item: DatasetSampleItem = data.sample || data;
        replaceSample(item);
        notify(
          t(
            `IA verificou ${sampleId} com ${(item.annotations || []).length} boxes`,
            `AI verified ${sampleId} with ${(item.annotations || []).length} boxes`
          )
        );
        void loadDatasetState();
      } else {
        notify(data?.message || t("A anotação por IA falhou", "AI annotation failed"));
      }
    } catch {
      if (isMountedRef.current) notify(t("Erro de rede na anotação por IA", "Network error during AI annotation"));
    } finally {
      if (isMountedRef.current) setBusyAction(null);
    }
  };

  const stopBatch = () => {
    batchCancelRef.current = true;
    if (batchAbortRef.current) {
      try {
        batchAbortRef.current.abort();
      } catch {
        // ignore
      }
      batchAbortRef.current = null;
    }
  };

  const runBatch = async (mode: "pending" | "all_positives") => {
    if (batchRunningRef.current) return;
    batchRunningRef.current = true;
    setBusyAction("batch-auto");
    batchCancelRef.current = false;
    try {
      let currentList = samples;
      try {
        const listController = typeof AbortController !== "undefined" ? new AbortController() : null;
        batchAbortRef.current = listController;
        const listRes = await fetch("/api/dataset/samples", {
          credentials: "include",
          ...(listController ? { signal: listController.signal } : {}),
        });
        if (listRes.ok) {
          const rows = await readSampleList(listRes);
          if (rows.length > 0) currentList = rows;
        }
      } catch {
        // fall back to the in-memory list
      } finally {
        batchAbortRef.current = null;
      }
      if (!isMountedRef.current || batchCancelRef.current) return;
      const candidates = selectBatchAutoAnnotateCandidates(currentList, mode);
      if (candidates.length === 0) {
        if (isMountedRef.current) {
          notify(t("Nada para anotar com IA", "Nothing for the AI to annotate"));
        }
        return;
      }
      const startedAtMs = Date.now();
      let completed = 0;
      let succeeded = 0;
      let failed = 0;
      let boxesGenerated = 0;
      const snapshot = (active: boolean, current: DatasetSampleItem | null): BatchAutoAnnotateProgress => ({
        active,
        mode,
        total: candidates.length,
        completed,
        succeeded,
        failed,
        boxesGenerated,
        currentSampleId: current?.id ?? null,
        currentCamera: current?.camera ?? null,
        startedAtMs,
        finishedAtMs: active ? null : Date.now(),
        cancelled: batchCancelRef.current,
      });
      setBatchProgress(snapshot(true, candidates[0]));
      for (const cand of candidates) {
        if (!isMountedRef.current || batchCancelRef.current) break;
        if (!isSafeDatasetSampleId(cand.id)) {
          failed += 1;
          completed += 1;
          continue;
        }
        setBatchProgress(snapshot(true, cand));
        try {
          const itemController = typeof AbortController !== "undefined" ? new AbortController() : null;
          batchAbortRef.current = itemController;
          const res = await fetch(`/api/dataset/samples/${encodeURIComponent(cand.id)}/auto-annotate`, {
            method: "POST",
            credentials: "include",
            ...(itemController ? { signal: itemController.signal } : {}),
          });
          batchAbortRef.current = null;
          if (!isMountedRef.current || batchCancelRef.current) break;
          const data = await res.json().catch(() => null);
          if (!isMountedRef.current || batchCancelRef.current) break;
          if (res.ok && data) {
            const item: DatasetSampleItem = data.sample || data;
            boxesGenerated += Array.isArray(item.annotations) ? item.annotations.length : 0;
            succeeded += 1;
            if (isMountedRef.current) replaceSample(item);
          } else {
            failed += 1;
          }
        } catch (err) {
          batchAbortRef.current = null;
          if (
            !isMountedRef.current ||
            batchCancelRef.current ||
            (err as { name?: string })?.name === "AbortError"
          ) {
            break;
          }
          failed += 1;
        }
        completed += 1;
      }
      if (!isMountedRef.current) return;
      setBatchProgress(snapshot(false, null));
      notify(
        batchCancelRef.current
          ? t(
              `Lote interrompido em ${completed}/${candidates.length} (${succeeded} verificadas pela IA)`,
              `Batch stopped at ${completed}/${candidates.length} (${succeeded} verified by AI)`
            )
          : t(
              `IA verificou ${succeeded}/${candidates.length} amostras (+${boxesGenerated} boxes)`,
              `AI verified ${succeeded}/${candidates.length} samples (+${boxesGenerated} boxes)`
            )
      );
      void loadDatasetState();
    } finally {
      batchRunningRef.current = false;
      batchAbortRef.current = null;
      if (isMountedRef.current) setBusyAction(null);
    }
  };

  const confirmManual = async (sample: DatasetSampleItem) => {
    const next = neighbourAfter(sample.id);
    const ok = await patchSample(sample, { verified: true, verified_by: "manual" });
    if (ok && filter.tab !== "manual" && filter.tab !== "all") setSelectedId(next);
    else if (ok) setSelectedId(stepQueue(1));
  };

  const markBackground = async (sample: DatasetSampleItem) => {
    const next = neighbourAfter(sample.id);
    const ok = await patchSample(sample, {
      sample_kind: "hard_negative",
      annotations: [],
      verified: true,
      verified_by: "manual",
    });
    if (ok) setSelectedId(next);
  };

  const discardSample = async (sample: DatasetSampleItem) => {
    if (!isSafeDatasetSampleId(sample?.id)) return;
    if (!window.confirm(t("Descartar esta amostra do dataset?", "Discard this sample from the dataset?"))) return;
    const next = neighbourAfter(sample.id);
    try {
      const res = await fetch(`/api/dataset/samples/${encodeURIComponent(sample.id)}`, {
        method: "DELETE",
        credentials: "include",
      });
      if (!isMountedRef.current) return;
      if (res.ok) {
        setSamples((prev) => prev.filter((s) => s.id !== sample.id));
        setSelectedId(next);
        void loadDatasetState();
      } else {
        const err = await res.json().catch(() => null);
        notify(err?.message || t("Falha ao descartar a amostra", "Failed to discard the sample"));
      }
    } catch {
      if (isMountedRef.current) notify(t("Erro de rede ao descartar", "Network error while discarding"));
    }
  };

  const setBoxes = (sample: DatasetSampleItem, annotations: DatasetAnnotationBox[]) =>
    void patchSample(sample, { annotations });

  const removeBox = (sample: DatasetSampleItem, idx: number) => {
    setSelectedBox(null);
    setBoxes(
      sample,
      (sample.annotations || []).filter((_, i) => i !== idx)
    );
  };

  const relabelBox = (sample: DatasetSampleItem, idx: number, classId: number) => {
    const safeClassId =
      Number.isInteger(classId) && classId >= 0 && classId < NINJANVR_SECURITY_CLASSES.length
        ? classId
        : 0;
    const label = NINJANVR_SECURITY_CLASSES[safeClassId] ?? "person";
    setBoxes(
      sample,
      (sample.annotations || []).map((ann, i) =>
        i === idx ? { ...ann, class_id: safeClassId, label, source: "manual" } : ann
      )
    );
  };

  const goStep = (next: DatasetStudioStep) => {
    setStep(next);
    setDraft(null);
  };

  const runNextAction = () => {
    switch (nextAction.kind) {
      case "batch-annotate":
        void runBatch("pending");
        break;
      case "goto-readiness":
        goStep("readiness");
        break;
      case "goto-train":
        goStep("train");
        break;
      case "collect":
        goStep("annotate");
        notify(
          t(
            "Adicione frames pelo Explore: “Add to Training Dataset” ou marcando falso positivo.",
            "Add frames from Explore: “Add to Training Dataset” or by marking a false positive."
          )
        );
        break;
    }
  };

  // Keyboard shortcuts for the annotate workbench.
  useEffect(() => {
    if (step !== "annotate") return;
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.ctrlKey || e.metaKey || e.altKey || isTypingTarget(e.target)) return;
      const key = e.key.toLowerCase();
      if (key === "escape") {
        setDraft(null);
        setSelectedBox(null);
        return;
      }
      const quick = Number(e.key);
      if (Number.isInteger(quick) && quick >= 1 && quick <= QUICK_CLASS_IDS.length) {
        e.preventDefault();
        setActiveClassId(QUICK_CLASS_IDS[quick - 1]);
        return;
      }
      if (key === "j" || key === "arrowdown") {
        e.preventDefault();
        setSelectedId(stepQueue(1));
        return;
      }
      if (key === "k" || key === "arrowup") {
        e.preventDefault();
        setSelectedId(stepQueue(-1));
        return;
      }
      if (!selected || busyAction) return;
      if (key === "v") {
        e.preventDefault();
        if (canConfirmSample(selected)) void confirmManual(selected);
      } else if (key === "a") {
        e.preventDefault();
        void runAutoAnnotate(selected.id);
      } else if (key === "n") {
        e.preventDefault();
        void markBackground(selected);
      } else if ((key === "delete" || key === "backspace") && selectedBox !== null) {
        e.preventDefault();
        removeBox(selected, selectedBox);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  // ---- drawing -----------------------------------------------------------
  const pointFromEvent = (e: React.PointerEvent<HTMLDivElement>): Point => {
    const rect = e.currentTarget.getBoundingClientRect();
    if (
      !rect ||
      !Number.isFinite(rect.width) ||
      !Number.isFinite(rect.height) ||
      rect.width <= 0 ||
      rect.height <= 0 ||
      !Number.isFinite(e.clientX) ||
      !Number.isFinite(e.clientY)
    ) {
      return [0, 0];
    }
    return [
      Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width)),
      Math.max(0, Math.min(1, (e.clientY - rect.top) / rect.height)),
    ];
  };

  const onFramePointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!selected || e.button !== 0) return;
    try {
      e.currentTarget.setPointerCapture(e.pointerId);
    } catch {
      // ignore synthetic or released pointer events
    }
    const p = pointFromEvent(e);
    setSelectedBox(null);
    setDraft({ start: p, end: p });
  };

  const onFramePointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!draft) return;
    setDraft({ start: draft.start, end: pointFromEvent(e) });
  };

  const onFramePointerUp = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!draft || !selected) return;
    const box = normalizeDragBox(draft.start, pointFromEvent(e));
    setDraft(null);
    if (!box) return;
    const yolo = xyxyToYoloNormalized(box);
    const next: DatasetAnnotationBox = {
      class_id: activeClassId,
      label: NINJANVR_SECURITY_CLASSES[activeClassId] ?? "person",
      ...yolo,
      score: 1,
      source: "manual",
    };
    setBoxes(selected, [...(selected.annotations || []), next]);
  };

  // ---- detector runtime ----------------------------------------------------
  const loadCatalog = useCallback(async () => {
    try {
      const res = await fetch("/api/detectors/catalog", { credentials: "include" });
      if (!res.ok) return;
      const data = (await res.json()) as DetectorRuntimeCatalog;
      if (isMountedRef.current) setCatalog(data);
    } catch {
      // keep the last known state
    }
  }, []);

  useEffect(() => {
    if (step === "train") void loadCatalog();
  }, [step, loadCatalog]);

  // ---- upload --------------------------------------------------------------
  const stageFile = (file: File | undefined) => {
    if (uploadFileReaderRef.current) {
      try {
        uploadFileReaderRef.current.abort();
      } catch {
        // ignore
      }
      uploadFileReaderRef.current = null;
    }
    if (!file) return;
    if (!/\.(onnx|xml|bin)$/i.test(file.name)) {
      notify(t("Use um arquivo .onnx ou .xml", "Use an .onnx or .xml file"));
      return;
    }
    const reader = new FileReader();
    uploadFileReaderRef.current = reader;
    reader.onload = () => {
      uploadFileReaderRef.current = null;
      if (typeof reader.result === "string" && isMountedRef.current) {
        setUpload({ name: file.name, size: file.size, base64: reader.result });
        setUploadFilename(file.name);
      }
    };
    reader.onerror = () => {
      uploadFileReaderRef.current = null;
    };
    reader.onabort = () => {
      uploadFileReaderRef.current = null;
    };
    reader.readAsDataURL(file);
  };

  const deployModel = async () => {
    if (!upload) return;
    const [w, h] = uploadDims.split("x").map(Number);
    setBusyAction("upload-model");
    setDeployOutcome(null);
    try {
      const payload = buildModelUploadPayload({
        filename: uploadFilename,
        modelBase64: upload.base64,
        width: w,
        height: h,
      });
      const res = await fetch("/api/detectors/models/upload", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => null);
      if (!isMountedRef.current) return;
      const outcome = parseModelDeployResponse(res.status, data, Date.now());
      setDeployOutcome(outcome);
      if (data && typeof data === "object" && "catalog" in data && data.catalog) {
        setCatalog(data.catalog as DetectorRuntimeCatalog);
      }
      if (outcome.ok) {
        setUpload(null);
        const d = data as Record<string, unknown>;
        onModelDeployed?.({
          path: outcome.modelPath,
          labelmap_path: typeof d.labelmap_path === "string" ? d.labelmap_path : undefined,
          width: outcome.width,
          height: outcome.height,
          model_type: typeof d.model_type === "string" ? d.model_type : undefined,
          input_tensor: typeof d.input_tensor === "string" ? d.input_tensor : undefined,
          input_pixel_format: typeof d.input_pixel_format === "string" ? d.input_pixel_format : undefined,
        });
        notify(
          outcome.loaded
            ? t(`Modelo '${outcome.filename}' ativo nas câmeras`, `Model '${outcome.filename}' is live on the cameras`)
            : t("Modelo salvo, mas o detector não carregou", "Model saved, but the detector did not load it")
        );
      } else {
        notify(
          outcome.rolledBack
            ? t("O modelo não carregou. O anterior continua ativo.", "The model did not load. The previous one is still active.")
            : outcome.message || t("Falha ao implantar o modelo", "Model deploy failed")
        );
      }
      void loadCatalog();
    } catch {
      if (!isMountedRef.current) return;
      const failed: ModelDeployOutcome = {
        ok: false,
        loaded: false,
        persisted: false,
        rolledBack: false,
        filename: uploadFilename,
        modelPath: "",
        width: 0,
        height: 0,
        at: Date.now(),
        message: t("Erro de rede ao enviar o modelo. Nada foi alterado.", "Network error uploading the model. Nothing changed."),
      };
      setDeployOutcome(failed);
      notify(failed.message);
    } finally {
      if (isMountedRef.current) setBusyAction(null);
    }
  };

  const nvrOrigin =
    typeof window !== "undefined" && window.location?.origin ? window.location.origin : "http://localhost:5000";
  const cliCommand = buildLocalFinetuneCliCommand({
    nvrUrl: nvrOrigin,
    imgsz: modelCfg.width === 320 ? 320 : 640,
  });

  // ---- render helpers -----------------------------------------------------
  const provBadge = (s: DatasetSampleItem) => {
    const status = datasetQueueStatus(s);
    const label =
      status === "ai"
        ? "IA ✓"
        : status === "manual"
        ? "MANUAL ✓"
        : status === "pending"
        ? t("PENDENTE", "PENDING")
        : t("FUNDO", "BG");
    const cls = status === "background" ? "" : status;
    return <span className={`nvr-ds-prov ${cls}`}>{label}</span>;
  };

  const tabs: Array<{ id: DatasetQueueTab; label: string; count: number }> = [
    { id: "pending", label: t("PEND.", "PEND."), count: counts.pending },
    { id: "ai", label: "IA ✓", count: counts.ai },
    { id: "manual", label: "MANUAL ✓", count: counts.manual },
    { id: "all", label: t("TODAS", "ALL"), count: counts.all },
  ];

  const renderHeader = () => (
    <div className="nvr-ds-panel" style={{ padding: "12px 14px", gap: 10 }}>
      <div className="nvr-ds-head-top">
        <div className="nvr-ds-head-title">
          <h2 className="nvr-ds-panel-title">AI Dataset Studio</h2>
          <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
            {counts.all} frames · {summary?.total_annotations ?? 0} boxes · {cameras.length}{" "}
            {t("câmeras", "cameras")}
          </span>
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
          <button
            type="button"
            className="nvr-btn mono sm"
            onClick={() => void loadDatasetState()}
            aria-label={t("Recarregar dataset", "Reload dataset")}
          >
            {t("Atualizar", "Refresh")}
          </button>
          <span style={{ fontSize: "var(--text-sm)", color: "var(--text-secondary)" }}>
            {t("Próximo passo:", "Next step:")}
          </span>
          {nextAction.kind === "goto-train" && step === "train" ? (
            <a
              href="/api/dataset/export.zip"
              download="ninjanvr-yolo-dataset.zip"
              className="nvr-btn primary mono"
            >
              {nextAction.label}
            </a>
          ) : (
            <button
              type="button"
              className="nvr-btn primary mono"
              disabled={batchRunning}
              onClick={runNextAction}
            >
              {nextAction.kind === "batch-annotate" && <Sparkles size={12} />}
              {nextAction.label}
            </button>
          )}
        </div>
      </div>
      <nav className="nvr-ds-rail" aria-label={t("Etapas do dataset", "Dataset steps")}>
        {pipeline.map((p) => {
          const current = p.target === step;
          return (
            <button
              key={p.id}
              type="button"
              className={`nvr-ds-step is-${p.tone}`}
              aria-current={current ? "step" : undefined}
              onClick={() => goStep(p.target)}
            >
              <span className="nvr-ds-step-title">
                {p.index} · {p.title}
                {p.tone === "done" ? " ✓" : ""}
              </span>
              <span>{p.detail}</span>
            </button>
          );
        })}
      </nav>
    </div>
  );

  const renderStrip = () => (
    <section className="nvr-ds-strip" aria-live="polite">
      <div style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap", minWidth: 0 }}>
        <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-accent-text)" }}>
          TEACHER AI
        </span>
        {batchProgress ? (
          <span>
            {batchProgress.active
              ? t(
                  `Verificando ${batchProgress.completed}/${batchProgress.total} (${batchMetrics.pct}%)`,
                  `Verifying ${batchProgress.completed}/${batchProgress.total} (${batchMetrics.pct}%)`
                )
              : batchProgress.cancelled
              ? t("Lote interrompido", "Batch stopped")
              : t("Lote concluído", "Batch complete")}
            {" · "}
            <span className="mono">
              OK {batchProgress.succeeded} · ERR {batchProgress.failed} · +{batchProgress.boxesGenerated} boxes ·{" "}
              {batchMetrics.elapsedSec}s{batchMetrics.etaSec !== null ? ` · ETA ~${batchMetrics.etaSec}s` : ""}
            </span>
          </span>
        ) : (
          <span>
            {t(
              "A IA anota e verifica em lote. Conferir à mão é opcional: comece pelas de menor confiança.",
              "The AI annotates and verifies in batch. Checking by hand is optional: start with the lowest confidence."
            )}
          </span>
        )}
      </div>
      <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
        <span className="mono" style={{ fontSize: "var(--text-xs)" }}>
          {t(
            `${counts.pending} pendentes · ${counts.ai + counts.manual + counts.background} verificadas (${counts.ai} IA · ${counts.manual} manual · ${counts.background} fundo)`,
            `${counts.pending} pending · ${counts.ai + counts.manual + counts.background} verified (${counts.ai} AI · ${counts.manual} manual · ${counts.background} bg)`
          )}
        </span>
        {batchProgress?.active ? (
          <button type="button" className="nvr-btn danger mono sm" onClick={stopBatch}>
            {t("Parar", "Stop")}
          </button>
        ) : batchProgress ? (
          <button type="button" className="nvr-btn mono sm" onClick={() => setBatchProgress(null)}>
            {t("Fechar", "Dismiss")}
          </button>
        ) : counts.pending === 0 && counts.ai > 0 ? (
          <button
            type="button"
            className="nvr-btn mono sm"
            disabled={batchRunning}
            title={t("Amostras conferidas à mão não são alteradas", "Hand-checked samples are left untouched")}
            onClick={() => void runBatch("all_positives")}
          >
            {t(`Refazer ${counts.ai} com IA`, `Redo ${counts.ai} with AI`)}
          </button>
        ) : null}
      </div>
      {batchProgress && (
        <div className="nvr-util-bar" style={{ width: "100%", height: 4 }}>
          <div
            className="nvr-util-fill"
            style={{
              width: `${batchMetrics.pct}%`,
              background: batchProgress.active ? "var(--signal-accent)" : "var(--signal-live)",
            }}
          />
        </div>
      )}
    </section>
  );

  const renderQueue = () => (
    <aside className="nvr-ds-panel" aria-label={t("Fila de amostras", "Sample queue")}>
      <div className="nvr-segmented nvr-ds-tabs" role="tablist" aria-label={t("Filtrar por verificação", "Filter by verification")}>
        {tabs.map((tab) => (
          <button
            key={tab.id}
            type="button"
            role="tab"
            aria-selected={filter.tab === tab.id}
            className={`nvr-btn mono sm ${filter.tab === tab.id ? "active" : ""}`}
            onClick={() => {
              tabInitializedRef.current = true;
              changeFilter({ tab: tab.id });
            }}
          >
            {tab.label} {tab.count}
          </button>
        ))}
      </div>
      <div className="nvr-ds-filters">
        <label>
          <span>{t("Câmera", "Camera")}</span>
          <select
            className="nvr-select mono sm"
            value={filter.camera}
            onChange={(e) => changeFilter({ camera: e.target.value })}
          >
            <option value="all">{t("Todas", "All")}</option>
            {cameras.map(([cam, n]) => (
              <option key={cam} value={cam}>
                {cam} ({n})
              </option>
            ))}
          </select>
        </label>
        <label>
          <span>{t("Tipo", "Type")}</span>
          <select
            className="nvr-select mono sm"
            value={filter.kind}
            onChange={(e) => {
              const v = e.target.value;
              const kind: DatasetQueueKind = v === "objects" || v === "background" ? v : "all";
              changeFilter({ kind });
            }}
          >
            <option value="all">{t("Todos", "All")}</option>
            <option value="objects">
              {t("Com objetos", "With objects")} ({counts.objects})
            </option>
            <option value="background">
              {t("Fundo", "Background")} ({counts.background})
            </option>
          </select>
        </label>
      </div>
      <span className="nvr-ds-hint">
        {filter.tab === "ai"
          ? t("Verificadas pela IA, menor confiança primeiro.", "AI-verified, lowest confidence first.")
          : t("Mais recentes primeiro.", "Newest first.")}
      </span>
      {queue.length === 0 ? (
        <div className="nvr-ds-hint" style={{ padding: 10, border: "1px solid var(--border-divider)", borderRadius: 3 }}>
          {counts.all === 0
            ? t(
                "Dataset vazio. No Explore, use “Add to Training Dataset” ou marque falsos positivos.",
                "Empty dataset. In Explore, use “Add to Training Dataset” or mark false positives."
              )
            : t("Nenhuma amostra neste filtro.", "No samples match this filter.")}
        </div>
      ) : (
        <ul className="nvr-ds-queue">
          {queue.map((s) => {
            const minScore = minAnnotationScore(s);
            const boxes = (s.annotations || []).length;
            return (
              <li key={s.id}>
                <button
                  type="button"
                  className="nvr-ds-qitem"
                  aria-current={selected?.id === s.id ? "true" : undefined}
                  onClick={() => setSelectedId(s.id)}
                >
                  {isSafeDatasetSampleId(s.id) && (
                    <img
                      className="nvr-ds-thumb"
                      src={`/api/dataset/samples/${encodeURIComponent(s.id)}/image.webp`}
                      alt=""
                      loading="lazy"
                    />
                  )}
                  <span className="nvr-ds-qmeta">
                    <span className="nvr-ds-qtitle">
                      {s.camera.toUpperCase()} · {formatTs(s.created_at)}
                    </span>
                    <span className="nvr-ds-qsub">
                      {s.sample_kind === "hard_negative"
                        ? t("sem objetos", "no objects")
                        : `${boxes} ${boxes === 1 ? "box" : "boxes"}`}
                      {minScore !== null && s.sample_kind !== "hard_negative" && (
                        <>
                          {" · "}
                          {t("menor conf.", "min conf.")}{" "}
                          <span className={minScore < LOW_CONFIDENCE ? "nvr-ds-low" : undefined}>
                            {minScore.toFixed(2)}
                          </span>
                        </>
                      )}
                    </span>
                  </span>
                  {provBadge(s)}
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </aside>
  );

  const renderStage = () => {
    const mediaW = selected?.width || naturalSize?.w || 16;
    const mediaH = selected?.height || naturalSize?.h || 9;
    const frame = computeContainedBox(stageSize.w, stageSize.h, mediaW, mediaH);
    const idx = selectedQueueIdx;
    const draftBox = draft ? normalizeDragBox(draft.start, draft.end, 0) : null;
    return (
      <main className="nvr-ds-panel" style={{ minWidth: 0 }}>
        <div className="nvr-ds-classbar">
          <div className="nvr-ds-classes" role="radiogroup" aria-label={t("Classe para novas boxes", "Class for new boxes")}>
            {QUICK_CLASS_IDS.map((cid, i) => (
              <button
                key={cid}
                type="button"
                role="radio"
                aria-checked={activeClassId === cid}
                aria-keyshortcuts={String(i + 1)}
                className={`nvr-btn mono sm ${activeClassId === cid ? "active" : ""}`}
                onClick={() => setActiveClassId(cid)}
              >
                {NINJANVR_SECURITY_CLASSES[cid]} <kbd className="nvr-kbd">{i + 1}</kbd>
              </button>
            ))}
            <select
              className="nvr-select mono sm"
              aria-label={t("Outras classes", "Other classes")}
              value={(QUICK_CLASS_IDS as readonly number[]).includes(activeClassId) ? "" : String(activeClassId)}
              onChange={(e) => e.target.value !== "" && setActiveClassId(Number(e.target.value))}
              style={{ width: 150 }}
            >
              <option value="">{t("+ outras classes", "+ other classes")}</option>
              {NINJANVR_SECURITY_CLASSES.map((cls, cid) =>
                (QUICK_CLASS_IDS as readonly number[]).includes(cid) ? null : (
                  <option key={cls} value={cid}>
                    {cls}
                  </option>
                )
              )}
            </select>
          </div>
          <span className="nvr-ds-hint">
            {t("Arraste sobre a imagem para criar uma box da classe ativa", "Drag on the image to draw a box of the active class")}
          </span>
        </div>

        <div className="nvr-ds-stage" ref={stageRef}>
          {selected && isSafeDatasetSampleId(selected.id) ? (
            <div
              className="nvr-ds-frame"
              style={{ left: frame.left, top: frame.top, width: frame.width, height: frame.height }}
              onPointerDown={onFramePointerDown}
              onPointerMove={onFramePointerMove}
              onPointerUp={onFramePointerUp}
              onPointerCancel={() => setDraft(null)}
            >
              <img
                src={`/api/dataset/samples/${encodeURIComponent(selected.id)}/image.webp`}
                alt={t(`Frame da câmera ${selected.camera}`, `Frame from camera ${selected.camera}`)}
                draggable={false}
                onLoad={(e) =>
                  setNaturalSize({ w: e.currentTarget.naturalWidth, h: e.currentTarget.naturalHeight })
                }
              />
              {(selected.annotations || []).map((ann, i) => {
                const [x1, y1, x2, y2] = yoloNormalizedToXyxy(ann);
                const scoreVal =
                  typeof ann.score === "number" && Number.isFinite(ann.score) ? ann.score : null;
                const low = ann.source !== "manual" && scoreVal !== null && scoreVal < LOW_CONFIDENCE;
                const label = NINJANVR_SECURITY_CLASSES[ann.class_id] ?? ann.label;
                return (
                  <div
                    key={`${ann.class_id}-${i}`}
                    className={`nvr-ds-box ${low ? "is-low" : ""} ${selectedBox === i ? "is-selected" : ""} ${y1 < 0.06 ? "at-top" : ""}`}
                    style={{
                      left: `${x1 * 100}%`,
                      top: `${y1 * 100}%`,
                      width: `${(x2 - x1) * 100}%`,
                      height: `${(y2 - y1) * 100}%`,
                    }}
                  >
                    <span className="nvr-ds-box-label">
                      {label}
                      {ann.source !== "manual" && scoreVal !== null
                        ? ` ${scoreVal.toFixed(2)}`
                        : ""}
                    </span>
                  </div>
                );
              })}
              {draftBox && (
                <div
                  className="nvr-ds-box is-draft"
                  style={{
                    left: `${draftBox[0] * 100}%`,
                    top: `${draftBox[1] * 100}%`,
                    width: `${(draftBox[2] - draftBox[0]) * 100}%`,
                    height: `${(draftBox[3] - draftBox[1]) * 100}%`,
                  }}
                >
                  <span className="nvr-ds-box-label">{NINJANVR_SECURITY_CLASSES[activeClassId]}</span>
                </div>
              )}
            </div>
          ) : (
            <div className="nvr-ds-stage-empty">
              <span>{t("Selecione uma amostra na fila.", "Pick a sample from the queue.")}</span>
            </div>
          )}
        </div>

        <div className="nvr-ds-navbar">
          <button
            type="button"
            className="nvr-btn mono sm"
            disabled={queue.length === 0 || (idx >= 0 ? idx <= 0 : lastQueueIdxRef.current <= 0)}
            onClick={() => setSelectedId(stepDatasetQueue(queue, selected?.id ?? null, -1))}
          >
            ← {t("Anterior", "Previous")} <kbd className="nvr-kbd">K</kbd>
          </button>
          <span className="mono" style={{ fontSize: "var(--text-sm)", color: "var(--text-secondary)" }}>
            {idx >= 0
              ? `${idx + 1} / ${queue.length}`
              : selected
              ? t(`fora deste filtro · ${queue.length} na fila`, `outside this filter · ${queue.length} queued`)
              : `0 / ${queue.length}`}
          </span>
          <button
            type="button"
            className="nvr-btn mono sm"
            disabled={queue.length === 0 || (idx >= 0 && idx >= queue.length - 1)}
            onClick={() => setSelectedId(stepDatasetQueue(queue, selected?.id ?? null, 1))}
          >
            {t("Próxima", "Next")} <kbd className="nvr-kbd">J</kbd> →
          </button>
        </div>
      </main>
    );
  };

  const renderInspector = () => {
    if (!selected) {
      return (
        <aside className="nvr-ds-panel nvr-ds-inspector" aria-label={t("Inspetor", "Inspector")}>
          <span className="nvr-ds-hint">{t("Nenhuma amostra selecionada.", "No sample selected.")}</span>
        </aside>
      );
    }
    const status = datasetQueueStatus(selected);
    const anns = selected.annotations || [];
    const hasLow = anns.some(
      (a) =>
        a.source !== "manual" &&
        typeof a.score === "number" &&
        Number.isFinite(a.score) &&
        a.score < LOW_CONFIDENCE
    );
    const busy = Boolean(busyAction);
    const canConfirm = canConfirmSample(selected);
    return (
      <aside className="nvr-ds-panel nvr-ds-inspector" aria-label={t("Inspetor da amostra", "Sample inspector")} style={{ gap: 12, padding: 12 }}>
        <section style={{ display: "flex", flexDirection: "column", gap: 6 }}>
          <h3 className="nvr-ds-label">{t("AMOSTRA", "SAMPLE")}</h3>
          <dl className="nvr-ds-meta">
            <dt>{t("Câmera", "Camera")}</dt>
            <dd className="mono">{selected.camera}</dd>
            <dt>{t("Capturada", "Captured")}</dt>
            <dd className="mono">{formatDateTime(selected.created_at)}</dd>
            <dt>Split</dt>
            <dd className="mono">
              {selected.split} <span style={{ color: "var(--text-muted)" }}>{t("(automático)", "(automatic)")}</span>
            </dd>
            <dt>{t("IR noturno", "Night IR")}</dt>
            <dd>{selected.night_mode ? t("Sim", "Yes") : t("Não", "No")}</dd>
            <dt>{t("Verificação", "Verification")}</dt>
            <dd>
              {provBadge(selected)}{" "}
              <span style={{ color: "var(--text-muted)" }}>
                {status === "ai"
                  ? "Teacher AI"
                  : status === "manual"
                  ? t("conferida à mão", "checked by hand")
                  : status === "pending"
                  ? t("aguardando IA", "awaiting AI")
                  : t("exemplo de fundo", "background example")}
              </span>
            </dd>
          </dl>
        </section>

        {selected.sample_kind !== "hard_negative" && (
          <section style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            <h3 className="nvr-ds-label">BOXES ({anns.length})</h3>
            {anns.length === 0 ? (
              <span className="nvr-ds-hint">
                {t("Sem boxes. Rode a IA ou desenhe na imagem.", "No boxes. Run the AI or draw on the image.")}
              </span>
            ) : (
              <ul className="nvr-ds-boxes">
                {anns.map((ann, i) => {
                  const manual = ann.source === "manual";
                  const scoreVal =
                    typeof ann.score === "number" && Number.isFinite(ann.score) ? ann.score : null;
                  const low = !manual && scoreVal !== null && scoreVal < LOW_CONFIDENCE;
                  return (
                    <li
                      key={`${ann.class_id}-${i}`}
                      className={`nvr-ds-boxrow ${low ? "is-low" : ""} ${selectedBox === i ? "is-selected" : ""}`}
                      onMouseEnter={() => setSelectedBox(i)}
                      onFocus={() => setSelectedBox(i)}
                    >
                      <select
                        className="nvr-select mono sm"
                        aria-label={t(`Classe da box ${i + 1}`, `Class of box ${i + 1}`)}
                        value={ann.class_id}
                        disabled={busy}
                        onChange={(e) => relabelBox(selected, i, Number(e.target.value))}
                      >
                        {NINJANVR_SECURITY_CLASSES.map((cls, cid) => (
                          <option key={cls} value={cid}>
                            {cls}
                          </option>
                        ))}
                      </select>
                      <span className={`nvr-ds-score ${low ? "is-low" : ""} ${manual ? "is-manual" : ""}`}>
                        {manual ? t("manual", "manual") : `IA ${scoreVal !== null ? scoreVal.toFixed(2) : "—"}`}
                      </span>
                      <button
                        type="button"
                        className="nvr-btn danger mono sm"
                        aria-label={t(`Remover box ${i + 1}`, `Remove box ${i + 1}`)}
                        disabled={busy}
                        onClick={() => removeBox(selected, i)}
                      >
                        <X size={12} />
                      </button>
                    </li>
                  );
                })}
              </ul>
            )}
            {hasLow && (
              <span className="nvr-ds-hint">
                {t(
                  "Há box com confiança baixa. Vale conferir: editar qualquer box muda a verificação para manual.",
                  "Some boxes have low confidence. Worth a look: editing any box makes the verification manual."
                )}
              </span>
            )}
          </section>
        )}

        <section className="nvr-ds-actions" style={{ marginTop: "auto" }}>
          <button
            type="button"
            className="nvr-btn success mono is-primary-action"
            disabled={busy || !canConfirm}
            title={
              canConfirm
                ? undefined
                : t("Desenhe uma box, rode a IA ou marque “Sem objetos”", "Draw a box, run the AI or mark “No objects”")
            }
            onClick={() => void confirmManual(selected)}
          >
            {t("Confirmar e próxima", "Confirm and next")} <kbd className="nvr-kbd">V</kbd>
          </button>
          <div className="nvr-ds-actions-row">
            <button
              type="button"
              className="nvr-btn mono sm"
              disabled={busy}
              onClick={() => void runAutoAnnotate(selected.id)}
            >
              <Sparkles size={12} />
              {busyAction === `auto-${selected.id}` ? t("Anotando…", "Annotating…") : t("Refazer com IA", "Redo with AI")}{" "}
              <kbd className="nvr-kbd">A</kbd>
            </button>
            <button
              type="button"
              className="nvr-btn mono sm"
              disabled={busy || selected.sample_kind === "hard_negative"}
              onClick={() => void markBackground(selected)}
            >
              {t("Sem objetos", "No objects")} <kbd className="nvr-kbd">N</kbd>
            </button>
          </div>
          <button type="button" className="nvr-btn danger mono sm" disabled={busy} onClick={() => void discardSample(selected)}>
            <Trash2 size={12} />
            {t("Descartar amostra", "Discard sample")}
          </button>
          <p className="nvr-ds-hint" style={{ margin: "4px 0 0" }}>
            {t(
              "Confirmar marca a amostra como MANUAL ✓. “Sem objetos” vira exemplo de fundo (hard negative) e ensina o modelo a não disparar nessa cena.",
              "Confirm marks the sample MANUAL ✓. “No objects” turns it into a background example (hard negative) that teaches the model not to fire on this scene."
            )}
          </p>
        </section>
      </aside>
    );
  };

  const checkIcon = (c: ReadinessCheck) =>
    c.severity === "ok" ? "✓" : c.severity === "warn" ? "!" : c.severity === "block" ? "×" : "i";

  const renderReadiness = () => {
    const camRows = Object.entries(summary?.by_camera ?? {})
      .map(
        ([cam, rawN]) =>
          [
            cam,
            Number.isFinite(Number(rawN)) && Number(rawN) > 0 ? Math.floor(Number(rawN)) : 0,
          ] as [string, number]
      )
      .sort((a, b) => b[1] - a[1]);
    const camMax = camRows.reduce((m, [, n]) => Math.max(m, n), 0);
    const classRows = datasetClassCoverage(summary);
    const learned = classRows.filter((r) => r.count >= DATASET_MIN_BOXES_PER_CLASS);
    const weak = classRows.filter((r) => r.count < DATASET_MIN_BOXES_PER_CLASS);
    const verdictText =
      report.verdict === "ready"
        ? t("PRONTO PARA TREINAR", "READY TO TRAIN")
        : report.verdict === "warnings"
        ? t("PODE TREINAR, COM RESSALVAS", "CAN TRAIN, WITH CAVEATS")
        : t("AINDA NÃO DÁ PARA TREINAR", "NOT READY TO TRAIN");
    const verdictTone =
      report.verdict === "ready"
        ? "var(--signal-live-border)"
        : report.verdict === "warnings"
        ? "var(--signal-motion-border)"
        : "var(--signal-alert-border)";
    return (
      <div className="nvr-ds-readiness">
        <section className="nvr-ds-panel nvr-ds-checks-panel" aria-labelledby="ds-checks-h">
          <div className="nvr-ds-panel-head">
            <h3 id="ds-checks-h" className="nvr-ds-panel-title">
              {t("O dataset está pronto para treinar?", "Is the dataset ready to train?")}
            </h3>
            <span className="nvr-ds-hint">{t("Cada item diz o que falta e onde resolver.", "Each item says what is missing and where to fix it.")}</span>
          </div>
          <ul className="nvr-ds-checks">
            {report.checks.map((c) => (
              <li key={c.id} className={`nvr-ds-check is-${c.severity}`}>
                <span className={`nvr-ds-check-icon ${c.severity}`} aria-label={c.severity}>
                  {checkIcon(c)}
                </span>
                <div className="nvr-ds-check-body">
                  <strong>{c.title}</strong>
                  <span>{c.detail}</span>
                  {c.id === "cameras" && camRows.length > 0 && (
                    <div className="nvr-ds-bars">
                      {camRows.map(([cam, n]) => {
                        const thin = n < DATASET_MIN_PER_CAMERA;
                        return (
                          <React.Fragment key={cam}>
                            <span className={thin ? "is-thin" : undefined}>{cam}</span>
                            <span className={`nvr-ds-bar ${thin ? "is-thin" : ""}`} aria-hidden="true">
                              <span style={{ width: `${camMax > 0 ? (n / camMax) * 100 : 0}%` }} />
                            </span>
                            <span className={thin ? "is-thin" : undefined} style={{ textAlign: "right" }}>
                              {n}
                            </span>
                          </React.Fragment>
                        );
                      })}
                    </div>
                  )}
                  {c.id === "classes" && classRows.length > 0 && (
                    <div className="nvr-ds-bars">
                      {learned.slice(0, 8).map((r) => (
                        <React.Fragment key={r.label}>
                          <span>{r.label}</span>
                          <span className="nvr-ds-bar" aria-hidden="true">
                            <span style={{ width: `${r.pct}%` }} />
                          </span>
                          <span style={{ textAlign: "right" }}>{r.count}</span>
                        </React.Fragment>
                      ))}
                      {weak.length > 0 && (
                        <>
                          <span style={{ color: "var(--text-muted)" }}>
                            +{weak.length} {t("classes", "classes")}
                          </span>
                          <span style={{ color: "var(--text-muted)", gridColumn: "2 / 4" }}>
                            {t(`abaixo de ${DATASET_MIN_BOXES_PER_CLASS} boxes`, `under ${DATASET_MIN_BOXES_PER_CLASS} boxes`)}:{" "}
                            {weak.map((r) => r.label).join(", ")}
                          </span>
                        </>
                      )}
                    </div>
                  )}
                  {c.id === "verification" && c.severity === "block" && (
                    <button
                      type="button"
                      className="nvr-btn mono sm"
                      style={{ alignSelf: "flex-start" }}
                      onClick={() => {
                        tabInitializedRef.current = true;
                        changeFilter({ tab: "pending" });
                        goStep("annotate");
                      }}
                    >
                      {t("Ir para as pendentes", "Go to pending")}
                    </button>
                  )}
                </div>
                <span className="nvr-ds-check-value">{c.value}</span>
              </li>
            ))}
          </ul>
        </section>

        <aside className="nvr-ds-verdict" style={{ display: "flex", flexDirection: "column", gap: 12 }}>
          <section className="nvr-ds-panel" style={{ borderColor: verdictTone, padding: 14, gap: 10 }}>
            <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700 }}>
              {verdictText}
            </span>
            <p style={{ margin: 0, fontSize: "var(--text-base)", lineHeight: 1.5 }}>
              {report.verdict === "ready"
                ? t("Tudo certo. Siga para o treino.", "All good. Move on to training.")
                : report.verdict === "warnings"
                ? t(
                    `Nada impede o treino. ${report.warnCount} aviso(s) podem afetar a qualidade.`,
                    `Nothing blocks training. ${report.warnCount} warning(s) may affect quality.`
                  )
                : t(
                    `${report.blockCount} bloqueio(s) precisam ser resolvidos antes.`,
                    `${report.blockCount} blocker(s) must be fixed first.`
                  )}
            </p>
            <button type="button" className="nvr-btn mono sm" onClick={() => goStep("annotate")}>
              {t("Voltar e coletar mais", "Go back and collect more")}
            </button>
          </section>
          <section className="nvr-ds-panel" style={{ padding: 14, gap: 6 }}>
            <h3 className="nvr-ds-label">{t("O QUE BLOQUEIA O TREINO", "WHAT BLOCKS TRAINING")}</h3>
            <ul style={{ margin: 0, paddingLeft: 16, fontSize: "var(--text-sm)", color: "var(--text-secondary)", lineHeight: 1.6 }}>
              <li>{t("Frames sem verificação (nem IA, nem manual)", "Unverified frames (neither AI nor manual)")}</li>
              <li>{t("Nenhum frame de validação", "No validation frames")}</li>
              <li>
                {t(`Menos de ${DATASET_MIN_SAMPLES} frames no total`, `Fewer than ${DATASET_MIN_SAMPLES} frames in total`)}
              </li>
            </ul>
          </section>
        </aside>
      </div>
    );
  };

  const renderTrainDeploy = () => {
    const presetActive =
      modelCfg.labelmap_path === NINJANVR_SECURITY_15CL_LABELMAP_PATH && modelCfg.model_type === "yolo-generic";
    const [newW, newH] = uploadDims.split("x");
    return (
      <div className="nvr-ds-twocol">
        <section className="nvr-ds-panel" aria-labelledby="ds-train-h">
          <div className="nvr-ds-panel-head">
            <h3 id="ds-train-h" className="nvr-ds-panel-title">{t("Treinar o modelo", "Train the model")}</h3>
            <span className="nvr-ds-hint">
              {t(
                "O treino roda fora do NVR. Aqui você leva o dataset e traz o modelo de volta.",
                "Training runs outside the NVR. Take the dataset out here and bring the model back."
              )}
            </span>
          </div>
          <div className="nvr-ds-section-body">
            {report.blockCount > 0 && (
              <div className="nvr-ds-tile" style={{ borderColor: "var(--signal-alert-border)" }}>
                <span style={{ fontSize: "var(--text-sm)" }}>
                  {t(
                    `${report.blockCount} bloqueio(s) na prontidão. O .zip sai, mas o treino tende a falhar.`,
                    `${report.blockCount} readiness blocker(s). The .zip downloads, but training will likely fail.`
                  )}
                </span>
                <button type="button" className="nvr-btn mono sm" onClick={() => goStep("readiness")}>
                  {t("Ver prontidão", "See readiness")}
                </button>
              </div>
            )}
            <ol className="nvr-ds-steps">
              <li>
                <span className="nvr-ds-step-dot is-active">A</span>
                <div style={{ display: "flex", flexDirection: "column", gap: 8, minWidth: 0 }}>
                  <strong>{t("Baixe o dataset", "Download the dataset")}</strong>
                  <div className="nvr-ds-tile">
                    <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
                      <span className="mono" style={{ fontSize: "var(--text-sm)" }}>ninjanvr-yolo-dataset.zip</span>
                      <span className="nvr-ds-hint">
                        {counts.all} frames · {summary?.total_annotations ?? 0} boxes ·{" "}
                        {t("formato YOLO · 15 classes", "YOLO format · 15 classes")}
                      </span>
                    </div>
                    <a href="/api/dataset/export.zip" download="ninjanvr-yolo-dataset.zip" className="nvr-btn mono sm">
                      {t("Baixar .zip", "Download .zip")}
                    </a>
                  </div>
                </div>
              </li>
              <li>
                <span className="nvr-ds-step-dot">B</span>
                <div style={{ display: "flex", flexDirection: "column", gap: 8, minWidth: 0 }}>
                  <strong>{t("Escolha onde treinar", "Choose where to train")}</strong>
                  <div className="nvr-segmented" role="tablist" aria-label={t("Onde treinar", "Where to train")} style={{ alignSelf: "flex-start" }}>
                    <button
                      type="button"
                      role="tab"
                      aria-selected={trainTarget === "colab"}
                      className={`nvr-btn mono sm ${trainTarget === "colab" ? "active" : ""}`}
                      onClick={() => setTrainTarget("colab")}
                    >
                      {t("Google Colab (sem GPU local)", "Google Colab (no local GPU)")}
                    </button>
                    <button
                      type="button"
                      role="tab"
                      aria-selected={trainTarget === "local"}
                      className={`nvr-btn mono sm ${trainTarget === "local" ? "active" : ""}`}
                      onClick={() => setTrainTarget("local")}
                    >
                      {t("Minha máquina (GPU ou CPU)", "My machine (GPU or CPU)")}
                    </button>
                  </div>
                  {trainTarget === "colab" ? (
                    <div className="nvr-ds-tile" style={{ flexDirection: "column", alignItems: "stretch" }}>
                      <ol style={{ margin: 0, paddingLeft: 18, fontSize: "var(--text-sm)", color: "var(--text-secondary)", lineHeight: 1.7 }}>
                        <li>{t("Abra o notebook e faça upload do .zip.", "Open the notebook and upload the .zip.")}</li>
                        <li>{t("Rode todas as células.", "Run all cells.")}</li>
                        <li>{t("Baixe o .onnx gerado e solte em “Implantar”, ao lado.", "Download the generated .onnx and drop it in “Deploy”, alongside.")}</li>
                      </ol>
                      <a
                        href={COLAB_MODEL_FACTORY_URL}
                        target="_blank"
                        rel="noopener noreferrer"
                        className="nvr-btn mono sm"
                        style={{ alignSelf: "flex-start" }}
                      >
                        {t("Abrir notebook no Colab", "Open notebook in Colab")} <ExternalLink size={12} />
                      </a>
                    </div>
                  ) : (
                    <div className="nvr-ds-tile" style={{ flexDirection: "column", alignItems: "stretch" }}>
                      <span style={{ fontSize: "var(--text-sm)", color: "var(--text-secondary)" }}>
                        {t(
                          "Com o .zip na pasta atual, rode o script do repositório. Ele envia o modelo de volta ao NVR ao terminar.",
                          "With the .zip in the current folder, run the repository script. It sends the model back to the NVR when done."
                        )}
                      </span>
                      <code className="nvr-ds-code">{cliCommand}</code>
                      <button
                        type="button"
                        className="nvr-btn mono sm"
                        style={{ alignSelf: "flex-start" }}
                        onClick={() => {
                          navigator.clipboard?.writeText(cliCommand);
                          notify(t("Comando copiado", "Command copied"));
                        }}
                      >
                        <Copy size={12} />
                        {t("Copiar comando", "Copy command")}
                      </button>
                    </div>
                  )}
                  <details style={{ fontSize: "var(--text-sm)", color: "var(--text-secondary)" }}>
                    <summary style={{ cursor: "pointer" }}>{t("Parâmetros de treino (avançado)", "Training parameters (advanced)")}</summary>
                    <div className="mono" style={{ marginTop: 6, fontSize: "var(--text-xs)", lineHeight: 1.7 }}>
                      yolo11s · imgsz {modelCfg.width === 320 ? 320 : 640} · epochs 30 · freeze 10 · FP16
                    </div>
                  </details>
                </div>
              </li>
            </ol>
          </div>
        </section>

        <section className="nvr-ds-panel" aria-labelledby="ds-deploy-h">
          <div className="nvr-ds-panel-head">
            <h3 id="ds-deploy-h" className="nvr-ds-panel-title">{t("Implantar o modelo treinado", "Deploy the trained model")}</h3>
            <span className="nvr-ds-hint">
              {t(
                "Troca o detector sem reiniciar. O labelmap de 15 classes vai junto.",
                "Swaps the detector without a restart. The 15-class labelmap goes with it."
              )}
            </span>
          </div>
          <div className="nvr-ds-section-body">
            {upload ? (
              <div className="nvr-ds-tile is-ok" style={{ flexDirection: "column", alignItems: "stretch" }}>
                <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 8, flexWrap: "wrap" }}>
                  <div style={{ display: "flex", flexDirection: "column", gap: 2, minWidth: 0 }}>
                    <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-live-text)" }}>
                      {t("ARQUIVO PRONTO", "FILE READY")}
                    </span>
                    <span className="mono" style={{ fontSize: "var(--text-sm)", overflowWrap: "anywhere" }}>
                      {upload.name} · {formatBytes(upload.size)}
                    </span>
                  </div>
                  <button
                    type="button"
                    className="nvr-btn mono sm"
                    onClick={() => {
                      if (uploadFileReaderRef.current) {
                        try {
                          uploadFileReaderRef.current.abort();
                        } catch {
                          // ignore
                        }
                        uploadFileReaderRef.current = null;
                      }
                      setUpload(null);
                    }}
                  >
                    {t("Trocar arquivo", "Change file")}
                  </button>
                </div>
                <div className="nvr-ds-filters">
                  <label>
                    <span>{t("Nome no NVR", "Name on the NVR")}</span>
                    <input
                      type="text"
                      className="nvr-input mono sm"
                      value={uploadFilename}
                      onChange={(e) => setUploadFilename(e.target.value)}
                    />
                  </label>
                  <label>
                    <span>{t("Entrada do modelo", "Model input")}</span>
                    <select className="nvr-select mono sm" value={uploadDims} onChange={(e) => setUploadDims(e.target.value)}>
                      <option value="640x640">640×640 (YOLOv8 / YOLO11)</option>
                      <option value="320x320">320×320 (YOLOX edge)</option>
                      <option value="300x300">300×300 (MobileNetV2 SSD)</option>
                    </select>
                  </label>
                </div>
              </div>
            ) : (
              <label
                className={`nvr-ds-drop ${dragOver ? "is-over" : ""}`}
                onDragOver={(e) => {
                  e.preventDefault();
                  setDragOver(true);
                }}
                onDragLeave={() => setDragOver(false)}
                onDrop={(e) => {
                  e.preventDefault();
                  setDragOver(false);
                  stageFile(e.dataTransfer.files?.[0]);
                }}
              >
                <Upload size={20} color="var(--signal-accent-text)" aria-hidden="true" />
                <span>{t("Solte o arquivo .onnx ou .xml aqui", "Drop the .onnx or .xml file here")}</span>
                <span className="nvr-ds-hint">{t("ou clique para escolher", "or click to choose")}</span>
                <input
                  type="file"
                  accept=".onnx,.xml,.bin"
                  className="nvr-ds-visually-hidden"
                  onChange={(e) => stageFile(e.target.files?.[0])}
                />
              </label>
            )}

            {deployOutcome && <DeployOutcomeCard t={t} outcome={deployOutcome} isPt={isPt} onDismiss={() => setDeployOutcome(null)} />}

            <div className="nvr-ds-compare">
              <ActiveModelTile t={t} catalog={catalog} configPath={modelCfg.path} configW={modelCfg.width} configH={modelCfg.height} modelType={modelCfg.model_type} onRefresh={() => void loadCatalog()} />
              <div className={`nvr-ds-tile ${upload ? "is-next" : ""}`} style={{ flexDirection: "column", alignItems: "flex-start", gap: 4 }}>
                <span className="nvr-ds-hint" style={upload ? { color: "var(--signal-accent-text)" } : undefined}>
                  {t("NOVO", "NEW")}
                </span>
                <span className="mono" style={{ fontSize: "var(--text-sm)", overflowWrap: "anywhere" }}>
                  {upload ? activeModelDisplayName(uploadFilename) : "—"}
                </span>
                <span className="nvr-ds-hint">
                  {upload ? `${newW}×${newH} · ${t("15 classes de segurança", "15 security classes")}` : t("nenhum arquivo", "no file")}
                </span>
              </div>
            </div>

            <button
              type="button"
              className="nvr-btn success mono"
              style={{ justifyContent: "center", minHeight: 42, fontWeight: 700 }}
              disabled={!upload || busyAction === "upload-model"}
              onClick={() => void deployModel()}
            >
              {busyAction === "upload-model"
                ? t("Enviando e recarregando…", "Uploading and reloading…")
                : t("Implantar e recarregar detector", "Deploy and reload detector")}
            </button>

            <details style={{ fontSize: "var(--text-sm)", color: "var(--text-secondary)" }}>
              <summary style={{ cursor: "pointer" }}>{t("Avançado: preset de 15 classes sem upload", "Advanced: 15-class preset without upload")}</summary>
              <div style={{ display: "flex", flexDirection: "column", gap: 6, marginTop: 6 }}>
                <span className="nvr-ds-hint">
                  {t(
                    `Aponta a config para o modelo e labelmap de 15 classes já presentes no servidor. Labelmap ativo: ${modelCfg.labelmap_path}`,
                    `Points the config at the 15-class model and labelmap already on the server. Active labelmap: ${modelCfg.labelmap_path}`
                  )}
                </span>
                <button
                  type="button"
                  className={`nvr-btn mono sm ${presetActive ? "active" : ""}`}
                  style={{ alignSelf: "flex-start" }}
                  disabled={!updateDraft || presetActive}
                  onClick={() => {
                    updateDraft?.((next) => {
                      next.model = applySecurity15ClassPreset(next.model);
                    });
                    notify(t("Preset aplicado no rascunho. Salve a config para ativar.", "Preset applied to the draft. Save the config to activate it."));
                  }}
                >
                  {presetActive ? t("Preset já aplicado", "Preset already applied") : t("Aplicar preset no rascunho", "Apply preset to draft")}
                </button>
              </div>
            </details>
          </div>
        </section>
      </div>
    );
  };

  return (
    <div className="nvr-ds" style={{ gridColumn: "1 / -1" }}>
      {renderHeader()}
      {step === "annotate" && (
        <>
          {renderStrip()}
          <div className="nvr-ds-bench">
            {renderQueue()}
            {renderStage()}
            {renderInspector()}
          </div>
        </>
      )}
      {step === "readiness" && renderReadiness()}
      {step === "train" && renderTrainDeploy()}
    </div>
  );
};

type Tr = (pt: string, en: string) => string;

function clock(ms: number, isPt: boolean): string {
  return new Date(ms).toLocaleTimeString(isPt ? "pt-BR" : "en-US", { hour: "2-digit", minute: "2-digit" });
}

const ActiveModelTile: React.FC<{
  t: Tr;
  catalog: DetectorRuntimeCatalog | null;
  configPath: string;
  configW: number;
  configH: number;
  modelType: string;
  onRefresh: () => void;
}> = ({ t, catalog, configPath, configW, configH, modelType, onRefresh }) => {
  const state = resolveActiveModelState(catalog, configPath);
  const runtimePath = catalog?.active_model_path || configPath;
  const badge =
    state === "loaded"
      ? { cls: "is-ok", text: t("CARREGADO", "LOADED") }
      : state === "not_loaded"
      ? { cls: "is-bad", text: t("NÃO CARREGADO", "NOT LOADED") }
      : state === "pending"
      ? { cls: "is-warn", text: t("CONFIG DIFERENTE", "CONFIG DIFFERS") }
      : { cls: "", text: t("VERIFICANDO…", "CHECKING…") };
  return (
    <div className="nvr-ds-tile" style={{ flexDirection: "column", alignItems: "flex-start", gap: 4 }}>
      <span className="nvr-ds-active-head">
        <span className="nvr-ds-hint">{t("RODANDO AGORA", "RUNNING NOW")}</span>
        <span className={`nvr-ds-active-badge mono ${badge.cls}`}>{badge.text}</span>
      </span>
      <span className="mono" style={{ fontSize: "var(--text-sm)", overflowWrap: "anywhere" }}>
        {activeModelDisplayName(runtimePath)}
      </span>
      <span className="nvr-ds-hint">
        {catalog && state === "loaded"
          ? `${catalog.compiled_width || configW}×${catalog.compiled_height || configH} · ${catalog.active_device} · ${
              catalogInferenceMs(catalog) === null
                ? t("aguardando a primeira inferência", "waiting for the first inference")
                : t(
                    `${formatMeasured(catalogInferenceMs(catalog), 1, " ms")} por inferência`,
                    `${formatMeasured(catalogInferenceMs(catalog), 1, " ms")} per inference`
                  )
            }`
          : `${configW}×${configH} · ${modelType}`}
      </span>
      {state === "not_loaded" && (
        <span className="nvr-ds-hint" style={{ color: "var(--signal-alert-text)" }}>
          {t(
            "O detector não conseguiu abrir este arquivo. As câmeras estão sem IA de objetos até um modelo válido ser implantado.",
            "The detector could not open this file. Cameras have no object AI until a valid model is deployed."
          )}
        </span>
      )}
      {state === "pending" && (
        <span className="nvr-ds-hint" style={{ color: "var(--signal-motion-text)" }}>
          {t(
            `O config.yml aponta para ${activeModelDisplayName(configPath)}, mas o detector ainda roda o modelo acima.`,
            `config.yml points at ${activeModelDisplayName(configPath)}, but the detector still runs the model above.`
          )}
        </span>
      )}
      <button type="button" className="nvr-btn mono sm" onClick={onRefresh}>
        {t("Verificar de novo", "Check again")}
      </button>
    </div>
  );
};

const DeployOutcomeCard: React.FC<{
  t: Tr;
  isPt: boolean;
  outcome: ModelDeployOutcome;
  onDismiss: () => void;
}> = ({ t, isPt, outcome, onDismiss }) => {
  const tone = outcome.ok && outcome.loaded ? "is-ok" : outcome.ok ? "is-warn" : "is-bad";
  const title = outcome.ok
    ? outcome.loaded
      ? t(`Implantado às ${clock(outcome.at, isPt)}`, `Deployed at ${clock(outcome.at, isPt)}`)
      : t("Arquivo salvo, mas o detector não carregou", "File saved, but the detector did not load it")
    : outcome.rolledBack
    ? t("Não implantado: o modelo não carregou", "Not deployed: the model did not load")
    : t("Não implantado", "Not deployed");
  const lines: string[] = [];
  if (outcome.ok) {
    lines.push(
      t(
        `${outcome.filename} · ${outcome.width}×${outcome.height}`,
        `${outcome.filename} · ${outcome.width}×${outcome.height}`
      )
    );
    lines.push(
      outcome.loaded
        ? t("As câmeras já estão detectando com este modelo. Não precisa reiniciar.", "Cameras are already detecting with this model. No restart needed.")
        : t(
            "Confira o formato (.onnx exportado com opset compatível) e a entrada do modelo, depois implante de novo.",
            "Check the format (.onnx exported with a compatible opset) and the model input, then deploy again."
          )
    );
    lines.push(
      outcome.persisted
        ? t("Gravado no config.yml: continua ativo depois de reiniciar.", "Written to config.yml: stays active after a restart.")
        : t(
            "Atenção: não foi possível gravar no config.yml. Use Salvar config para manter depois de reiniciar.",
            "Warning: config.yml could not be written. Use Save Config to keep it after a restart."
          )
    );
  } else {
    lines.push(
      outcome.rolledBack
        ? t(
            "O modelo anterior continua ativo e o config.yml não mudou. Confira o arquivo e tente de novo.",
            "The previous model is still active and config.yml did not change. Check the file and try again."
          )
        : outcome.message
    );
  }
  return (
    <div className={`nvr-ds-outcome ${tone}`} role={tone === "is-bad" ? "alert" : "status"}>
      <div className="nvr-ds-outcome-head">
        <strong>{title}</strong>
        <button type="button" className="nvr-btn mono sm" onClick={onDismiss} aria-label={t("Fechar", "Dismiss")}>
          <X size={12} />
        </button>
      </div>
      {lines.map((l) => (
        <span key={l}>{l}</span>
      ))}
    </div>
  );
};
