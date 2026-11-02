import React, { useCallback, useEffect, useState } from "react";
import { KeyRound, Plus, ShieldCheck, Trash2 } from "lucide-react";
import { useI18n } from "../../i18n";
import {
  AudioConfig,
  AuthConfig,
  MqttConfig,
  NinjaConfigResponse,
  RecordConfig,
  ReviewConfig,
  SnapshotsConfig,
  UserAccountItem,
} from "../../types/nvr";
import { ToggleSwitch } from "./ConsoleAndStreamsPanels";

const ALERT_LABEL_CANDIDATES = [
  "person",
  "car",
  "bicycle",
  "motorcycle",
  "bus",
  "dog",
  "cat",
];

export const RecordingReviewSection: React.FC<{
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  scope: "global" | string;
  inspectorCam: string;
  notify: (msg: string) => void;
}> = ({ draftConfig, updateDraft, scope, inspectorCam, notify }) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";

  const defaultRec: RecordConfig = {
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

  const defaultSnap: SnapshotsConfig = {
    enabled: true,
    timestamp: false,
    bounding_box: true,
    crop: false,
    quality: 70,
  };

  const defaultRev: ReviewConfig = {
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

  const activeRec: RecordConfig =
    scope === "global"
      ? { ...defaultRec, ...(draftConfig.record || {}) }
      : { ...defaultRec, ...(draftConfig.cameras?.[inspectorCam]?.record || {}) };

  const activeSnap: SnapshotsConfig =
    scope === "global"
      ? { ...defaultSnap, ...(draftConfig.snapshots || {}) }
      : {
          ...defaultSnap,
          ...(draftConfig.cameras?.[inspectorCam]?.snapshots || {}),
        };

  const activeRev: ReviewConfig =
    scope === "global"
      ? {
          alerts: { ...defaultRev.alerts, ...(draftConfig.review?.alerts || {}) },
          detections: {
            ...defaultRev.detections,
            ...(draftConfig.review?.detections || {}),
          },
        }
      : {
          alerts: {
            ...defaultRev.alerts,
            ...(draftConfig.cameras?.[inspectorCam]?.review?.alerts || {}),
          },
          detections: {
            ...defaultRev.detections,
            ...(draftConfig.cameras?.[inspectorCam]?.review?.detections || {}),
          },
        };

  const camZoneNames = Object.keys(draftConfig.cameras?.[inspectorCam]?.zones || {});

  const mutateRec = (fn: (r: RecordConfig) => void) => {
    updateDraft((next) => {
      const cloned: RecordConfig = JSON.parse(JSON.stringify(activeRec));
      fn(cloned);
      if (scope === "global") {
        next.record = cloned;
      } else if (next.cameras[inspectorCam]) {
        next.cameras[inspectorCam].record = cloned;
      }
    });
  };

  const mutateSnap = (fn: (s: SnapshotsConfig) => void) => {
    updateDraft((next) => {
      const cloned: SnapshotsConfig = JSON.parse(JSON.stringify(activeSnap));
      fn(cloned);
      if (scope === "global") {
        next.snapshots = cloned;
      } else if (next.cameras[inspectorCam]) {
        next.cameras[inspectorCam].snapshots = cloned;
      }
    });
  };

  const mutateRev = (fn: (rv: ReviewConfig) => void) => {
    updateDraft((next) => {
      const cloned: ReviewConfig = JSON.parse(JSON.stringify(activeRev));
      fn(cloned);
      if (scope === "global") {
        next.review = cloned;
      } else if (next.cameras[inspectorCam]) {
        next.cameras[inspectorCam].review = cloned;
      }
    });
  };

  const defaultAudio: AudioConfig = {
    enabled: false,
    min_volume_dbfs: -60,
    labels: ["audio", "speech", "bark", "alarm"],
  };

  const activeAudio: AudioConfig =
    scope === "global"
      ? { ...defaultAudio, ...(draftConfig.audio || {}) }
      : {
          ...defaultAudio,
          ...(draftConfig.audio || {}),
          ...(draftConfig.cameras?.[inspectorCam]?.audio || {}),
        };

  const mutateAudio = (fn: (au: AudioConfig) => void) => {
    updateDraft((next) => {
      const cloned: AudioConfig = JSON.parse(JSON.stringify(activeAudio));
      fn(cloned);
      if (scope === "global") {
        next.audio = cloned;
      } else if (next.cameras[inspectorCam]) {
        next.cameras[inspectorCam].audio = cloned;
      }
    });
  };

  return (
    <div className="nvr-card-grid">
      {/* Recording & Retention Card */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt ? "GRAVAÇÃO MP4 & RETENÇÃO" : "MP4 RECORDING & RETENTION"} (
            {scope === "global" ? "GLOBAL" : inspectorCam.toUpperCase()})
          </span>
          <button
            type="button"
            className="nvr-btn"
            style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
            onClick={() => {
              updateDraft((next) => {
                Object.values(next.cameras).forEach((cam) => {
                  cam.record = JSON.parse(JSON.stringify(activeRec));
                });
              });
              notify("Applied recording & retention policy to all cameras");
            }}
          >
            {isPt ? "Aplicar a Todas" : "Apply to All Cameras"}
          </button>
        </div>
        <div className="nvr-cfg-card-body">
          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Ativar Gravação Direta de Segmentos MP4 (10s)"
                  : "Enable 10s Direct MP4 Recording"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Grava segmentos H.264/AAC contínuos ou acionados por eventos em disco."
                  : "Write continuous or event-gated H.264/AAC segments to disk."}
              </div>
            </div>
            <ToggleSwitch
              checked={activeRec.enabled}
              label="Enable Recording"
              onChange={(v) => mutateRec((r) => (r.enabled = v))}
            />
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 8 }}>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt ? "Dias Contínuos (24/7)" : "Continuous Days"}
              </div>
              <input
                type="number"
                step={0.5}
                min={0}
                max={365}
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={activeRec.continuous?.days ?? 14}
                onChange={(e) =>
                  mutateRec((r) => {
                    r.continuous = { days: Number(e.target.value) };
                  })
                }
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt ? "Dias c/ Movimento" : "Motion-Only Days"}
              </div>
              <input
                type="number"
                step={0.5}
                min={0}
                max={365}
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={activeRec.motion?.days ?? 0}
                onChange={(e) =>
                  mutateRec((r) => {
                    r.motion = { days: Number(e.target.value) };
                  })
                }
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt ? "Qualidade Preview" : "Preview Quality"}
              </div>
              <select
                className="nvr-select mono"
                style={{ width: "100%", marginTop: 3 }}
                value={activeRec.preview?.quality || "medium"}
                onChange={(e) =>
                  mutateRec((r) => {
                    r.preview = { quality: e.target.value };
                  })
                }
              >
                <option value="very_low">Very Low</option>
                <option value="low">Low</option>
                <option value="medium">Medium</option>
                <option value="high">High</option>
                <option value="very_high">Very High</option>
              </select>
            </div>
          </div>

          {/* Alerts Retention (Clean 2x2 Grid so labels never wrap and Retain Mode never clips) */}
          <div
            style={{
              padding: 10,
              background: "var(--bg-canvas)",
              border: "1px solid var(--border-hairline)",
              borderRadius: 3,
              display: "flex",
              flexDirection: "column",
              gap: 8,
            }}
          >
            <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-alert-text)" }}>
              {isPt ? "POLÍTICA DE RETENÇÃO DE ALERTAS" : "ALERT EVENT RETENTION POLICY"}
            </span>
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1.2fr", gap: 8 }}>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Dias de Retenção" : "Retain Days"}
                </div>
                <input
                  type="number"
                  min={0.5}
                  step={0.5}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.alerts?.retain?.days ?? 14}
                  onChange={(e) =>
                    mutateRec((r) => {
                      r.alerts!.retain.days = Number(e.target.value);
                    })
                  }
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Modo de Retenção" : "Retain Mode"}
                </div>
                <select
                  className="nvr-select mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.alerts?.retain?.mode || "motion"}
                  onChange={(e) =>
                    mutateRec((r) => {
                      r.alerts!.retain.mode = e.target.value;
                    })
                  }
                >
                  <option value="all">{isPt ? "Todos Segmentos" : "All Segments"}</option>
                  <option value="motion">
                    {isPt ? "Com Movimento" : "Motion Active"}
                  </option>
                  <option value="active_objects">
                    {isPt ? "Objetos Ativos" : "Active Objects"}
                  </option>
                </select>
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Pré-Captura (seg)" : "Pre-Capture (sec)"}
                </div>
                <input
                  type="number"
                  min={0}
                  max={60}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.alerts?.pre_capture ?? 5}
                  onChange={(e) =>
                    mutateRec((r) => {
                      r.alerts!.pre_capture = Number(e.target.value);
                    })
                  }
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Pós-Captura (seg)" : "Post-Capture (sec)"}
                </div>
                <input
                  type="number"
                  min={0}
                  max={60}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.alerts?.post_capture ?? 5}
                  onChange={(e) =>
                    mutateRec((r) => {
                      r.alerts!.post_capture = Number(e.target.value);
                    })
                  }
                />
              </div>
            </div>
          </div>

          {/* Detections Retention (Clean 2x2 Grid) */}
          <div
            style={{
              padding: 10,
              background: "var(--bg-canvas)",
              border: "1px solid var(--border-hairline)",
              borderRadius: 3,
              display: "flex",
              flexDirection: "column",
              gap: 8,
            }}
          >
            <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-motion-text)" }}>
              {isPt
                ? "POLÍTICA DE RETENÇÃO DE DETECÇÕES"
                : "DETECTION EVENT RETENTION POLICY"}
            </span>
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1.2fr", gap: 8 }}>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Dias de Retenção" : "Retain Days"}
                </div>
                <input
                  type="number"
                  min={0.5}
                  step={0.5}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.detections?.retain?.days ?? 14}
                  onChange={(e) =>
                    mutateRec((r) => {
                      r.detections!.retain.days = Number(e.target.value);
                    })
                  }
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Modo de Retenção" : "Retain Mode"}
                </div>
                <select
                  className="nvr-select mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.detections?.retain?.mode || "motion"}
                  onChange={(e) =>
                    mutateRec((r) => {
                      r.detections!.retain.mode = e.target.value;
                    })
                  }
                >
                  <option value="all">{isPt ? "Todos Segmentos" : "All Segments"}</option>
                  <option value="motion">
                    {isPt ? "Com Movimento" : "Motion Active"}
                  </option>
                  <option value="active_objects">
                    {isPt ? "Objetos Ativos" : "Active Objects"}
                  </option>
                </select>
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Pré-Captura (seg)" : "Pre-Capture (sec)"}
                </div>
                <input
                  type="number"
                  min={0}
                  max={60}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.detections?.pre_capture ?? 5}
                  onChange={(e) =>
                    mutateRec((r) => {
                      r.detections!.pre_capture = Number(e.target.value);
                    })
                  }
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Pós-Captura (seg)" : "Post-Capture (sec)"}
                </div>
                <input
                  type="number"
                  min={0}
                  max={60}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.detections?.post_capture ?? 5}
                  onChange={(e) =>
                    mutateRec((r) => {
                      r.detections!.post_capture = Number(e.target.value);
                    })
                  }
                />
              </div>
            </div>
          </div>

          {/* Hybrid Dual-Stream Recording & Disk Quota Retention */}
          <div
            style={{
              padding: 10,
              background: "var(--bg-canvas)",
              border: "1px solid var(--border-hairline)",
              borderRadius: 3,
              display: "flex",
              flexDirection: "column",
              gap: 8,
            }}
          >
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-accent-text)" }}>
                {isPt
                  ? "GRAVAÇÃO HÍBRIDA SUB/MAIN & COTA DE DISCO"
                  : "HYBRID DUAL-STREAM & DISK QUOTA"}
              </span>
            </div>

            <div className="nvr-field-row" style={{ padding: "2px 0" }}>
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? "Gravação Contínua Híbrida (24/7 Substream + Eventos em Main)"
                    : "Hybrid Dual-Stream (24/7 Substream + Main on Events)"}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Economiza até 80% de disco mantendo 24/7 no substream e promovendo eventos para 1080p/4K."
                    : "Saves up to 80% disk space by keeping idle 24/7 segments on substream and retaining Main Stream on events."}
                </div>
              </div>
              <ToggleSwitch
                checked={Boolean(activeRec.continuous_substream)}
                label="Hybrid Dual-Stream Recording"
                onChange={(v) => mutateRec((r) => (r.continuous_substream = v))}
              />
            </div>

            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8 }}>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Cota Máxima de Disco (GB)" : "Max Disk Quota (GB)"}
                </div>
                <input
                  type="number"
                  min={0}
                  step={5}
                  placeholder={isPt ? "Ilimitado (0)" : "Unlimited (0)"}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.max_size_gb ?? ""}
                  onChange={(e) => {
                    const val = e.target.value.trim();
                    mutateRec((r) => {
                      r.max_size_gb = val && Number(val) > 0 ? Number(val) : null;
                    });
                  }}
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Espaço Livre Mínimo (GB)" : "Min Free Space Guard (GB)"}
                </div>
                <input
                  type="number"
                  min={0}
                  step={1}
                  placeholder={isPt ? "Padrão (0)" : "Default (0)"}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.min_free_gb ?? ""}
                  onChange={(e) => {
                    const val = e.target.value.trim();
                    mutateRec((r) => {
                      r.min_free_gb = val && Number(val) > 0 ? Number(val) : null;
                    });
                  }}
                />
              </div>
            </div>
          </div>

          {/* Hot-to-Cold Tiered Storage & Real-Time Alert Clip Backup */}
          <div
            style={{
              padding: 10,
              background: "var(--bg-canvas)",
              border: "1px solid var(--border-hairline)",
              borderRadius: 3,
              display: "flex",
              flexDirection: "column",
              gap: 8,
            }}
          >
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-live-text)" }}>
                {isPt
                  ? "ARMAZENAMENTO EM CAMADAS HOT→COLD"
                  : "HOT-TO-COLD TIERED STORAGE"}
              </span>
              <ToggleSwitch
                checked={Boolean(activeRec.storage_tiers?.enabled)}
                label="Enable Hot-to-Cold Tiered Storage"
                onChange={(v) =>
                  mutateRec((r) => {
                    r.storage_tiers = {
                      enabled: v,
                      hot_dir: r.storage_tiers?.hot_dir || "/media/frigate/recordings",
                      cold_dir: r.storage_tiers?.cold_dir || "/media/frigate/archive",
                      migrate_after_hours: r.storage_tiers?.migrate_after_hours ?? 24,
                    };
                  })
                }
              />
            </div>
            <div style={{ display: "grid", gridTemplateColumns: "1.6fr 1fr", gap: 8 }}>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Diretório Cold Archive (NAS/HDD)" : "Cold Archive Path (NAS/HDD)"}
                </div>
                <input
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  placeholder="/media/frigate/archive"
                  value={activeRec.storage_tiers?.cold_dir || ""}
                  onChange={(e) =>
                    mutateRec((r) => {
                      r.storage_tiers = {
                        enabled: r.storage_tiers?.enabled ?? false,
                        hot_dir: r.storage_tiers?.hot_dir,
                        cold_dir: e.target.value || null,
                        migrate_after_hours: r.storage_tiers?.migrate_after_hours ?? 24,
                      };
                    })
                  }
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Migrar Após (Horas)" : "Migrate After (Hours)"}
                </div>
                <input
                  type="number"
                  min={1}
                  max={720}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeRec.storage_tiers?.migrate_after_hours ?? 24}
                  onChange={(e) =>
                    mutateRec((r) => {
                      r.storage_tiers = {
                        enabled: r.storage_tiers?.enabled ?? false,
                        hot_dir: r.storage_tiers?.hot_dir,
                        cold_dir: r.storage_tiers?.cold_dir,
                        migrate_after_hours: Number(e.target.value),
                      };
                    })
                  }
                />
              </div>
            </div>

            <div
              style={{
                borderTop: "1px solid var(--border-hairline)",
                paddingTop: 8,
                display: "flex",
                justifyContent: "space-between",
                alignItems: "center",
              }}
            >
              <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-alert-text)" }}>
                {isPt
                  ? "BACKUP EM TEMPO REAL DE CLIPES DE ALERTA"
                  : "REAL-TIME OFFSITE ALERT CLIP BACKUP"}
              </span>
              <ToggleSwitch
                checked={Boolean(activeRec.alert_backup?.enabled)}
                label="Enable Real-Time Alert Backup"
                onChange={(v) =>
                  mutateRec((r) => {
                    r.alert_backup = {
                      enabled: v,
                      target_dir: r.alert_backup?.target_dir || "/media/frigate/exports/backup",
                      severities: r.alert_backup?.severities || ["alert"],
                      include_snapshot: r.alert_backup?.include_snapshot ?? true,
                    };
                  })
                }
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt ? "Diretório de Backup de Alertas" : "Alert Backup Target Directory"}
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                placeholder="/media/frigate/exports/backup"
                value={activeRec.alert_backup?.target_dir || ""}
                onChange={(e) =>
                  mutateRec((r) => {
                    r.alert_backup = {
                      enabled: r.alert_backup?.enabled ?? false,
                      target_dir: e.target.value || null,
                      severities: r.alert_backup?.severities || ["alert"],
                      include_snapshot: r.alert_backup?.include_snapshot ?? true,
                    };
                  })
                }
              />
            </div>
          </div>
        </div>
      </div>

      {/* Snapshots & Review Classification Rules */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "SNAPSHOTS & CLASSIFICAÇÃO DE REVISÃO"
              : "SNAPSHOTS & REVIEW CLASSIFICATION"}{" "}
            ({scope === "global" ? "GLOBAL" : inspectorCam.toUpperCase()})
          </span>
        </div>
        <div className="nvr-cfg-card-body">
          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Salvar Snapshots de Objetos Rastreados"
                  : "Save Tracked Object Snapshots"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Persiste imagens JPEG/WebP quando um novo pico de confiança é atingido."
                  : "Persist JPEG/WebP thumbnails when a new peak confidence score is reached."}
              </div>
            </div>
            <ToggleSwitch
              checked={activeSnap.enabled}
              label="Enable Snapshots"
              onChange={(v) => mutateSnap((s) => (s.enabled = v))}
            />
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Qualidade JPEG/WebP" : "Snapshot Encode Quality"} (
                {activeSnap.quality ?? 70}%)
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Compressão da imagem salva em /clips (10–100)."
                  : "Compression quality for saved event snapshots (10–100)."}
              </div>
            </div>
            <input
              type="range"
              min={20}
              max={100}
              aria-label="Snapshot Quality"
              value={activeSnap.quality ?? 70}
              onChange={(e) => mutateSnap((s) => (s.quality = Number(e.target.value)))}
            />
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Snapshot Alta Resolução (Main Stream)"
                  : "High-Res Main-Stream Snapshots"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Projeta a caixa delimitadora do substream sobre o quadro 1080p/4K da câmera."
                  : "Projects normalized substream bounding boxes onto full-resolution main-stream frames."}
              </div>
            </div>
            <ToggleSwitch
              checked={
                (activeSnap.high_res ?? true) && (activeSnap.from_main_stream ?? true)
              }
              label="High-Res Main-Stream Snapshots"
              onChange={(v) =>
                mutateSnap((s) => {
                  s.high_res = v;
                  s.from_main_stream = v;
                })
              }
            />
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Política de Seleção de Snapshot" : "Snapshot Selection Policy"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Evita sobrescrever snapshots de zona quando o objeto sai da zona."
                  : "Controls whether out-of-zone higher scores can overwrite in-zone alert snapshots."}
              </div>
            </div>
            <select
              className="nvr-select mono"
              aria-label="Snapshot Selection Policy"
              value={activeSnap.selection || "best_in_zone"}
              onChange={(e) =>
                mutateSnap(
                  (s) =>
                    (s.selection = e.target.value as
                      | "best_overall"
                      | "best_in_zone"
                      | "first_trigger")
                )
              }
            >
              <option value="best_in_zone">
                {isPt ? "Melhor na Zona (best_in_zone)" : "Best in Zone (best_in_zone)"}
              </option>
              <option value="best_overall">
                {isPt ? "Melhor Geral (best_overall)" : "Best Overall (best_overall)"}
              </option>
              <option value="first_trigger">
                {isPt ? "Primeiro Gatilho (first_trigger)" : "First Trigger (first_trigger)"}
              </option>
            </select>
          </div>

          <div
            style={{
              display: "grid",
              gridTemplateColumns: "1fr 1fr 1fr",
              gap: 8,
              padding: "6px 8px",
              background: "var(--bg-canvas)",
              border: "1px solid var(--border-hairline)",
              borderRadius: 3,
            }}
          >
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 6 }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)" }}>
                Bounding Box
              </span>
              <ToggleSwitch
                checked={activeSnap.bounding_box ?? true}
                label="Snapshot Bounding Box"
                onChange={(v) => mutateSnap((s) => (s.bounding_box = v))}
              />
            </div>
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 6 }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)" }}>
                Timestamp
              </span>
              <ToggleSwitch
                checked={activeSnap.timestamp ?? false}
                label="Snapshot Timestamp"
                onChange={(v) => mutateSnap((s) => (s.timestamp = v))}
              />
            </div>
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 6 }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)" }}>
                {isPt ? "Recortar" : "Crop Box"}
              </span>
              <ToggleSwitch
                checked={activeSnap.crop ?? false}
                label="Snapshot Crop"
                onChange={(v) => mutateSnap((s) => (s.crop = v))}
              />
            </div>
          </div>

          {/* Review Alerts Classification */}
          <div
            style={{
              padding: 10,
              background: "var(--bg-canvas)",
              border: "1px solid rgba(239, 68, 68, 0.35)",
              borderRadius: 3,
              display: "flex",
              flexDirection: "column",
              gap: 8,
            }}
          >
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-alert-text)" }}>
                {isPt
                  ? `SEVERIDADE: ALERTAS (CORTE ${activeRev.alerts.cutoff_time}s)`
                  : `REVIEW SEVERITY: ALERTS (CUTOFF ${activeRev.alerts.cutoff_time}s)`}
              </span>
              <ToggleSwitch
                checked={activeRev.alerts.enabled}
                label="Enable Review Alerts"
                onChange={(v) => mutateRev((rv) => (rv.alerts.enabled = v))}
              />
            </div>

            <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
              {isPt ? "Classes que Geram Alerta:" : "Labels Qualifying as Alerts:"}
            </div>
            <div className="nvr-chip-group">
              {ALERT_LABEL_CANDIDATES.map((lbl) => {
                const labels = activeRev.alerts.labels || ["person", "car"];
                const on = labels.includes(lbl);
                return (
                  <button
                    key={lbl}
                    type="button"
                    className={`nvr-btn mono ${on ? "active" : ""}`}
                    style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                    onClick={() =>
                      mutateRev((rv) => {
                        const curr = rv.alerts.labels || ["person", "car"];
                        rv.alerts.labels = curr.includes(lbl)
                          ? curr.filter((x) => x !== lbl)
                          : [...curr, lbl];
                      })
                    }
                  >
                    {lbl}
                  </button>
                );
              })}
            </div>

            {scope !== "global" && camZoneNames.length > 0 && (
              <>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt
                    ? "Zonas Obrigatórias p/ Alerta (vazio = qualquer área):"
                    : "Required Zones for Alerts (empty = any area):"}
                </div>
                <div className="nvr-chip-group">
                  {camZoneNames.map((z) => {
                    const req = activeRev.alerts.required_zones || [];
                    const on = req.includes(z);
                    return (
                      <button
                        key={z}
                        type="button"
                        className={`nvr-btn mono ${on ? "active" : ""}`}
                        style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                        onClick={() =>
                          mutateRev((rv) => {
                            const curr = rv.alerts.required_zones || [];
                            rv.alerts.required_zones = curr.includes(z)
                              ? curr.filter((x) => x !== z)
                              : [...curr, z];
                          })
                        }
                      >
                        {z}
                      </button>
                    );
                  })}
                </div>
              </>
            )}

            <div className="nvr-field-row" style={{ paddingTop: 4, borderBottom: "none" }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt
                  ? "Tempo de Corte do Alerta (seg sem atividade)"
                  : "Alert Inactivity Cutoff (seconds)"}
              </span>
              <input
                type="number"
                min={15}
                max={300}
                className="nvr-input mono"
                style={{ width: 90 }}
                value={activeRev.alerts.cutoff_time}
                onChange={(e) =>
                  mutateRev((rv) => (rv.alerts.cutoff_time = Number(e.target.value)))
                }
              />
            </div>
          </div>

          {/* Review Detections Classification */}
          <div
            style={{
              padding: 10,
              background: "var(--bg-canvas)",
              border: "1px solid rgba(245, 158, 11, 0.35)",
              borderRadius: 3,
              display: "flex",
              flexDirection: "column",
              gap: 8,
            }}
          >
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-motion-text)" }}>
                {isPt
                  ? `SEVERIDADE: DETECÇÕES (CORTE ${activeRev.detections.cutoff_time}s)`
                  : `REVIEW SEVERITY: DETECTIONS (CUTOFF ${activeRev.detections.cutoff_time}s)`}
              </span>
              <ToggleSwitch
                checked={activeRev.detections.enabled}
                label="Enable Review Detections"
                onChange={(v) => mutateRev((rv) => (rv.detections.enabled = v))}
              />
            </div>
            <div className="nvr-field-row" style={{ borderBottom: "none" }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt
                  ? "Tempo de Corte da Detecção (seg sem atividade)"
                  : "Detection Inactivity Cutoff (seconds)"}
              </span>
              <input
                type="number"
                min={15}
                max={300}
                className="nvr-input mono"
                style={{ width: 90 }}
                value={activeRev.detections.cutoff_time}
                onChange={(e) =>
                  mutateRev((rv) => (rv.detections.cutoff_time = Number(e.target.value)))
                }
              />
            </div>
          </div>

          {/* Acoustic Event Detections */}
          <div
            style={{
              padding: 10,
              background: "var(--bg-canvas)",
              border: "1px solid rgba(6, 182, 212, 0.45)",
              borderRadius: 3,
              display: "flex",
              flexDirection: "column",
              gap: 8,
            }}
          >
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-accent-text)",
                }}
              >
                {isPt
                  ? `GATILHO ACÚSTICO / DETECÇÃO DE ÁUDIO (${activeAudio.min_volume_dbfs ?? -60} dBFS)`
                  : `ACOUSTIC EVENT DETECTIONS (${activeAudio.min_volume_dbfs ?? -60} dBFS)`}
              </span>
              <ToggleSwitch
                checked={activeAudio.enabled}
                label="Enable Audio Event Detections"
                onChange={(v) => mutateAudio((a) => (a.enabled = v))}
              />
            </div>

            <div className="nvr-field-row">
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? `Limiar de Volume Acústico (${activeAudio.min_volume_dbfs ?? -60} dBFS)`
                    : `Audio Trigger Threshold (${activeAudio.min_volume_dbfs ?? -60} dBFS)`}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Gera evento 'Audio Detections' quando o RMS do segmento supera este nível (ex.: -68 dBFS p/ música ambiente)."
                    : "Triggers an 'Audio Detections' review event when segment RMS exceeds this level (e.g. -68 dBFS for room music)."}
                </div>
              </div>
              <input
                type="range"
                min={-80}
                max={-10}
                step={1}
                aria-label="Audio Trigger Threshold dBFS"
                value={activeAudio.min_volume_dbfs ?? -60}
                onChange={(e) =>
                  mutateAudio((a) => (a.min_volume_dbfs = Number(e.target.value)))
                }
              />
            </div>

            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 6, flexWrap: "wrap" }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt ? "Presets Rápidos de Microfone:" : "Quick Mic Sensitivity Presets:"}
              </span>
              <div style={{ display: "flex", gap: 4 }}>
                {(
                  [
                    [-70, isPt ? "Música / Ambiente (-70 dBFS)" : "Music / Ambient (-70 dBFS)"],
                    [-58, isPt ? "Voz Normal (-58 dBFS)" : "Normal Voice (-58 dBFS)"],
                    [-38, isPt ? "Alarme / Latido Alto (-38 dBFS)" : "Loud Alarm / Bark (-38 dBFS)"],
                  ] as const
                ).map(([dbVal, lbl]) => (
                  <button
                    key={dbVal}
                    type="button"
                    className={`nvr-btn mono ${(activeAudio.min_volume_dbfs ?? -60) === dbVal ? "active" : ""}`}
                    style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                    onClick={() =>
                      mutateAudio((a) => {
                        a.enabled = true;
                        a.min_volume_dbfs = dbVal;
                      })
                    }
                  >
                    {lbl}
                  </button>
                ))}
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};

export const CommsAuthUsersSection: React.FC<{
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  notify: (msg: string) => void;
}> = ({ draftConfig, updateDraft, notify }) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";
  const [users, setUsers] = useState<UserAccountItem[]>([]);
  const [newUsername, setNewUsername] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [newRole, setNewRole] = useState<"admin" | "operator" | "viewer">("viewer");
  const [newAllowedCameras, setNewAllowedCameras] = useState("*");
  const [pwResetTarget, setPwResetTarget] = useState<string | null>(null);
  const [pwResetValue, setPwResetValue] = useState("");
  const [apiTokens, setApiTokens] = useState<
    Array<{
      id: string;
      name: string;
      prefix: string;
      scope: string;
      created_at: number;
    }>
  >([]);
  const [newTokenName, setNewTokenName] = useState("");
  const [newTokenScope, setNewTokenScope] = useState<"read_only" | "metrics" | "full">(
    "read_only"
  );
  const [createdPlaintextToken, setCreatedPlaintextToken] = useState<string | null>(null);
  const [newPeerId, setNewPeerId] = useState("");
  const [newPeerUrl, setNewPeerUrl] = useState("");
  const [newPeerTokenEnv, setNewPeerTokenEnv] = useState("NINJANVR_PEER_TOKEN");
  const [federationStatus, setFederationStatus] = useState<string | null>(null);

  const loadTokens = useCallback(async () => {
    try {
      const res = await fetch("/api/tokens", { credentials: "include" });
      if (res.ok) {
        const list = await res.json();
        if (Array.isArray(list)) setApiTokens(list);
      }
    } catch {
      // ignore
    }
  }, []);

  useEffect(() => {
    loadTokens();
  }, [loadTokens]);

  const handleCreateToken = async () => {
    if (!newTokenName.trim()) {
      notify("Enter a name for the API token");
      return;
    }
    const res = await fetch("/api/tokens", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ name: newTokenName.trim(), scope: newTokenScope }),
    });
    const data = await res.json().catch(() => ({}));
    if (res.ok && data.token) {
      setNewTokenName("");
      setCreatedPlaintextToken(data.token);
      if (Array.isArray(data.tokens)) setApiTokens(data.tokens);
      else loadTokens();
      notify("API Bearer token generated — copy it now (shown only once)");
    } else {
      notify(data.message || "Failed to generate API token");
    }
  };

  const handleRevokeToken = async (id: string) => {
    const res = await fetch(`/api/tokens/${encodeURIComponent(id)}`, {
      method: "DELETE",
      credentials: "include",
    });
    const data = await res.json().catch(() => ({}));
    if (res.ok) {
      if (Array.isArray(data.tokens)) setApiTokens(data.tokens);
      else loadTokens();
      notify("API token revoked");
    }
  };

  const loadUsers = useCallback(async () => {
    try {
      const res = await fetch("/api/users", { credentials: "include" });
      if (res.ok) {
        const list = await res.json();
        if (Array.isArray(list)) setUsers(list);
      }
    } catch {
      // ignore
    }
  }, []);

  useEffect(() => {
    loadUsers();
  }, [loadUsers]);

  const mqttCfg: MqttConfig = draftConfig.mqtt || {
    enabled: false,
    host: "127.0.0.1",
    port: 1883,
    topic_prefix: "frigate",
    client_id: "frigate",
    stats_interval: 60,
  };

  const authCfg: AuthConfig = draftConfig.auth || {
    enabled: true,
    reset_admin_password: false,
    cookie_name: "frigate_token",
    cookie_secure: false,
    session_length: 86400,
    refresh_time: 1800,
    hash_iterations: 600000,
    trusted_proxies: [],
  };

  const mutateMqtt = (patch: Partial<MqttConfig>) => {
    updateDraft((next) => {
      next.mqtt = { ...mqttCfg, ...patch };
    });
  };

  const mutateAuth = (patch: Partial<AuthConfig>) => {
    updateDraft((next) => {
      next.auth = { ...authCfg, ...patch };
    });
  };

  const handleCreateUser = async () => {
    if (!newUsername.trim() || !newPassword) {
      notify("Enter both a username and password");
      return;
    }
    const res = await fetch("/api/users", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({
        username: newUsername.trim(),
        password: newPassword,
        role: newRole,
        allowed_cameras: newAllowedCameras.trim() || "*",
      }),
    });
    const data = await res.json().catch(() => ({}));
    if (res.ok && data.success !== false) {
      setNewUsername("");
      setNewPassword("");
      setNewAllowedCameras("*");
      if (Array.isArray(data.users)) setUsers(data.users);
      else loadUsers();
      notify(data.message || "User created");
    } else {
      notify(data.message || "Failed to create user");
    }
  };

  const handleRoleChange = async (username: string, role: string) => {
    const res = await fetch(`/api/users/${encodeURIComponent(username)}/role`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ role }),
    });
    const data = await res.json().catch(() => ({}));
    if (res.ok && data.success !== false) {
      if (Array.isArray(data.users)) setUsers(data.users);
      else loadUsers();
      notify(`Updated role for '${username}' to ${role}`);
    } else {
      notify(data.message || "Failed to update role");
    }
  };

  const handleCamerasChange = async (username: string, allowed_cameras: string) => {
    const res = await fetch(`/api/users/${encodeURIComponent(username)}/cameras`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ allowed_cameras: allowed_cameras.trim() || "*" }),
    });
    const data = await res.json().catch(() => ({}));
    if (res.ok && data.success !== false) {
      if (Array.isArray(data.users)) setUsers(data.users);
      else loadUsers();
      notify(`Updated camera scope for '${username}'`);
    } else {
      notify(data.message || "Failed to update camera scope");
    }
  };

  const handlePasswordReset = async (username: string) => {
    if (!pwResetValue) return;
    const res = await fetch(`/api/users/${encodeURIComponent(username)}/password`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ password: pwResetValue }),
    });
    const data = await res.json().catch(() => ({}));
    if (res.ok && data.success !== false) {
      setPwResetTarget(null);
      setPwResetValue("");
      loadUsers();
      notify(`Updated password for '${username}'`);
    } else {
      notify(data.message || "Failed to update password");
    }
  };

  const handleDeleteUser = async (username: string) => {
    const res = await fetch(`/api/users/${encodeURIComponent(username)}`, {
      method: "DELETE",
      credentials: "include",
    });
    const data = await res.json().catch(() => ({}));
    if (res.ok && data.success !== false) {
      if (Array.isArray(data.users)) setUsers(data.users);
      else loadUsers();
      notify(`Deleted user '${username}'`);
    } else {
      notify(data.message || "Cannot delete user");
    }
  };

  return (
    <div className="nvr-card-grid">
      {/* MQTT Broker Configuration */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "BROKER MQTT & TELEMETRIA HOME ASSISTANT"
              : "MQTT TELEMETRY & HOME ASSISTANT BROKER"}
          </span>
          <ToggleSwitch
            checked={mqttCfg.enabled}
            label="Enable MQTT"
            onChange={(v) => mutateMqtt({ enabled: v })}
          />
        </div>
        <div className="nvr-cfg-card-body">
          <div style={{ display: "grid", gridTemplateColumns: "1.6fr 1fr", gap: 8 }}>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                Broker Host / IP
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={mqttCfg.host}
                onChange={(e) => mutateMqtt({ host: e.target.value })}
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                Broker Port
              </div>
              <input
                type="number"
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={mqttCfg.port}
                onChange={(e) => mutateMqtt({ port: Number(e.target.value) })}
              />
            </div>
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 8 }}>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                Topic Prefix
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={mqttCfg.topic_prefix}
                onChange={(e) => mutateMqtt({ topic_prefix: e.target.value })}
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                Client ID
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={mqttCfg.client_id}
                onChange={(e) => mutateMqtt({ client_id: e.target.value })}
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                Stats Interval (s)
              </div>
              <input
                type="number"
                min={5}
                max={600}
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={mqttCfg.stats_interval}
                onChange={(e) =>
                  mutateMqtt({ stats_interval: Number(e.target.value) })
                }
              />
            </div>
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8 }}>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt ? "Usuário MQTT (Opcional)" : "MQTT Username (Optional)"}
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                placeholder="Optional broker user"
                value={mqttCfg.user || ""}
                onChange={(e) =>
                  mutateMqtt({ user: e.target.value ? e.target.value : null })
                }
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt ? "Senha MQTT" : "MQTT Password"}{" "}
                {mqttCfg.has_password ? "(Preserved)" : ""}
              </div>
              <input
                type="password"
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                placeholder={
                  mqttCfg.has_password ? "•••••••• (preserved)" : "Optional password"
                }
                value={mqttCfg.password || ""}
                onChange={(e) =>
                  mutateMqtt({ password: e.target.value, clear_password: false })
                }
              />
            </div>
          </div>
        </div>
      </div>

      {/* Authentication, JWT & TLS Card */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "AUTENTICAÇÃO JWT, TLS & BANCO DE DADOS"
              : "JWT AUTHENTICATION, TLS & DATABASE"}
          </span>
          <ShieldCheck size={13} color="var(--signal-live-text)" />
        </div>
        <div className="nvr-cfg-card-body">
          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Exigir Autenticação JWT" : "Enforce JWT Authentication"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Exige login de operador por cookie ou Bearer token na UI e API."
                  : "Require operator login cookie or Bearer token for API and UI routes."}
              </div>
            </div>
            <ToggleSwitch
              checked={authCfg.enabled}
              label="Enforce JWT Authentication"
              onChange={(v) => mutateAuth({ enabled: v })}
            />
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Ativar Listener TLS (:8971)" : "Enable TLS Listener (:8971)"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Serve HTTPS/WSS com Rustls junto à porta HTTP interna :5000."
                  : "Serve HTTPS/WSS with Rustls alongside internal port :5000."}
              </div>
            </div>
            <ToggleSwitch
              checked={draftConfig.tls?.enabled ?? true}
              label="Enable TLS Listener"
              onChange={(v) =>
                updateDraft((next) => {
                  next.tls = { enabled: v };
                })
              }
            />
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 8 }}>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                Session Length (s)
              </div>
              <input
                type="number"
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={authCfg.session_length}
                onChange={(e) =>
                  mutateAuth({ session_length: Number(e.target.value) })
                }
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                Refresh Window (s)
              </div>
              <input
                type="number"
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={authCfg.refresh_time}
                onChange={(e) =>
                  mutateAuth({ refresh_time: Number(e.target.value) })
                }
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                PBKDF2 Iterations
              </div>
              <input
                type="number"
                min={10000}
                step={10000}
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={authCfg.hash_iterations}
                onChange={(e) =>
                  mutateAuth({ hash_iterations: Number(e.target.value) })
                }
              />
            </div>
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "1fr 1.4fr", gap: 8 }}>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                JWT Cookie Name
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={authCfg.cookie_name}
                onChange={(e) => mutateAuth({ cookie_name: e.target.value })}
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                SQLite WAL Database Path
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 3 }}
                value={draftConfig.database?.path || "/config/frigate.db"}
                onChange={(e) =>
                  updateDraft((next) => {
                    next.database = { path: e.target.value };
                  })
                }
              />
            </div>
          </div>
        </div>
      </div>

      {/* Operator User Accounts CRUD Card */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? `CONTAS DE OPERADOR & PERMISSÕES (${users.length} USUÁRIOS)`
              : `OPERATOR ACCOUNTS & ROLE ACCESS (${users.length} USERS)`}
          </span>
          <span className="nvr-badge live">SQLITE PBKDF2-SHA256</span>
        </div>
        <div className="nvr-cfg-card-body">
          <table className="nvr-table mono">
            <thead>
              <tr>
                <th>{isPt ? "Usuário" : "Username"}</th>
                <th>{isPt ? "Nível de Acesso" : "Access Role"}</th>
                <th>{isPt ? "Câmeras Permitidas" : "Allowed Cameras"}</th>
                <th>{isPt ? "Status da Senha" : "Password Status"}</th>
                <th>{isPt ? "Ações" : "Actions"}</th>
              </tr>
            </thead>
            <tbody>
              {users.map((u) => (
                <tr key={u.username}>
                  <td style={{ fontWeight: 700 }}>{u.username}</td>
                  <td>
                    <select
                      className="nvr-select mono"
                      style={{ height: 24, fontSize: "var(--text-xs)" }}
                      aria-label={`Role for ${u.username}`}
                      value={u.role}
                      onChange={(e) => handleRoleChange(u.username, e.target.value)}
                    >
                      <option value="admin">admin (Full Access)</option>
                      <option value="operator">operator (Triage & Export)</option>
                      <option value="viewer">viewer (Read-Only)</option>
                    </select>
                  </td>
                  <td>
                    <input
                      className="nvr-input mono"
                      style={{ height: 24, width: 140, fontSize: "var(--text-xs)" }}
                      aria-label={`Allowed cameras for ${u.username}`}
                      defaultValue={u.allowed_cameras || "*"}
                      disabled={u.role === "admin"}
                      onBlur={(e) => handleCamerasChange(u.username, e.target.value)}
                    />
                  </td>
                  <td style={{ color: "var(--text-secondary)" }}>
                    {u.password_changed_at
                      ? `Updated ${u.password_changed_at.slice(0, 10)}`
                      : "Provisioned"}
                  </td>
                  <td>
                    <div style={{ display: "flex", gap: 6, alignItems: "center" }}>
                      {pwResetTarget === u.username ? (
                        <>
                          <input
                            type="password"
                            className="nvr-input mono"
                            style={{ height: 24, width: 150, fontSize: "var(--text-xs)" }}
                            placeholder="New password..."
                            value={pwResetValue}
                            onChange={(e) => setPwResetValue(e.target.value)}
                          />
                          <button
                            type="button"
                            className="nvr-btn success"
                            style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                            onClick={() => handlePasswordReset(u.username)}
                          >
                            Save
                          </button>
                          <button
                            type="button"
                            className="nvr-btn"
                            style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                            onClick={() => {
                              setPwResetTarget(null);
                              setPwResetValue("");
                            }}
                          >
                            Cancel
                          </button>
                        </>
                      ) : (
                        <button
                          type="button"
                          className="nvr-btn"
                          style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                          onClick={() => {
                            setPwResetTarget(u.username);
                            setPwResetValue("");
                          }}
                        >
                          <KeyRound size={11} />
                          {isPt ? "Redefinir Senha" : "Reset Password"}
                        </button>
                      )}
                      <button
                        type="button"
                        className="nvr-btn danger"
                        style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
                        disabled={users.length <= 1}
                        title="Delete user account"
                        onClick={() => handleDeleteUser(u.username)}
                      >
                        <Trash2 size={11} />
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>

          <div
            style={{
              display: "flex",
              gap: 8,
              alignItems: "center",
              flexWrap: "wrap",
              paddingTop: 6,
            }}
          >
            <input
              className="nvr-input mono"
              style={{ width: 180 }}
              placeholder={isPt ? "Novo usuário" : "New username"}
              aria-label="New Account Username"
              value={newUsername}
              onChange={(e) => setNewUsername(e.target.value)}
            />
            <input
              type="password"
              className="nvr-input mono"
              style={{ width: 200 }}
              placeholder={isPt ? "Senha inicial" : "Initial password"}
              aria-label="New Account Password"
              value={newPassword}
              onChange={(e) => setNewPassword(e.target.value)}
            />
            <select
              className="nvr-select mono"
              aria-label="New Account Role"
              value={newRole}
              onChange={(e) =>
                setNewRole(e.target.value as "admin" | "operator" | "viewer")
              }
            >
              <option value="viewer">Role: viewer</option>
              <option value="operator">Role: operator</option>
              <option value="admin">Role: admin</option>
            </select>
            <input
              className="nvr-input mono"
              style={{ width: 160 }}
              placeholder="Allowed cams (* or cam1,cam2)"
              aria-label="New Account Allowed Cameras"
              value={newAllowedCameras}
              onChange={(e) => setNewAllowedCameras(e.target.value)}
            />
            <button
              type="button"
              className="nvr-btn primary"
              onClick={handleCreateUser}
            >
              <Plus size={12} />
              {isPt ? "Criar Conta de Operador" : "Create Operator Account"}
            </button>
          </div>
        </div>
      </div>

      {/* Scoped API Bearer Tokens Card */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? `TOKENS DE API SCOPED (${apiTokens.length})`
              : `SCOPED API BEARER TOKENS (${apiTokens.length})`}
          </span>
          <span className="nvr-badge mono">SHA-256 HASHED // nnvr_*</span>
        </div>
        <div className="nvr-cfg-card-body">
          {createdPlaintextToken && (
            <div
              className="mono"
              style={{
                padding: "8px 10px",
                background: "rgba(16, 185, 129, 0.14)",
                border: "1px solid var(--signal-live)",
                borderRadius: 3,
                display: "flex",
                justifyContent: "space-between",
                alignItems: "center",
                fontSize: "var(--text-xs)",
              }}
            >
              <span>
                NEW TOKEN (COPY NOW): <strong>{createdPlaintextToken}</strong>
              </span>
              <button
                type="button"
                className="nvr-btn success mono"
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  navigator.clipboard?.writeText(createdPlaintextToken);
                  notify("Copied API token to clipboard");
                }}
              >
                Copy Token
              </button>
            </div>
          )}

          {apiTokens.length > 0 && (
            <table className="nvr-table mono">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Prefix</th>
                  <th>Scope</th>
                  <th>Action</th>
                </tr>
              </thead>
              <tbody>
                {apiTokens.map((tk) => (
                  <tr key={tk.id}>
                    <td style={{ fontWeight: 700 }}>{tk.name}</td>
                    <td>{tk.prefix}...</td>
                    <td>{tk.scope}</td>
                    <td>
                      <button
                        type="button"
                        className="nvr-btn danger"
                        style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                        onClick={() => handleRevokeToken(tk.id)}
                      >
                        <Trash2 size={11} />
                        Revoke
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}

          <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
            <input
              className="nvr-input mono"
              style={{ width: 200 }}
              placeholder="Token name (e.g. prometheus)"
              aria-label="API Token Name"
              value={newTokenName}
              onChange={(e) => setNewTokenName(e.target.value)}
            />
            <select
              className="nvr-select mono"
              aria-label="API Token Scope"
              value={newTokenScope}
              onChange={(e) =>
                setNewTokenScope(e.target.value as "read_only" | "metrics" | "full")
              }
            >
              <option value="read_only">Scope: read_only (GET /api/*)</option>
              <option value="metrics">Scope: metrics (GET /api/stats*)</option>
              <option value="full">Scope: full (Admin)</option>
            </select>
            <button
              type="button"
              className="nvr-btn primary"
              onClick={handleCreateToken}
            >
              <Plus size={12} />
              Generate API Token
            </button>
          </div>
        </div>

        <div
          style={{
            marginTop: 18,
            paddingTop: 14,
            borderTop: "1px solid var(--border-hairline)",
            display: "flex",
            flexDirection: "column",
            gap: 10,
          }}
        >
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
            <strong className="mono" style={{ fontSize: 12, color: "var(--text-primary)" }}>
              {isPt
                ? "SSO OPENID CONNECT (AUTHELIA / ZITADEL) & FORWARDAUTH"
                : "OPENID CONNECT (OIDC) SSO & TRUSTED-PROXY FORWARDAUTH"}
            </strong>
            <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
              <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                OIDC SSO:
              </span>
              <ToggleSwitch
                checked={Boolean(authCfg.oidc?.enabled)}
                label="Enable OIDC SSO"
                onChange={(v) =>
                  mutateAuth({
                    oidc: {
                      enabled: v,
                      provider_name: authCfg.oidc?.provider_name || "Zitadel",
                      issuer_url: authCfg.oidc?.issuer_url || "",
                      client_id: authCfg.oidc?.client_id || "",
                      client_secret_env:
                        authCfg.oidc?.client_secret_env || "NINJANVR_OIDC_CLIENT_SECRET",
                      auto_redirect: authCfg.oidc?.auto_redirect ?? false,
                      default_role: authCfg.oidc?.default_role || "viewer",
                    },
                  })
                }
              />
              <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                ForwardAuth Proxy:
              </span>
              <ToggleSwitch
                checked={Boolean(authCfg.proxy?.enabled)}
                label="Enable Trusted-Proxy ForwardAuth"
                onChange={(v) =>
                  mutateAuth({
                    proxy: {
                      enabled: v,
                      trusted_proxies: authCfg.proxy?.trusted_proxies || ["127.0.0.1/32"],
                      user_header: authCfg.proxy?.user_header || "Remote-User",
                      groups_header: authCfg.proxy?.groups_header || "Remote-Groups",
                      logout_url: authCfg.proxy?.logout_url || "",
                    },
                  })
                }
              />
            </div>
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "1fr 1.6fr 1.2fr 0.8fr", gap: 8 }}>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                Provider Name
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                placeholder="Zitadel / Authelia"
                value={authCfg.oidc?.provider_name || "SSO"}
                onChange={(e) =>
                  mutateAuth({
                    oidc: {
                      ...(authCfg.oidc || { enabled: false }),
                      provider_name: e.target.value,
                    },
                  })
                }
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                OIDC Issuer URL
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                placeholder="https://auth.example.com"
                value={authCfg.oidc?.issuer_url || ""}
                onChange={(e) =>
                  mutateAuth({
                    oidc: {
                      ...(authCfg.oidc || { enabled: false }),
                      issuer_url: e.target.value,
                    },
                  })
                }
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                OIDC Client ID
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                placeholder="ninjanvr-web"
                value={authCfg.oidc?.client_id || ""}
                onChange={(e) =>
                  mutateAuth({
                    oidc: {
                      ...(authCfg.oidc || { enabled: false }),
                      client_id: e.target.value,
                    },
                  })
                }
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                Default Role
              </div>
              <select
                className="nvr-select mono"
                style={{ width: "100%", marginTop: 2 }}
                value={authCfg.oidc?.default_role || "viewer"}
                onChange={(e) =>
                  mutateAuth({
                    oidc: {
                      ...(authCfg.oidc || { enabled: false }),
                      default_role: e.target.value,
                    },
                  })
                }
              >
                <option value="viewer">viewer</option>
                <option value="operator">operator</option>
                <option value="admin">admin</option>
              </select>
            </div>
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "1.4fr 1fr 1fr", gap: 8 }}>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                Trusted Proxy CIDRs (comma-separated)
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                placeholder="127.0.0.1/32, 10.42.0.0/16"
                value={(authCfg.proxy?.trusted_proxies || authCfg.trusted_proxies || []).join(", ")}
                onChange={(e) => {
                  const list = e.target.value
                    .split(",")
                    .map((s) => s.trim())
                    .filter(Boolean);
                  mutateAuth({
                    trusted_proxies: list,
                    proxy: {
                      ...(authCfg.proxy || { enabled: false }),
                      trusted_proxies: list,
                    },
                  });
                }}
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                ForwardAuth User Header
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                placeholder="Remote-User"
                value={authCfg.proxy?.user_header || "Remote-User"}
                onChange={(e) =>
                  mutateAuth({
                    proxy: {
                      ...(authCfg.proxy || { enabled: false }),
                      user_header: e.target.value,
                    },
                  })
                }
              />
            </div>
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                ForwardAuth Groups Header
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                placeholder="Remote-Groups"
                value={authCfg.proxy?.groups_header || "Remote-Groups"}
                onChange={(e) =>
                  mutateAuth({
                    proxy: {
                      ...(authCfg.proxy || { enabled: false }),
                      groups_header: e.target.value,
                    },
                  })
                }
              />
            </div>
          </div>
        </div>
      </div>

      {/* Multi-Node NVR Federation (Single Glass Console) Card */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? `FEDERAÇÃO MULTI-NÓ NVR (${Object.keys(draftConfig.peers || {}).length + 1} NÓS)`
              : `MULTI-NODE NVR FEDERATION (${Object.keys(draftConfig.peers || {}).length + 1} NODES)`}
          </span>
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <button
              type="button"
              className="nvr-btn mono"
              style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
              onClick={async () => {
                try {
                  const res = await fetch("/api/federation/nodes", { credentials: "include" });
                  if (res.ok) {
                    const data = await res.json();
                    setFederationStatus(JSON.stringify(data, null, 2));
                    notify("Probed federated NVR topology (/api/federation/nodes)");
                  }
                } catch {
                  notify("Failed to query /api/federation/nodes");
                }
              }}
            >
              {isPt ? "Testar Topologia Federada" : "Probe Federated Nodes"}
            </button>
            <span className="nvr-badge live mono">SINGLE GLASS CONSOLE</span>
          </div>
        </div>
        <div className="nvr-cfg-card-body">
          <table className="nvr-table mono">
            <thead>
              <tr>
                <th>{isPt ? "Nó NVR" : "Node ID"}</th>
                <th>{isPt ? "Endereço / Papel" : "Endpoint / Role"}</th>
                <th>{isPt ? "Autenticação" : "Auth Token Env"}</th>
                <th>{isPt ? "Status" : "Status"}</th>
              </tr>
            </thead>
            <tbody>
              <tr>
                <td style={{ fontWeight: 700, color: "var(--signal-live-text)" }}>
                  local (Primary)
                </td>
                <td>{window.location.origin}</td>
                <td>Local Session</td>
                <td>
                  <span className="nvr-badge live mono">ONLINE</span>
                </td>
              </tr>
              {Object.entries(draftConfig.peers || {}).map(([peerId, peerCfg]) => (
                <tr key={peerId}>
                  <td style={{ fontWeight: 700 }}>{peerId}</td>
                  <td>{peerCfg.url}</td>
                  <td>{peerCfg.token_env || "NINJANVR_PEER_TOKEN"}</td>
                  <td>
                    <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                      <ToggleSwitch
                        checked={peerCfg.enabled}
                        label={`Enable peer ${peerId}`}
                        onChange={(v) =>
                          updateDraft((next) => {
                            if (next.peers && next.peers[peerId]) {
                              next.peers[peerId].enabled = v;
                            }
                          })
                        }
                      />
                      <button
                        type="button"
                        className="nvr-btn danger mono"
                        style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                        onClick={() =>
                          updateDraft((next) => {
                            if (next.peers) {
                              delete next.peers[peerId];
                            }
                          })
                        }
                      >
                        <Trash2 size={11} />
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>

          <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap", paddingTop: 6 }}>
            <input
              className="nvr-input mono"
              style={{ width: 150 }}
              placeholder={isPt ? "ID do nó (ex: sitio)" : "Peer ID (e.g. country_house)"}
              value={newPeerId}
              onChange={(e) => setNewPeerId(e.target.value)}
            />
            <input
              className="nvr-input mono"
              style={{ width: 260 }}
              placeholder="https://nvr-remote.internal:8971"
              value={newPeerUrl}
              onChange={(e) => setNewPeerUrl(e.target.value)}
            />
            <input
              className="nvr-input mono"
              style={{ width: 210 }}
              placeholder="NINJANVR_PEER_TOKEN"
              value={newPeerTokenEnv}
              onChange={(e) => setNewPeerTokenEnv(e.target.value)}
            />
            <button
              type="button"
              className="nvr-btn primary"
              onClick={() => {
                const slug = newPeerId.trim().toLowerCase().replace(/[^a-z0-9_-]/g, "_");
                if (!slug || !newPeerUrl.trim()) {
                  notify("Enter both a Peer ID and URL");
                  return;
                }
                updateDraft((next) => {
                  if (!next.peers) next.peers = {};
                  next.peers[slug] = {
                    enabled: true,
                    url: newPeerUrl.trim(),
                    token_env: newPeerTokenEnv.trim() || "NINJANVR_PEER_TOKEN",
                  };
                });
                setNewPeerId("");
                setNewPeerUrl("");
                notify(`Added federated NVR node '${slug}' (click Save Config to persist)`);
              }}
            >
              <Plus size={12} />
              {isPt ? "Adicionar Nó NVR" : "Add Federated Peer"}
            </button>
          </div>

          {federationStatus && (
            <pre
              className="mono"
              style={{
                margin: 0,
                padding: 8,
                background: "var(--bg-canvas)",
                border: "1px solid var(--border-hairline)",
                borderRadius: 3,
                fontSize: "var(--text-xs)",
                maxHeight: 160,
                overflowY: "auto",
              }}
            >
              {federationStatus}
            </pre>
          )}
        </div>
      </div>
    </div>
  );
};
