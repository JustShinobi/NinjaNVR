import type {
  AudioConfig,
  CameraConfig,
  CameraDetectConfig,
  NinjaConfigResponse,
  ObjectFilterConfig,
  ObjectsConfig,
  RecordConfig,
  ReviewConfig,
  SnapshotsConfig,
} from "../types/nvr";
import { buildCleanYamlFromDraft } from "./configYaml";
import { sanitizeConfigGeometryDraft } from "./visionGeometry";

export type NinjaConfig = NinjaConfigResponse;
export type ConfigScope = "global" | string;
export type ScopedConfigSection =
  | "record"
  | "detect"
  | "objects"
  | "snapshots"
  | "review"
  | "audio";
export type SnapshotSelectionPolicy = NonNullable<SnapshotsConfig["selection"]>;

export const DEFAULT_DETECT_CONFIG: CameraDetectConfig = {
  enabled: true,
  width: 1280,
  height: 720,
  fps: 5,
  min_initialized: 2,
  max_disappeared: 25,
  stationary: { interval: 50, threshold: 50, classifier: true },
};

export const DEFAULT_OBJECT_FILTER_CONFIG: ObjectFilterConfig = {
  min_area: 0,
  max_area: 24000000,
  min_ratio: 0,
  max_ratio: 24000000,
  threshold: 0.7,
  min_score: 0.5,
};

export const DEFAULT_ATTRIBUTE_LABELS: string[] = [
  "face",
  "license_plate",
  "correios",
  "mercado_livre",
  "amazon",
  "fedex",
  "dhl",
  "ups",
  "usps",
];

export const DEFAULT_RECORD_CONFIG: RecordConfig = {
  enabled: true,
  expire_interval: 60,
  continuous: { days: 14 },
  motion: { days: 0 },
  alerts: {
    pre_capture: 5,
    post_capture: 5,
    retain: { days: 14, mode: "motion" },
  },
  detections: {
    pre_capture: 5,
    post_capture: 5,
    retain: { days: 14, mode: "motion" },
  },
  preview: { quality: "medium" },
};

export const DEFAULT_SNAPSHOTS_CONFIG: SnapshotsConfig = {
  enabled: true,
  timestamp: false,
  bounding_box: true,
  crop: false,
  quality: 70,
};

export const DEFAULT_REVIEW_CONFIG: ReviewConfig = {
  alerts: {
    enabled: true,
    labels: ["person", "car"],
    required_zones: [],
    cutoff_time: 40,
  },
  detections: {
    enabled: true,
    required_zones: [],
    cutoff_time: 30,
  },
};

export const DEFAULT_AUDIO_CONFIG: AudioConfig = {
  enabled: false,
  min_volume_dbfs: -60,
  labels: ["audio", "speech", "bark", "alarm"],
};

function cloneDeep<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

function isConfigDraftObject(val: unknown): val is NinjaConfigResponse {
  return (
    val !== null &&
    typeof val === "object" &&
    !Array.isArray(val) &&
    ("cameras" in val || "version" in val || "safe_mode" in val)
  );
}

export function toggleAttributeLabelList(
  current: string[] | undefined,
  label: string
): string[] {
  const base = current ?? DEFAULT_ATTRIBUTE_LABELS;
  const clean = label.trim().toLowerCase();
  if (!clean) return [...base];
  return base.includes(clean)
    ? base.filter((x) => x !== clean)
    : [...base, clean];
}

export function toggleSubLabelRuleList(
  current: string[] | undefined,
  subLabel: string
): string[] {
  const base = current ? [...current] : [];
  const clean = subLabel.trim();
  if (!clean) return base;
  const existingIdx = base.findIndex(
    (item) => item.toLowerCase() === clean.toLowerCase()
  );
  if (existingIdx >= 0) {
    return base.filter((_, idx) => idx !== existingIdx);
  }
  return [...base, clean];
}

export interface PartialRecordConfigOverride {
  enabled?: boolean;
  expire_interval?: number;
  continuous?: { days: number };
  motion?: { days: number };
  alerts?: {
    pre_capture?: number;
    post_capture?: number;
    retain?: { days?: number; mode?: string };
  };
  detections?: {
    pre_capture?: number;
    post_capture?: number;
    retain?: { days?: number; mode?: string };
  };
  preview?: { quality: string };
  continuous_substream?: boolean;
  mode_tiering?: boolean;
  max_size_gb?: number | null;
  min_free_gb?: number | null;
  cold_compaction?: RecordConfig["cold_compaction"];
  storage_tiers?: RecordConfig["storage_tiers"];
  alert_backup?: RecordConfig["alert_backup"];
}

export function resolveEffectiveRecordConfig(
  base: RecordConfig,
  override?: PartialRecordConfigOverride | null
): RecordConfig {
  const src = override || {};
  return {
    ...base,
    ...src,
    continuous: {
      days: src.continuous?.days ?? base.continuous?.days ?? 14,
    },
    motion: {
      days: src.motion?.days ?? base.motion?.days ?? 0,
    },
    alerts: {
      pre_capture: src.alerts?.pre_capture ?? base.alerts?.pre_capture ?? 5,
      post_capture: src.alerts?.post_capture ?? base.alerts?.post_capture ?? 5,
      retain: {
        days: src.alerts?.retain?.days ?? base.alerts?.retain?.days ?? 14,
        mode: src.alerts?.retain?.mode || base.alerts?.retain?.mode || "motion",
      },
    },
    detections: {
      pre_capture:
        src.detections?.pre_capture ?? base.detections?.pre_capture ?? 5,
      post_capture:
        src.detections?.post_capture ?? base.detections?.post_capture ?? 5,
      retain: {
        days:
          src.detections?.retain?.days ?? base.detections?.retain?.days ?? 14,
        mode:
          src.detections?.retain?.mode ||
          base.detections?.retain?.mode ||
          "motion",
      },
    },
    preview: {
      quality: src.preview?.quality || base.preview?.quality || "medium",
    },
  };
}

function resolveTargetCamera(
  config: NinjaConfigResponse | null | undefined,
  scope: ConfigScope,
  activeCamera?: string
): string {
  const cleanActive = (activeCamera || "").trim();
  const cleanScope = (scope || "").trim();
  if (cleanScope && cleanScope !== "global" && cleanScope !== "camera") {
    if (!cleanActive || cleanActive === cleanScope) {
      return cleanScope;
    }
    if (
      config?.cameras &&
      cleanScope in config.cameras &&
      !(cleanActive in config.cameras)
    ) {
      return cleanScope;
    }
  }
  if (cleanActive) return cleanActive;
  if (cleanScope && cleanScope !== "global" && cleanScope !== "camera") {
    return cleanScope;
  }
  const firstCam = Object.keys(config?.cameras || {})[0];
  return firstCam || "";
}

function getCameraEntry(
  config: NinjaConfigResponse | null | undefined,
  cameraName: string
): CameraConfig | undefined {
  if (!config?.cameras || !cameraName) return undefined;
  return config.cameras[cameraName];
}

export interface ScopedConfigLens {
  readonly scope: ConfigScope;
  readonly activeCamera: string;
  readonly isGlobal: boolean;

  readDetect(): CameraDetectConfig;
  mutateDetect(
    draft: NinjaConfigResponse,
    patchOrFn:
      | Partial<CameraDetectConfig>
      | ((detect: CameraDetectConfig) => void)
  ): CameraDetectConfig;
  mutateDetect(
    patchOrFn:
      | Partial<CameraDetectConfig>
      | ((detect: CameraDetectConfig) => void)
  ): CameraDetectConfig;

  readObjects(): ObjectsConfig;
  readTrackedLabels(): string[];
  readAttributeLabels(): string[];
  readObjectFilter(label: string): ObjectFilterConfig;
  mutateObjects(
    draft: NinjaConfigResponse,
    fn: (objects: ObjectsConfig) => void
  ): ObjectsConfig;
  mutateObjects(fn: (objects: ObjectsConfig) => void): ObjectsConfig;
  toggleTrackedLabel(
    draft: NinjaConfigResponse,
    label: string,
    options?: { allowEmpty?: boolean }
  ): string[];
  toggleTrackedLabel(
    label: string,
    options?: { allowEmpty?: boolean }
  ): string[];
  toggleAttributeLabel(draft: NinjaConfigResponse, label: string): string[];
  toggleAttributeLabel(label: string): string[];
  updateObjectFilter<K extends keyof ObjectFilterConfig>(
    draft: NinjaConfigResponse,
    label: string,
    key: K,
    value: ObjectFilterConfig[K]
  ): ObjectFilterConfig;
  updateObjectFilter(
    draft: NinjaConfigResponse,
    label: string,
    patch: Partial<ObjectFilterConfig>
  ): ObjectFilterConfig;
  updateObjectFilter<K extends keyof ObjectFilterConfig>(
    label: string,
    key: K,
    value: ObjectFilterConfig[K]
  ): ObjectFilterConfig;
  updateObjectFilter(
    label: string,
    patch: Partial<ObjectFilterConfig>
  ): ObjectFilterConfig;

  readExcludedSubLabels(): string[];
  toggleExcludedSubLabel(
    draft: NinjaConfigResponse,
    subLabel: string
  ): string[];
  toggleExcludedSubLabel(
    draft: NinjaConfigResponse,
    label: string,
    subLabel: string
  ): string[];
  toggleExcludedSubLabel(subLabel: string): string[];
  toggleExcludedSubLabel(label: string, subLabel: string): string[];

  readSnapshotSelection(): SnapshotSelectionPolicy;
  mutateSnapshotSelection(
    draft: NinjaConfigResponse,
    patch: SnapshotSelectionPolicy | Partial<SnapshotsConfig>
  ): SnapshotsConfig;
  mutateSnapshotSelection(
    patch: SnapshotSelectionPolicy | Partial<SnapshotsConfig>
  ): SnapshotsConfig;

  readRecord(): RecordConfig;
  mutateRecord(
    draft: NinjaConfigResponse,
    fnOrPatch: ((record: RecordConfig) => void) | Partial<RecordConfig>
  ): RecordConfig;
  mutateRecord(
    fnOrPatch: ((record: RecordConfig) => void) | Partial<RecordConfig>
  ): RecordConfig;

  readSnapshots(): SnapshotsConfig;
  mutateSnapshots(
    draft: NinjaConfigResponse,
    fnOrPatch: ((snapshots: SnapshotsConfig) => void) | Partial<SnapshotsConfig>
  ): SnapshotsConfig;
  mutateSnapshots(
    fnOrPatch: ((snapshots: SnapshotsConfig) => void) | Partial<SnapshotsConfig>
  ): SnapshotsConfig;

  readReview(): ReviewConfig;
  mutateReview(
    draft: NinjaConfigResponse,
    fnOrPatch: ((review: ReviewConfig) => void) | Partial<ReviewConfig>
  ): ReviewConfig;
  mutateReview(
    fnOrPatch: ((review: ReviewConfig) => void) | Partial<ReviewConfig>
  ): ReviewConfig;

  readAudio(): AudioConfig;
  mutateAudio(
    draft: NinjaConfigResponse,
    fnOrPatch: ((audio: AudioConfig) => void) | Partial<AudioConfig>
  ): AudioConfig;
  mutateAudio(
    fnOrPatch: ((audio: AudioConfig) => void) | Partial<AudioConfig>
  ): AudioConfig;

  applySectionToAllCameras(
    draft: NinjaConfigResponse,
    section: ScopedConfigSection
  ): void;
  applySectionToAllCameras(section: ScopedConfigSection): void;
}

export function createScopedConfigLens(
  config: NinjaConfigResponse,
  scope: ConfigScope,
  activeCamera: string = ""
): ScopedConfigLens {
  const isGlobal = scope === "global";
  const targetCamera = resolveTargetCamera(config, scope, activeCamera);

  const readDetectFrom = (cfg: NinjaConfigResponse): CameraDetectConfig => {
    const globalDetect = cfg.detect;
    const base: CameraDetectConfig = {
      ...DEFAULT_DETECT_CONFIG,
      stationary: { ...DEFAULT_DETECT_CONFIG.stationary! },
      ...(globalDetect || {}),
      ...(globalDetect?.stationary
        ? {
            stationary: {
              ...DEFAULT_DETECT_CONFIG.stationary!,
              ...globalDetect.stationary,
            },
          }
        : {}),
    };
    if (isGlobal) {
      return base;
    }
    const camDetect = getCameraEntry(cfg, targetCamera)?.detect;
    if (!camDetect) {
      return base;
    }
    return {
      ...base,
      ...camDetect,
      stationary: {
        ...(base.stationary || DEFAULT_DETECT_CONFIG.stationary!),
        ...(camDetect.stationary || {}),
      },
    };
  };

  const readTrackedLabelsFrom = (cfg: NinjaConfigResponse): string[] => {
    if (isGlobal) {
      return [...(cfg.objects?.track || ["person"])];
    }
    const camTrack = getCameraEntry(cfg, targetCamera)?.objects?.track;
    return [...(camTrack || cfg.objects?.track || ["person"])];
  };

  const readAttributeLabelsFrom = (cfg: NinjaConfigResponse): string[] => {
    if (isGlobal) {
      return [...(cfg.objects?.attribute_labels ?? DEFAULT_ATTRIBUTE_LABELS)];
    }
    const camAttrs =
      getCameraEntry(cfg, targetCamera)?.objects?.attribute_labels;
    return [
      ...(camAttrs ??
        cfg.objects?.attribute_labels ??
        DEFAULT_ATTRIBUTE_LABELS),
    ];
  };

  const readObjectFilterFrom = (
    cfg: NinjaConfigResponse,
    label: string
  ): ObjectFilterConfig => {
    const cleanLabel = label.trim() || "person";
    const globalFilter = cfg.objects?.filters?.[cleanLabel];
    const camFilter = !isGlobal
      ? getCameraEntry(cfg, targetCamera)?.objects?.filters?.[cleanLabel]
      : undefined;
    const resolved = isGlobal ? globalFilter : camFilter ?? globalFilter;
    return {
      ...DEFAULT_OBJECT_FILTER_CONFIG,
      ...(resolved ? cloneDeep(resolved) : {}),
    };
  };

  const readObjectsFrom = (cfg: NinjaConfigResponse): ObjectsConfig => {
    const track = readTrackedLabelsFrom(cfg);
    const attributeLabels = readAttributeLabelsFrom(cfg);
    const mergedFilters: Record<string, ObjectFilterConfig> = {};
    if (cfg.objects?.filters) {
      for (const [k, v] of Object.entries(cfg.objects.filters)) {
        mergedFilters[k] = {
          ...DEFAULT_OBJECT_FILTER_CONFIG,
          ...cloneDeep(v),
        };
      }
    }
    if (!isGlobal) {
      const camFilters = getCameraEntry(cfg, targetCamera)?.objects?.filters;
      if (camFilters) {
        for (const [k, v] of Object.entries(camFilters)) {
          mergedFilters[k] = {
            ...(mergedFilters[k] || DEFAULT_OBJECT_FILTER_CONFIG),
            ...cloneDeep(v),
          };
        }
      }
    }
    const rawMask = isGlobal
      ? cfg.objects?.mask
      : getCameraEntry(cfg, targetCamera)?.objects?.mask ?? cfg.objects?.mask;
    return {
      track,
      attribute_labels: attributeLabels,
      filters: mergedFilters,
      ...(rawMask !== undefined ? { mask: cloneDeep(rawMask) } : {}),
    };
  };

  const ensureScopedObjectsTarget = (
    draft: NinjaConfigResponse
  ): ObjectsConfig | null => {
    if (isGlobal) {
      if (!draft.objects) {
        draft.objects = {
          track: readTrackedLabelsFrom(draft),
          filters: {},
        };
      } else if (!draft.objects.filters) {
        draft.objects.filters = {};
      }
      return draft.objects;
    }
    const cam = getCameraEntry(draft, targetCamera);
    if (!cam) return null;
    if (!cam.objects) {
      cam.objects = {
        track: readTrackedLabelsFrom(draft),
        filters: {},
      };
    } else if (!cam.objects.filters) {
      cam.objects.filters = {};
    }
    return cam.objects;
  };

  const readSnapshotSelectionFrom = (
    cfg: NinjaConfigResponse
  ): SnapshotSelectionPolicy => {
    const raw = isGlobal
      ? cfg.snapshots?.selection
      : getCameraEntry(cfg, targetCamera)?.snapshots?.selection ??
        cfg.snapshots?.selection;
    return raw || "best_in_zone";
  };

  const readRecordFrom = (cfg: NinjaConfigResponse): RecordConfig => {
    const globalEffective = resolveEffectiveRecordConfig(
      DEFAULT_RECORD_CONFIG,
      cfg.record
    );
    if (isGlobal) {
      return cloneDeep(globalEffective);
    }
    return cloneDeep(
      resolveEffectiveRecordConfig(
        globalEffective,
        getCameraEntry(cfg, targetCamera)?.record
      )
    );
  };

  const readSnapshotsFrom = (cfg: NinjaConfigResponse): SnapshotsConfig => {
    const globalSnap: SnapshotsConfig = {
      ...DEFAULT_SNAPSHOTS_CONFIG,
      ...(cfg.snapshots || {}),
    };
    if (isGlobal) {
      return cloneDeep(globalSnap);
    }
    return cloneDeep({
      ...globalSnap,
      ...(getCameraEntry(cfg, targetCamera)?.snapshots || {}),
    });
  };

  const readReviewFrom = (cfg: NinjaConfigResponse): ReviewConfig => {
    const globalAlerts = {
      ...DEFAULT_REVIEW_CONFIG.alerts,
      labels: [...(DEFAULT_REVIEW_CONFIG.alerts.labels || ["person", "car"])],
      required_zones: [...DEFAULT_REVIEW_CONFIG.alerts.required_zones],
      ...(cfg.review?.alerts ? cloneDeep(cfg.review.alerts) : {}),
    };
    const globalDetections = {
      ...DEFAULT_REVIEW_CONFIG.detections,
      required_zones: [...DEFAULT_REVIEW_CONFIG.detections.required_zones],
      ...(cfg.review?.detections ? cloneDeep(cfg.review.detections) : {}),
    };
    if (isGlobal) {
      return {
        alerts: globalAlerts,
        detections: globalDetections,
      };
    }
    const camRev = getCameraEntry(cfg, targetCamera)?.review;
    return {
      alerts: {
        ...globalAlerts,
        ...(camRev?.alerts ? cloneDeep(camRev.alerts) : {}),
      },
      detections: {
        ...globalDetections,
        ...(camRev?.detections ? cloneDeep(camRev.detections) : {}),
      },
    };
  };

  const readExcludedSubLabelsFrom = (cfg: NinjaConfigResponse): string[] => {
    const list = isGlobal
      ? cfg.review?.alerts?.exclude_sub_labels
      : getCameraEntry(cfg, targetCamera)?.review?.alerts?.exclude_sub_labels ??
        cfg.review?.alerts?.exclude_sub_labels;
    return list ? [...list] : [];
  };

  const readAudioFrom = (cfg: NinjaConfigResponse): AudioConfig => {
    const globalAudio: AudioConfig = {
      ...DEFAULT_AUDIO_CONFIG,
      labels: [...(DEFAULT_AUDIO_CONFIG.labels || [])],
      ...(cfg.audio ? cloneDeep(cfg.audio) : {}),
    };
    if (isGlobal) {
      return globalAudio;
    }
    const camAudio = getCameraEntry(cfg, targetCamera)?.audio;
    return {
      ...globalAudio,
      ...(camAudio ? cloneDeep(camAudio) : {}),
    };
  };

  function mutateDetect(
    draftOrPatch:
      | NinjaConfigResponse
      | Partial<CameraDetectConfig>
      | ((detect: CameraDetectConfig) => void),
    maybePatch?:
      | Partial<CameraDetectConfig>
      | ((detect: CameraDetectConfig) => void)
  ): CameraDetectConfig {
    const draft = isConfigDraftObject(draftOrPatch) ? draftOrPatch : config;
    const patchOrFn = (
      isConfigDraftObject(draftOrPatch) ? maybePatch : draftOrPatch
    ) as
      | Partial<CameraDetectConfig>
      | ((detect: CameraDetectConfig) => void);

    const base = cloneDeep(readDetectFrom(draft));
    if (typeof patchOrFn === "function") {
      patchOrFn(base);
    } else if (patchOrFn && typeof patchOrFn === "object") {
      Object.assign(base, patchOrFn);
      if (patchOrFn.stationary) {
        base.stationary = {
          ...(readDetectFrom(draft).stationary ||
            DEFAULT_DETECT_CONFIG.stationary!),
          ...patchOrFn.stationary,
        };
      }
    }

    if (isGlobal) {
      draft.detect = base;
    } else {
      const cam = getCameraEntry(draft, targetCamera);
      if (cam) {
        cam.detect = base;
      }
    }
    return base;
  }

  function mutateObjects(
    draftOrFn: NinjaConfigResponse | ((objects: ObjectsConfig) => void),
    maybeFn?: (objects: ObjectsConfig) => void
  ): ObjectsConfig {
    const draft = isConfigDraftObject(draftOrFn) ? draftOrFn : config;
    const fn = (isConfigDraftObject(draftOrFn) ? maybeFn : draftOrFn) as (
      objects: ObjectsConfig
    ) => void;
    const target = ensureScopedObjectsTarget(draft);
    if (target && typeof fn === "function") {
      fn(target);
      return target;
    }
    return readObjectsFrom(draft);
  }

  function toggleTrackedLabel(
    draftOrLabel: NinjaConfigResponse | string,
    labelOrOptions?: string | { allowEmpty?: boolean },
    maybeOptions?: { allowEmpty?: boolean }
  ): string[] {
    const draft =
      typeof draftOrLabel === "string" ? config : draftOrLabel;
    const rawLabel =
      typeof draftOrLabel === "string"
        ? draftOrLabel
        : String(labelOrOptions || "");
    const options =
      typeof draftOrLabel === "string"
        ? (labelOrOptions as { allowEmpty?: boolean } | undefined)
        : maybeOptions;

    const clean = rawLabel.trim();
    const target = ensureScopedObjectsTarget(draft);
    if (!target) {
      return readTrackedLabelsFrom(draft);
    }
    const curr =
      target.track && target.track.length > 0
        ? [...target.track]
        : readTrackedLabelsFrom(draft);
    if (!clean) {
      target.track = curr;
      return curr;
    }
    if (curr.includes(clean)) {
      if (curr.length <= 1 && !options?.allowEmpty) {
        target.track = curr;
        return curr;
      }
      target.track = curr.filter((x) => x !== clean);
    } else {
      target.track = [...curr, clean];
    }
    return [...target.track];
  }

  function toggleAttributeLabel(
    draftOrLabel: NinjaConfigResponse | string,
    maybeLabel?: string
  ): string[] {
    const draft =
      typeof draftOrLabel === "string" ? config : draftOrLabel;
    const rawLabel =
      typeof draftOrLabel === "string" ? draftOrLabel : String(maybeLabel || "");
    const currentAttrs = readAttributeLabelsFrom(draft);
    const target = ensureScopedObjectsTarget(draft);
    const nextAttrs = toggleAttributeLabelList(
      target?.attribute_labels ?? currentAttrs,
      rawLabel
    );
    if (target) {
      target.attribute_labels = nextAttrs;
    }
    return [...nextAttrs];
  }

  function updateObjectFilter(
    arg1: NinjaConfigResponse | string,
    arg2: string | keyof ObjectFilterConfig | Partial<ObjectFilterConfig>,
    arg3?:
      | keyof ObjectFilterConfig
      | Partial<ObjectFilterConfig>
      | ObjectFilterConfig[keyof ObjectFilterConfig],
    arg4?: ObjectFilterConfig[keyof ObjectFilterConfig]
  ): ObjectFilterConfig {
    let draft: NinjaConfigResponse;
    let label: string;
    let patch: Partial<ObjectFilterConfig>;

    if (typeof arg1 === "string") {
      draft = config;
      label = arg1;
      if (typeof arg2 === "string") {
        patch = { [arg2]: arg3 } as Partial<ObjectFilterConfig>;
      } else {
        patch = (arg2 || {}) as Partial<ObjectFilterConfig>;
      }
    } else {
      draft = arg1;
      label = String(arg2 || "person");
      if (typeof arg3 === "string") {
        patch = { [arg3]: arg4 } as Partial<ObjectFilterConfig>;
      } else {
        patch = (arg3 || {}) as Partial<ObjectFilterConfig>;
      }
    }

    const cleanLabel = label.trim() || "person";
    const currentFilter = readObjectFilterFrom(draft, cleanLabel);
    const merged: ObjectFilterConfig = {
      ...currentFilter,
      ...patch,
    };
    if (patch.min_score !== undefined && merged.min_score > merged.threshold) {
      merged.threshold = merged.min_score;
    }
    if (patch.threshold !== undefined && merged.threshold < merged.min_score) {
      merged.min_score = merged.threshold;
    }

    const target = ensureScopedObjectsTarget(draft);
    if (target) {
      if (!target.filters) target.filters = {};
      target.filters[cleanLabel] = merged;
    }
    return merged;
  }

  function toggleExcludedSubLabel(
    arg1: NinjaConfigResponse | string,
    arg2?: string,
    arg3?: string
  ): string[] {
    let draft: NinjaConfigResponse;
    let subLabel: string;
    if (typeof arg1 === "string") {
      draft = config;
      subLabel = arg2 !== undefined ? arg2 : arg1;
    } else {
      draft = arg1;
      subLabel = arg3 !== undefined ? arg3 : String(arg2 || "");
    }

    const currentList = readExcludedSubLabelsFrom(draft);
    const nextList = toggleSubLabelRuleList(currentList, subLabel);

    if (isGlobal) {
      if (!draft.review) {
        draft.review = cloneDeep(DEFAULT_REVIEW_CONFIG);
      }
      draft.review.alerts.exclude_sub_labels = nextList;
    } else {
      const cam = getCameraEntry(draft, targetCamera);
      if (cam) {
        if (!cam.review) {
          cam.review = readReviewFrom(draft);
        }
        cam.review.alerts.exclude_sub_labels = nextList;
      }
    }
    return [...nextList];
  }

  function mutateSnapshotSelection(
    draftOrPatch:
      | NinjaConfigResponse
      | SnapshotSelectionPolicy
      | Partial<SnapshotsConfig>,
    maybePatch?: SnapshotSelectionPolicy | Partial<SnapshotsConfig>
  ): SnapshotsConfig {
    const draft = isConfigDraftObject(draftOrPatch) ? draftOrPatch : config;
    const patch = (
      isConfigDraftObject(draftOrPatch) ? maybePatch : draftOrPatch
    ) as SnapshotSelectionPolicy | Partial<SnapshotsConfig>;
    const normalizedPatch: Partial<SnapshotsConfig> =
      typeof patch === "string" ? { selection: patch } : patch || {};

    if (isGlobal) {
      if (!draft.snapshots) {
        draft.snapshots = { enabled: true };
      }
      Object.assign(draft.snapshots, normalizedPatch);
      return draft.snapshots;
    }
    const cam = getCameraEntry(draft, targetCamera);
    if (cam) {
      if (!cam.snapshots) {
        cam.snapshots = {
          enabled: draft.snapshots?.enabled ?? true,
          ...(draft.snapshots ? cloneDeep(draft.snapshots) : {}),
        };
      }
      Object.assign(cam.snapshots, normalizedPatch);
      return cam.snapshots;
    }
    return readSnapshotsFrom(draft);
  }

  function mutateRecord(
    draftOrFn:
      | NinjaConfigResponse
      | ((record: RecordConfig) => void)
      | Partial<RecordConfig>,
    maybeFn?: ((record: RecordConfig) => void) | Partial<RecordConfig>
  ): RecordConfig {
    const draft = isConfigDraftObject(draftOrFn) ? draftOrFn : config;
    const fnOrPatch = (
      isConfigDraftObject(draftOrFn) ? maybeFn : draftOrFn
    ) as ((record: RecordConfig) => void) | Partial<RecordConfig>;

    const cloned = readRecordFrom(draft);
    if (typeof fnOrPatch === "function") {
      fnOrPatch(cloned);
    } else if (fnOrPatch && typeof fnOrPatch === "object") {
      Object.assign(cloned, fnOrPatch);
    }

    if (isGlobal) {
      draft.record = cloned;
    } else {
      const cam = getCameraEntry(draft, targetCamera);
      if (cam) {
        cam.record = cloned;
      }
    }
    return cloned;
  }

  function mutateSnapshots(
    draftOrFn:
      | NinjaConfigResponse
      | ((snapshots: SnapshotsConfig) => void)
      | Partial<SnapshotsConfig>,
    maybeFn?: ((snapshots: SnapshotsConfig) => void) | Partial<SnapshotsConfig>
  ): SnapshotsConfig {
    const draft = isConfigDraftObject(draftOrFn) ? draftOrFn : config;
    const fnOrPatch = (
      isConfigDraftObject(draftOrFn) ? maybeFn : draftOrFn
    ) as ((snapshots: SnapshotsConfig) => void) | Partial<SnapshotsConfig>;

    const cloned = readSnapshotsFrom(draft);
    if (typeof fnOrPatch === "function") {
      fnOrPatch(cloned);
    } else if (fnOrPatch && typeof fnOrPatch === "object") {
      Object.assign(cloned, fnOrPatch);
    }

    if (isGlobal) {
      draft.snapshots = cloned;
    } else {
      const cam = getCameraEntry(draft, targetCamera);
      if (cam) {
        cam.snapshots = cloned;
      }
    }
    return cloned;
  }

  function mutateReview(
    draftOrFn:
      | NinjaConfigResponse
      | ((review: ReviewConfig) => void)
      | Partial<ReviewConfig>,
    maybeFn?: ((review: ReviewConfig) => void) | Partial<ReviewConfig>
  ): ReviewConfig {
    const draft = isConfigDraftObject(draftOrFn) ? draftOrFn : config;
    const fnOrPatch = (
      isConfigDraftObject(draftOrFn) ? maybeFn : draftOrFn
    ) as ((review: ReviewConfig) => void) | Partial<ReviewConfig>;

    const cloned = readReviewFrom(draft);
    if (typeof fnOrPatch === "function") {
      fnOrPatch(cloned);
    } else if (fnOrPatch && typeof fnOrPatch === "object") {
      if (fnOrPatch.alerts) {
        cloned.alerts = { ...cloned.alerts, ...fnOrPatch.alerts };
      }
      if (fnOrPatch.detections) {
        cloned.detections = { ...cloned.detections, ...fnOrPatch.detections };
      }
    }

    if (isGlobal) {
      draft.review = cloned;
    } else {
      const cam = getCameraEntry(draft, targetCamera);
      if (cam) {
        cam.review = cloned;
      }
    }
    return cloned;
  }

  function mutateAudio(
    draftOrFn:
      | NinjaConfigResponse
      | ((audio: AudioConfig) => void)
      | Partial<AudioConfig>,
    maybeFn?: ((audio: AudioConfig) => void) | Partial<AudioConfig>
  ): AudioConfig {
    const draft = isConfigDraftObject(draftOrFn) ? draftOrFn : config;
    const fnOrPatch = (
      isConfigDraftObject(draftOrFn) ? maybeFn : draftOrFn
    ) as ((audio: AudioConfig) => void) | Partial<AudioConfig>;

    const cloned = readAudioFrom(draft);
    if (typeof fnOrPatch === "function") {
      fnOrPatch(cloned);
    } else if (fnOrPatch && typeof fnOrPatch === "object") {
      Object.assign(cloned, fnOrPatch);
    }

    if (isGlobal) {
      draft.audio = cloned;
    } else {
      const cam = getCameraEntry(draft, targetCamera);
      if (cam) {
        cam.audio = cloned;
      }
    }
    return cloned;
  }

  function applySectionToAllCameras(
    draftOrSection: NinjaConfigResponse | ScopedConfigSection,
    maybeSection?: ScopedConfigSection
  ): void {
    const draft =
      typeof draftOrSection === "string" ? config : draftOrSection;
    const section =
      typeof draftOrSection === "string"
        ? draftOrSection
        : (maybeSection as ScopedConfigSection);

    if (!draft.cameras) return;
    const sourceLens = createScopedConfigLens(draft, scope, activeCamera);

    for (const cam of Object.values(draft.cameras)) {
      if (!cam) continue;
      switch (section) {
        case "record":
          cam.record = cloneDeep(sourceLens.readRecord());
          break;
        case "detect":
          cam.detect = cloneDeep(sourceLens.readDetect());
          break;
        case "objects":
          cam.objects = cloneDeep(sourceLens.readObjects());
          break;
        case "snapshots":
          cam.snapshots = cloneDeep(sourceLens.readSnapshots());
          break;
        case "review":
          cam.review = cloneDeep(sourceLens.readReview());
          break;
        case "audio":
          cam.audio = cloneDeep(sourceLens.readAudio());
          break;
      }
    }
  }

  return {
    scope,
    activeCamera: targetCamera,
    isGlobal,
    readDetect: () => readDetectFrom(config),
    mutateDetect,
    readObjects: () => readObjectsFrom(config),
    readTrackedLabels: () => readTrackedLabelsFrom(config),
    readAttributeLabels: () => readAttributeLabelsFrom(config),
    readObjectFilter: (label: string) => readObjectFilterFrom(config, label),
    mutateObjects,
    toggleTrackedLabel,
    toggleAttributeLabel,
    updateObjectFilter,
    readExcludedSubLabels: () => readExcludedSubLabelsFrom(config),
    toggleExcludedSubLabel,
    readSnapshotSelection: () => readSnapshotSelectionFrom(config),
    mutateSnapshotSelection,
    readRecord: () => readRecordFrom(config),
    mutateRecord,
    readSnapshots: () => readSnapshotsFrom(config),
    mutateSnapshots,
    readReview: () => readReviewFrom(config),
    mutateReview,
    readAudio: () => readAudioFrom(config),
    mutateAudio,
    applySectionToAllCameras,
  };
}

export function mutateScopedConfig(
  draft: NinjaConfigResponse,
  scope: ConfigScope,
  activeCamera: string,
  fn: (lens: ScopedConfigLens, draft: NinjaConfigResponse) => void
): NinjaConfigResponse {
  const lens = createScopedConfigLens(draft, scope, activeCamera);
  fn(lens, draft);
  return draft;
}

export interface StructuredDraftMutationOptions {
  syncYamlImmediately: boolean;
  baselineYaml: string;
}

export interface StructuredDraftMutationResult {
  draft: NinjaConfigResponse;
  rawYaml: string | null;
  yamlStale: boolean;
  rejectedGeometryCount: number;
}

export function applyStructuredDraftMutation(
  prevDraft: NinjaConfigResponse,
  mutator: (next: NinjaConfigResponse) => void,
  options: StructuredDraftMutationOptions
): StructuredDraftMutationResult {
  const cloned = cloneDeep(prevDraft);
  mutator(cloned);
  const { cleaned, rejectedCount } = sanitizeConfigGeometryDraft(cloned);
  if (options.syncYamlImmediately) {
    const nextYaml = buildCleanYamlFromDraft(cleaned, options.baselineYaml);
    return {
      draft: cleaned,
      rawYaml: nextYaml,
      yamlStale: false,
      rejectedGeometryCount: rejectedCount,
    };
  }
  return {
    draft: cleaned,
    rawYaml: null,
    yamlStale: true,
    rejectedGeometryCount: rejectedCount,
  };
}

export function synthesizeDraftYamlIfNeeded(
  draft: NinjaConfigResponse | null,
  baselineYaml: string,
  currentRawYaml: string,
  yamlStale: boolean
): { rawYaml: string; yamlStale: boolean } {
  if (!draft || !yamlStale) {
    return { rawYaml: currentRawYaml, yamlStale: false };
  }
  return {
    rawYaml: buildCleanYamlFromDraft(draft, baselineYaml || currentRawYaml),
    yamlStale: false,
  };
}
