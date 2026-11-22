import type {
  StorageCleanupJobPhase,
  StorageCleanupJobSnapshot,
  StorageCleanupJobStatus,
  StorageCleanupTarget,
} from "../types/nvr";

const VALID_TARGETS = new Set<StorageCleanupTarget>([
  "events",
  "reviews",
  "excess_recordings",
  "recordings",
  "previews",
  "exports",
  "wal_checkpoint",
  "retention_pass",
]);

const VALID_STATUSES = new Set<StorageCleanupJobStatus>([
  "queued",
  "running",
  "cancelling",
  "completed",
  "cancelled",
  "failed",
]);

const VALID_PHASES = new Set<StorageCleanupJobPhase>([
  "scanning",
  "purging_catalog",
  "unlinking_media",
  "pruning_segments",
  "checkpointing_wal",
  "refreshing_breakdown",
  "done",
]);

function safeNonNegNum(val: unknown, fallback = 0): number {
  if (typeof val === "number" && Number.isFinite(val)) {
    return Math.max(0, val);
  }
  if (typeof val === "string") {
    const parsed = Number(val);
    if (Number.isFinite(parsed)) {
      return Math.max(0, parsed);
    }
  }
  return fallback;
}

export function formatStorageBytes(rawBytes: unknown): string {
  const bytes = safeNonNegNum(rawBytes, 0);
  if (bytes < 1024) {
    return `${Math.round(bytes)} B`;
  }
  const kb = bytes / 1024;
  if (kb < 1024) {
    return `${kb >= 100 ? Math.round(kb) : kb.toFixed(1)} KB`;
  }
  const mb = kb / 1024;
  if (mb < 1024) {
    return `${mb >= 100 ? mb.toFixed(0) : mb.toFixed(1)} MB`;
  }
  const gb = mb / 1024;
  if (gb < 1024) {
    return `${gb.toFixed(2)} GB`;
  }
  const tb = gb / 1024;
  return `${tb.toFixed(2)} TB`;
}

export function formatStorageShortBytes(rawBytes: unknown): string {
  const bytes = safeNonNegNum(rawBytes, 0);
  if (bytes <= 0) return "0 B";
  const mb = bytes / (1024 * 1024);
  if (mb < 1) {
    return `${Math.max(1, Math.round(bytes / 1024))}K`;
  }
  if (mb < 1024) {
    return `${mb >= 10 ? Math.round(mb) : mb.toFixed(1)}M`;
  }
  const gb = mb / 1024;
  return `${gb >= 10 ? gb.toFixed(1) : gb.toFixed(2)}G`;
}

export function isActiveCleanupJobStatus(
  status: StorageCleanupJobStatus | undefined | null
): boolean {
  return status === "queued" || status === "running" || status === "cancelling";
}

export function isTerminalCleanupJobStatus(
  status: StorageCleanupJobStatus | undefined | null
): boolean {
  return (
    status === "completed" || status === "cancelled" || status === "failed"
  );
}

export function parseCleanupJobPayload(
  raw: unknown
): StorageCleanupJobSnapshot | null {
  if (!raw || typeof raw !== "object") return null;
  const obj = raw as Record<string, unknown>;
  const id = typeof obj.id === "string" ? obj.id.trim() : "";
  if (!id || id.length > 128) return null;

  const targetRaw = typeof obj.target === "string" ? obj.target.trim() : "";
  if (!VALID_TARGETS.has(targetRaw as StorageCleanupTarget)) return null;
  const target = targetRaw as StorageCleanupTarget;

  const statusRaw = typeof obj.status === "string" ? obj.status.trim() : "";
  if (!VALID_STATUSES.has(statusRaw as StorageCleanupJobStatus)) return null;
  const status = statusRaw as StorageCleanupJobStatus;

  const phaseRaw = typeof obj.phase === "string" ? obj.phase.trim() : "";
  const phase: StorageCleanupJobPhase = VALID_PHASES.has(
    phaseRaw as StorageCleanupJobPhase
  )
    ? (phaseRaw as StorageCleanupJobPhase)
    : isTerminalCleanupJobStatus(status)
    ? "done"
    : "scanning";

  const camera =
    typeof obj.camera === "string" && obj.camera.trim().length > 0
      ? obj.camera.trim()
      : null;

  const progressPct = Math.min(100, safeNonNegNum(obj.progress_pct, 0));
  const processedUnits = Math.round(safeNonNegNum(obj.processed_units, 0));
  const totalUnits = Math.round(safeNonNegNum(obj.total_units, 0));
  const deletedItems = Math.round(safeNonNegNum(obj.deleted_items, 0));
  const deletedSegments = Math.round(safeNonNegNum(obj.deleted_segments, 0));
  const freedBytes = Math.round(safeNonNegNum(obj.freed_bytes, 0));
  const elapsedMs = Math.round(safeNonNegNum(obj.elapsed_ms, 0));

  const etaSecs =
    typeof obj.eta_secs === "number" &&
    Number.isFinite(obj.eta_secs) &&
    obj.eta_secs > 0
      ? Math.round(obj.eta_secs)
      : null;
  const ratePerSec =
    typeof obj.rate_per_sec === "number" &&
    Number.isFinite(obj.rate_per_sec) &&
    obj.rate_per_sec > 0
      ? obj.rate_per_sec
      : null;

  const cancellable =
    typeof obj.cancellable === "boolean"
      ? obj.cancellable && (status === "queued" || status === "running")
      : false;

  const createdAt = safeNonNegNum(obj.created_at, 0);
  const updatedAt = safeNonNegNum(obj.updated_at, createdAt);
  const message = typeof obj.message === "string" ? obj.message.trim() : "";
  const error =
    typeof obj.error === "string" && obj.error.trim().length > 0
      ? obj.error.trim()
      : null;

  return {
    id,
    target,
    camera,
    status,
    phase,
    progress_pct: progressPct,
    processed_units: processedUnits,
    total_units: totalUnits,
    deleted_items: deletedItems,
    deleted_segments: deletedSegments,
    freed_bytes: freedBytes,
    elapsed_ms: elapsedMs,
    eta_secs: etaSecs,
    rate_per_sec: ratePerSec,
    cancellable,
    created_at: createdAt,
    updated_at: updatedAt,
    message,
    error,
  };
}

/**
 * Reconciles a polled `GET /api/jobs/cleanup` array against the store's current job:
 * - Always adopts any active (`queued | running | cancelling`) job (e.g. page reload mid-cleanup).
 * - Updates the currently tracked active job if it transitioned to terminal while `/ws` was offline.
 * - Avoids resurrecting stale historical terminal banners on a clean reload.
 */
export function reconcileCleanupJobsResponse(
  current: StorageCleanupJobSnapshot | null,
  jobs: ReadonlyArray<StorageCleanupJobSnapshot>
): StorageCleanupJobSnapshot | null {
  const active = jobs.find((j) => isActiveCleanupJobStatus(j.status));
  if (active) {
    return active;
  }
  if (current && isActiveCleanupJobStatus(current.status)) {
    const matched = jobs.find((j) => j.id === current.id);
    return matched ?? null;
  }
  return current;
}

export function formatCleanupEta(etaSecs: number | null | undefined): string | null {
  if (
    etaSecs === null ||
    etaSecs === undefined ||
    !Number.isFinite(etaSecs) ||
    etaSecs <= 0
  ) {
    return null;
  }
  const sec = Math.max(1, Math.round(etaSecs));
  if (sec < 60) {
    return `~${sec}s`;
  }
  if (sec < 3600) {
    const m = Math.floor(sec / 60);
    const s = sec % 60;
    return `~${m}m ${String(s).padStart(2, "0")}s`;
  }
  const h = Math.floor(sec / 3600);
  const m = Math.round((sec % 3600) / 60);
  return `~${h}h ${m}m`;
}

export function formatCleanupTargetLabel(
  target: StorageCleanupTarget,
  camera: string | null | undefined,
  isPt: boolean
): string {
  const camBadge = camera ? ` [${camera.toUpperCase()}]` : "";
  switch (target) {
    case "events":
      return `${isPt ? "Limpar Eventos" : "Clear Events"}${camBadge}`;
    case "reviews":
      return `${isPt ? "Limpar Revisões" : "Clear Reviews"}${camBadge}`;
    case "excess_recordings":
      return `${isPt ? "Podar Excedente MP4" : "Prune Excess MP4"}${camBadge}`;
    case "recordings":
      return `${isPt ? "Limpar Gravações MP4" : "Clear MP4 Recordings"}${camBadge}`;
    case "previews":
      return `${isPt ? "Limpar Previews" : "Clear Previews"}${camBadge}`;
    case "exports":
      return `${isPt ? "Limpar Exports" : "Clear Exports"}${camBadge}`;
    case "wal_checkpoint":
      return isPt ? "Compactar WAL SQLite" : "Checkpoint SQLite WAL";
    case "retention_pass":
      return isPt ? "Ciclo de Retenção" : "Retention Pass";
  }
}

export function formatCleanupPhaseLabel(
  phase: StorageCleanupJobPhase,
  status: StorageCleanupJobStatus,
  isPt: boolean
): string {
  if (status === "cancelling") {
    return isPt
      ? "Parando após concluir o micro-lote atual (preservando espaço já liberado)..."
      : "Stopping after current micro-batch (preserving already freed space)...";
  }
  switch (phase) {
    case "scanning":
      return isPt
        ? "Mapeando catálogo SQLite e arquivos no disco..."
        : "Scanning SQLite catalog & disk files...";
    case "purging_catalog":
      return isPt
        ? "Removendo registros de IA e miniaturas em micro-lotes..."
        : "Purging AI catalog & snapshots in micro-batches...";
    case "unlinking_media":
      return isPt
        ? "Removendo artefatos de mídia do disco..."
        : "Unlinking media artifacts from disk...";
    case "pruning_segments":
      return isPt
        ? "Desvinculando segmentos MP4 em micro-lotes..."
        : "Unlinking MP4 recording segments in micro-batches...";
    case "checkpointing_wal":
      return isPt
        ? "Sincronizando SQLite WAL..."
        : "Checkpointing SQLite WAL...";
    case "refreshing_breakdown":
      return isPt
        ? "Atualizando métricas granulares de disco..."
        : "Refreshing granular disk breakdown...";
    case "done":
      return isPt ? "Concluído" : "Finished";
  }
}

export function formatCleanupTerminalSummary(
  job: StorageCleanupJobSnapshot,
  freedFormatted: string,
  isPt: boolean
): string {
  const scopeLabel = job.camera ? ` [${job.camera.toUpperCase()}]` : "";
  const segSuffixPt =
    job.deleted_segments > 0 ? ` + ${job.deleted_segments} seg(s) MP4` : "";
  const segSuffixEn =
    job.deleted_segments > 0 ? ` + ${job.deleted_segments} MP4 seg(s)` : "";

  if (job.status === "completed") {
    return isPt
      ? `Limpeza concluída${scopeLabel}: ${job.deleted_items} item(ns)${segSuffixPt} removidos · ${freedFormatted} liberados`
      : `Cleanup completed${scopeLabel}: removed ${job.deleted_items} item(s)${segSuffixEn} · freed ${freedFormatted}`;
  }
  if (job.status === "cancelled") {
    return isPt
      ? `Limpeza interrompida${scopeLabel}: ${job.deleted_items} item(ns)${segSuffixPt} removidos até o cancelamento · ${freedFormatted} liberados preservados`
      : `Cleanup cancelled${scopeLabel}: removed ${job.deleted_items} item(s)${segSuffixEn} before stop · ${freedFormatted} freed`;
  }
  const reason = job.error || job.message || "error";
  return isPt
    ? `Falha na limpeza${scopeLabel} (${reason})`
    : `Storage cleanup failed${scopeLabel} (${reason})`;
}

export function resolveCleanupSrMilestone(
  job: StorageCleanupJobSnapshot | null,
  isPt: boolean
): string {
  if (!job) return "";
  const title = formatCleanupTargetLabel(job.target, job.camera, isPt);
  if (job.status === "cancelling") {
    return isPt
      ? `Interrompendo ${title}...`
      : `Stopping ${title}...`;
  }
  if (isActiveCleanupJobStatus(job.status)) {
    const bucket = Math.floor(job.progress_pct / 25) * 25;
    if (bucket === 0) {
      return isPt
        ? `${title} iniciada em segundo plano.`
        : `${title} started in background.`;
    }
    return isPt
      ? `${title}: ${bucket}% concluído.`
      : `${title}: ${bucket}% complete.`;
  }
  return "";
}
