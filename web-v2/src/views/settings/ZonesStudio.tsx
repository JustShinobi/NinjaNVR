import React, { useCallback, useEffect, useMemo, useState } from "react";
import { LiveMsePlayer } from "../../components/VideoPlayers";
import { useI18n } from "../../i18n";
import { useNvrStore } from "../../store/useNvrStore";
import type { CameraMotionConfig, NinjaConfigResponse } from "../../types/nvr";
import {
  applyMotionProfile,
  commitVisionEditor,
  copyVisionItemToCamera,
  countChangedCameras,
  deleteVisionItem,
  detectMotionProfile,
  diffVisionConfig,
  editorStateFromItem,
  emptyEditorState,
  formatCoordPoints,
  formatDurationShort,
  isMaskKind,
  listCameraVisionItems,
  readMasks,
  resolveSaveStatus,
  maskKindLabel,
  measureBox,
  minPointsFor,
  MOTION_PROFILES,
  objectLabel,
  profileLabel,
  rectFromCorners,
  revertVisionChange,
  slugifyItemName,
  tripwireGuide,
  visionKindLabel,
  ZONE_OBJECT_CHOICES,
  type CommitError,
  type MaskKind,
  type MotionProfileId,
  type Point,
  type VisionChange,
  type VisionEditorState,
  type VisionItem,
  type VisionItemGroup,
  type VisionItemKind,
  type VisionItemRef,
} from "../../utils/zonesStudio";
import { ToggleSwitch } from "./ConsoleAndStreamsPanels";
import { ROI_ALERT_DURATION_PILLS, STATE_ROI_SCENARIO_PRESETS } from "./VisionAndAiPanels";

const KIND_COLOR: Record<VisionItemKind, string> = {
  zone: "#0ea5e9",
  line: "#10b981",
  state: "#f59e0b",
  motion_mask: "#ef4444",
  object_mask: "#a855f7",
  privacy_mask: "#64748b",
};

/** On-screen OSD clock area shortcut for motion masks (top-left corner). */
const OSD_CLOCK_POINTS: Point[] = [
  [0.01, 0.01],
  [0.34, 0.01],
  [0.34, 0.08],
  [0.01, 0.08],
];

const ROI_STATE_PT: Record<string, string> = {
  closed: "fechada",
  open: "aberta",
  empty: "vazia",
  occupied: "ocupada",
  package_present: "com pacote",
  present: "presente",
  missing: "sumiu",
};

type Mode = { name: "list" } | { name: "pick" } | { name: "edit"; editor: VisionEditorState };
type Tab = "areas" | "motion";

function refId(camera: string, ref: VisionItemRef): string {
  return `${camera}:${ref.kind}:${ref.key}`;
}

function sameRef(a: VisionItemRef | null, b: VisionItemRef | null): boolean {
  return Boolean(a && b && a.kind === b.kind && a.key === b.key);
}

function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || target.isContentEditable;
}

function clone<T>(v: T): T {
  return JSON.parse(JSON.stringify(v)) as T;
}

export interface SaveOutcome {
  ok: boolean;
  message: string;
  at: number;
}

function formatClock(ts: number, pt: boolean): string {
  return new Date(ts).toLocaleTimeString(pt ? "pt-BR" : "en-US", { hour: "2-digit", minute: "2-digit" });
}

export interface ZonesStudioProps {
  draftConfig: NinjaConfigResponse;
  savedConfig: NinjaConfigResponse | null;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  inspectorCam: string;
  draftPoints: Point[];
  setDraftPoints: React.Dispatch<React.SetStateAction<Point[]>>;
  notify: (msg: string) => void;
  reviewOpen: boolean;
  onOpenReview: () => void;
  onCloseReview: () => void;
  onSave: () => Promise<void> | void;
  onDiscard: () => void;
  isSaving: boolean;
  isDirty: boolean;
  saveOutcome: SaveOutcome | null;
}

export const ZonesStudio: React.FC<ZonesStudioProps> = (props) => {
  const {
    draftConfig,
    savedConfig,
    updateDraft,
    inspectorCam,
    draftPoints,
    setDraftPoints,
    notify,
    reviewOpen,
  } = props;
  const { locale } = useI18n();
  const pt = locale === "pt";
  const tr = useCallback((ptText: string, enText: string) => (pt ? ptText : enText), [pt]);

  const [tab, setTab] = useState<Tab>("areas");
  const [mode, setMode] = useState<Mode>({ name: "list" });
  const [selected, setSelected] = useState<VisionItemRef | null>(null);
  const [rulerOn, setRulerOn] = useState(false);
  const [rulerPoints, setRulerPoints] = useState<Point[]>([]);
  const [copyTarget, setCopyTarget] = useState("");
  const motionNow = useNvrStore((st) => st.cameraActivity[inspectorCam]?.motion);
  const liveStates = useNvrStore((st) => st.cameraActivity[inspectorCam]?.states);

  const camCfg = draftConfig.cameras?.[inspectorCam];
  const detW = camCfg?.detect?.width || 1280;
  const detH = camCfg?.detect?.height || 720;
  const cameraNames = Object.keys(draftConfig.cameras || {});

  const items = useMemo(() => listCameraVisionItems(camCfg, pt), [camCfg, pt]);
  const changes = useMemo(() => diffVisionConfig(savedConfig, draftConfig, pt), [savedConfig, draftConfig, pt]);
  const unsavedIds = useMemo(() => {
    const ids = new Set<string>();
    for (const c of changes) {
      if (c.camera !== inspectorCam) continue;
      if (isMaskKind(c.kind as VisionItemKind)) ids.add(`${c.camera}:${c.kind}:coords:${c.key}`);
      else ids.add(`${c.camera}:${c.kind}:${c.key}`);
    }
    return ids;
  }, [changes, inspectorCam]);

  const isUnsaved = (item: VisionItem): boolean => {
    if (isMaskKind(item.ref.kind)) {
      const coords = readMasks(camCfg, item.ref.kind)[Number(item.ref.key)] ?? "";
      return unsavedIds.has(`${inspectorCam}:${item.ref.kind}:coords:${coords}`);
    }
    return unsavedIds.has(refId(inspectorCam, item.ref));
  };

  // Reset transient UI when the operator switches camera.
  useEffect(() => {
    setMode({ name: "list" });
    setSelected(null);
    setRulerPoints([]);
    setCopyTarget("");
    setDraftPoints([]);
  }, [inspectorCam, setDraftPoints]);

  const editing = mode.name === "edit" ? mode.editor : null;

  const startNew = (kind: VisionItemKind) => {
    const base = emptyEditorState(kind);
    if (kind === "state") {
      const preset = STATE_ROI_SCENARIO_PRESETS[0];
      base.roiClasses = preset.classes.split(",").map((s) => s.trim());
      base.roiAlertState = preset.anomalyState;
      base.roiTimeoutSec = preset.anomalyTimeoutSec;
      base.roiIntervalSec = preset.intervalSec;
      base.roiHysteresis = preset.hysteresis;
    }
    setRulerOn(false);
    setSelected(null);
    setDraftPoints([]);
    setMode({ name: "edit", editor: base });
  };

  const startEdit = (ref: VisionItemRef) => {
    if (!camCfg) return;
    const loaded = editorStateFromItem(camCfg, ref);
    if (!loaded) return;
    setRulerOn(false);
    setDraftPoints(loaded.points);
    setMode({ name: "edit", editor: loaded.state });
  };

  const cancelEdit = useCallback(() => {
    setDraftPoints([]);
    setMode({ name: "list" });
  }, [setDraftPoints]);

  const patchEditor = (patch: Partial<VisionEditorState>) => {
    if (mode.name !== "edit") return;
    setMode({ name: "edit", editor: { ...mode.editor, ...patch } });
  };

  const commitError = (err: CommitError): string => {
    switch (err) {
      case "need_points":
        return editing?.kind === "line"
          ? tr("Marque os 2 pontos da linha no vídeo", "Mark both line points on the video")
          : editing?.kind === "state"
          ? tr("Marque os 2 cantos do local no vídeo", "Mark two opposite corners on the video")
          : tr("Marque pelo menos 3 pontos no vídeo", "Mark at least 3 points on the video");
      case "need_name":
        return tr("Dê um nome com pelo menos 2 letras", "Enter a name with at least 2 characters");
      case "name_taken":
        return tr("Já existe um item com esse nome nesta câmera", "This camera already has an item with that name");
      case "need_states":
        return tr("Defina pelo menos 2 estados", "Define at least 2 states");
      case "bad_shape":
        return tr(
          "As bordas da área se cruzam ou ela é pequena demais. Clique nos pontos em ordem, contornando a área.",
          "The area's edges cross or it is too small. Click the points in order around the area."
        );
      case "line_too_short":
        return tr("A linha é curta demais. Marque pontos mais distantes.", "The line is too short. Mark points further apart.");
      default:
        return tr("Câmera não encontrada", "Camera not found");
    }
  };

  const commit = () => {
    if (!editing) return;
    const trial = clone(draftConfig);
    const res = commitVisionEditor(trial, inspectorCam, editing, draftPoints);
    if (!res.ok) {
      notify(commitError(res.error));
      return;
    }
    updateDraft((next) => {
      next.cameras[inspectorCam] = trial.cameras[inspectorCam];
    });
    setSelected(res.ref);
    setDraftPoints([]);
    setMode({ name: "list" });
    notify(
      editing.originalKey === null
        ? tr("Adicionado. Revise e salve para valer nas câmeras.", "Added. Review and save to apply it.")
        : tr("Alteração aplicada. Revise e salve para valer nas câmeras.", "Change applied. Review and save to apply it.")
    );
  };

  const removeSelected = () => {
    if (!selected) return;
    const ref = selected;
    updateDraft((next) => deleteVisionItem(next, inspectorCam, ref));
    setSelected(null);
  };

  const copySelected = () => {
    if (!selected || !copyTarget) return;
    const trial = clone(draftConfig);
    const ref = copyVisionItemToCamera(trial, inspectorCam, selected, copyTarget);
    if (!ref) {
      notify(tr("Não foi possível copiar", "Could not copy"));
      return;
    }
    updateDraft((next) => {
      next.cameras[copyTarget] = trial.cameras[copyTarget];
    });
    notify(tr(`Copiado para ${copyTarget}`, `Copied to ${copyTarget}`));
    setCopyTarget("");
  };

  // Click handling on the video stage depends on what is being drawn.
  const onPlotPoint = (nx: number, ny: number) => {
    const p: Point = [nx, ny];
    if (rulerOn && mode.name === "list") {
      setRulerPoints((prev) => (prev.length >= 2 ? [p] : [...prev, p]));
      return;
    }
    if (!editing) return;
    if (editing.kind === "line") {
      setDraftPoints((prev) => (prev.length >= 2 ? [p] : [...prev, p]));
      return;
    }
    if (editing.kind === "state") {
      setDraftPoints((prev) => (prev.length === 1 ? rectFromCorners(prev[0], p) : [p]));
      return;
    }
    setDraftPoints((prev) => {
      if (prev.length >= 3) {
        const [fx, fy] = prev[0];
        if (Math.hypot(fx - nx, fy - ny) < 0.02) return prev;
      }
      return [...prev, p];
    });
  };

  useEffect(() => {
    if (mode.name !== "edit") return;
    const onKey = (e: KeyboardEvent) => {
      if (isTypingTarget(e.target)) return;
      if (e.key === "Escape") {
        e.preventDefault();
        cancelEdit();
      } else if (e.key === "Backspace") {
        e.preventDefault();
        setDraftPoints((prev) => prev.slice(0, -1));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [mode.name, cancelEdit, setDraftPoints]);

  // While editing an existing item, hide its saved shape so only the draft is drawn.
  const stageCamCfg = useMemo(() => {
    if (!camCfg) return camCfg;
    if (!editing || editing.originalKey === null) return camCfg;
    const tmp = clone(draftConfig);
    deleteVisionItem(tmp, inspectorCam, { kind: editing.kind, key: editing.originalKey });
    return tmp.cameras[inspectorCam];
  }, [camCfg, editing, draftConfig, inspectorCam]);

  if (!camCfg) {
    return (
      <div className="nvr-zs-empty">
        {tr("Escolha uma câmera para configurar.", "Pick a camera to configure.")}
      </div>
    );
  }

  if (reviewOpen) {
    return (
      <ReviewPanel
        pt={pt}
        changes={changes}
        onUndo={(c) => updateDraft((next) => revertVisionChange(next, savedConfig, c))}
        onBack={props.onCloseReview}
        onSave={props.onSave}
        onDiscard={props.onDiscard}
        isSaving={props.isSaving}
        isDirty={props.isDirty}
        saveOutcome={props.saveOutcome}
        notify={notify}
      />
    );
  }

  const selectedItem = selected ? items.find((i) => sameRef(i.ref, selected)) ?? null : null;
  const profile = detectMotionProfile(camCfg.motion);

  // ------------------------------------------------------------------ stage overlay
  const overlay = (
    <StageOverlay
      pt={pt}
      editing={editing}
      draftPoints={draftPoints}
      selected={mode.name === "list" && tab === "areas" ? selectedItem : null}
      rulerPoints={rulerOn && mode.name === "list" ? rulerPoints : []}
      stateRois={Object.entries(stageCamCfg?.state_rois || {}).map(([name, r]) => ({
        name,
        crop: r.crop || [0.25, 0.25, 0.75, 0.75],
        current: liveStates?.[name],
      }))}
      detW={detW}
      detH={detH}
    />
  );

  const drawing = mode.name === "edit" || (rulerOn && mode.name === "list");
  const hints =
    mode.name === "list" && tab === "motion"
      ? [tr("Movimento dentro das áreas vermelhas tracejadas é ignorado", "Motion inside dashed red areas is ignored")]
      : mode.name === "pick"
      ? [tr("Escolha o tipo à esquerda para começar a desenhar", "Pick a type on the left to start drawing")]
      : stageHints(pt, editing, rulerOn && mode.name === "list");
  const ruler = rulerPoints.length === 2 ? measureBox(rulerPoints[0], rulerPoints[1], detW, detH) : null;

  return (
    <div className="nvr-zs">
      <aside className="nvr-zs-panel" aria-label={tr("Configuração da câmera", "Camera configuration")}>
        {mode.name === "list" && (
          <>
            <SaveStatus
              pt={pt}
              pending={changes.length}
              isDirty={props.isDirty}
              isSaving={props.isSaving}
              outcome={props.saveOutcome}
              onReview={props.onOpenReview}
            />
            <div className="nvr-segmented nvr-zs-tabs" role="tablist">
              <button
                type="button"
                role="tab"
                aria-selected={tab === "areas"}
                className={`nvr-btn ${tab === "areas" ? "active" : ""}`}
                onClick={() => setTab("areas")}
              >
                {tr("Áreas e linhas", "Areas and lines")}
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={tab === "motion"}
                className={`nvr-btn ${tab === "motion" ? "active" : ""}`}
                onClick={() => setTab("motion")}
              >
                {tr("Sensibilidade de movimento", "Motion sensitivity")}
              </button>
            </div>
            {tab === "areas" ? (
              <ItemList
                pt={pt}
                items={items}
                selected={selected}
                isUnsaved={isUnsaved}
                onSelect={(ref) => setSelected(sameRef(ref, selected) ? null : ref)}
                onNew={() => setMode({ name: "pick" })}
                profileText={profileLabel(profile, pt)}
                onOpenMotion={() => setTab("motion")}
              />
            ) : (
              <MotionPanel
                pt={pt}
                motion={camCfg.motion}
                profile={profile}
                motionNow={motionNow}
                onPatch={(patch) =>
                  updateDraft((next) => {
                    const m = next.cameras[inspectorCam]?.motion;
                    if (m) Object.assign(m, patch);
                  })
                }
                onProfile={(id) =>
                  updateDraft((next) => {
                    const m = next.cameras[inspectorCam]?.motion;
                    if (m) applyMotionProfile(m, id);
                  })
                }
                onApplyAll={() => {
                  const src = camCfg.motion;
                  updateDraft((next) => {
                    Object.values(next.cameras).forEach((c) => {
                      c.motion = {
                        ...c.motion,
                        enabled: src.enabled ?? true,
                        threshold: src.threshold ?? 30,
                        contour_area: src.contour_area ?? 10,
                        lightning_threshold: src.lightning_threshold ?? 0.8,
                        improve_contrast: src.improve_contrast ?? true,
                        frame_height: src.frame_height ?? 100,
                      };
                    });
                  });
                  notify(tr("Valores copiados para todas as câmeras", "Values copied to every camera"));
                }}
              />
            )}
            {tab === "areas" && selectedItem && (
              <div className="nvr-zs-selection" aria-live="polite">
                <div className="nvr-zs-selection-text">
                  <strong>{selectedItem.title}</strong>
                  <span>{selectedItem.summary}</span>
                </div>
                <div className="nvr-zs-row">
                  <button type="button" className="nvr-btn sm primary" onClick={() => startEdit(selectedItem.ref)}>
                    {tr("Editar", "Edit")}
                  </button>
                  {cameraNames.length > 1 && (
                    <>
                      <select
                        className="nvr-select sm"
                        aria-label={tr("Copiar para a câmera", "Copy to camera")}
                        value={copyTarget}
                        onChange={(e) => setCopyTarget(e.target.value)}
                      >
                        <option value="">{tr("Copiar para…", "Copy to…")}</option>
                        {cameraNames
                          .filter((c) => c !== inspectorCam)
                          .map((c) => (
                            <option key={c} value={c}>
                              {c}
                            </option>
                          ))}
                      </select>
                      <button type="button" className="nvr-btn sm" disabled={!copyTarget} onClick={copySelected}>
                        {tr("Copiar", "Copy")}
                      </button>
                    </>
                  )}
                  <button type="button" className="nvr-btn sm danger" onClick={removeSelected}>
                    {tr("Excluir", "Delete")}
                  </button>
                </div>
              </div>
            )}
          </>
        )}

        {mode.name === "pick" && (
          <PickPanel pt={pt} onPick={startNew} onBack={() => setMode({ name: "list" })} />
        )}

        {editing && (
          <EditorPanel
            pt={pt}
            editor={editing}
            points={draftPoints}
            onPatch={patchEditor}
            onSetPoints={setDraftPoints}
            takenNames={
              editing.kind === "zone"
                ? Object.keys(camCfg.zones || {})
                : editing.kind === "line"
                ? Object.keys(camCfg.lines || {})
                : editing.kind === "state"
                ? Object.keys(camCfg.state_rois || {})
                : []
            }
            onCommit={commit}
            onCancel={cancelEdit}
            onChangeType={() => {
              setDraftPoints([]);
              setMode({ name: "pick" });
            }}
            detW={detW}
            detH={detH}
          />
        )}
      </aside>

      <section className="nvr-zs-stage" aria-label={tr("Vídeo ao vivo", "Live video")}>
        <div className="nvr-zs-stage-bar">
          <div className="nvr-zs-hints">
            {hints.map((h) => (
              <span key={h}>{h}</span>
            ))}
          </div>
          {mode.name === "list" && tab === "areas" && (
            <button
              type="button"
              aria-pressed={rulerOn}
              className={`nvr-btn sm ${rulerOn ? "active" : ""}`}
              onClick={() => {
                setRulerOn((v) => !v);
                setRulerPoints([]);
              }}
            >
              {tr("Régua", "Ruler")}
            </button>
          )}
        </div>
        <div className="nvr-zs-video nvr-cam-tile">
          <LiveMsePlayer
            camera={inspectorCam}
            useMainStream={false}
            cameraConfig={stageCamCfg}
            showVectors={true}
            draftPoints={editing ? draftPoints : []}
            draftColor={editing ? KIND_COLOR[editing.kind] : undefined}
            onPlotPoint={drawing ? onPlotPoint : undefined}
            stageOverlay={overlay}
          />
        </div>
        {mode.name === "list" && rulerOn && (
          <div className="nvr-zs-readout">
            <div className="nvr-zs-readout-text">
              <strong>{tr("Régua", "Ruler")}</strong>
              <span>
                {tr(
                  "Clique em 2 cantos opostos de um objeto para saber o tamanho dele. Serve para ajustar os filtros de tamanho na aba de IA.",
                  "Click two opposite corners of an object to measure it. Use it to tune size filters in the AI tab."
                )}
              </span>
            </div>
            {ruler && (
              <div className="nvr-zs-readout-values mono">
                <span>
                  {ruler.widthPx} × {ruler.heightPx} px
                </span>
                <span>
                  {tr("área", "area")} <strong>{ruler.areaPx.toLocaleString(pt ? "pt-BR" : "en-US")} px²</strong>
                </span>
                <span>
                  <strong>{(ruler.areaFraction * 100).toFixed(1)}%</strong> {tr("do quadro", "of frame")}
                </span>
                <span>
                  {tr("proporção", "ratio")} <strong>{ruler.ratio.toFixed(2)}</strong>
                </span>
              </div>
            )}
          </div>
        )}
        {mode.name === "list" && !rulerOn && (
          <div className="nvr-zs-legend" aria-label={tr("Legenda", "Legend")}>
            <span><i style={{ background: KIND_COLOR.zone }} />{tr("Gera alertas", "Raises alerts")}</span>
            <span><i style={{ background: KIND_COLOR.motion_mask }} />{tr("Ignorada", "Ignored")}</span>
            <span><i style={{ background: KIND_COLOR.privacy_mask }} />{tr("Privacidade", "Privacy")}</span>
          </div>
        )}
      </section>
    </div>
  );
};

function stageHints(pt: boolean, editing: VisionEditorState | null, ruler: boolean): string[] {
  if (ruler) return [pt ? "Clique em 2 cantos opostos para medir" : "Click two opposite corners to measure"];
  if (!editing) {
    return [pt ? "Clique num item da lista para destacá-lo no vídeo" : "Select an item in the list to highlight it"];
  }
  if (editing.kind === "line") {
    return pt
      ? ["Clique no ponto 1 e depois no ponto 2", "Um 3º clique recomeça", "Esc cancela"]
      : ["Click point 1, then point 2", "A third click starts over", "Esc cancels"];
  }
  if (editing.kind === "state") {
    return pt
      ? ["Clique em 2 cantos opostos do local", "Um 3º clique recomeça", "Esc cancela"]
      : ["Click two opposite corners", "A third click starts over", "Esc cancels"];
  }
  return pt
    ? ["Clique para adicionar ponto", "Backspace desfaz", "Esc cancela"]
    : ["Click to add a point", "Backspace undoes", "Esc cancels"];
}

// ------------------------------------------------------------------------------------------------
// Stage overlay: selection highlight, tripwire sides, ruler box
// ------------------------------------------------------------------------------------------------

const StageOverlay: React.FC<{
  pt: boolean;
  editing: VisionEditorState | null;
  draftPoints: Point[];
  selected: VisionItem | null;
  rulerPoints: Point[];
  stateRois: Array<{ name: string; crop: [number, number, number, number]; current?: string }>;
  detW: number;
  detH: number;
}> = ({ pt, editing, draftPoints, selected, rulerPoints, stateRois, detW, detH }) => {
  const pct = (v: number) => `${(v * 100).toFixed(2)}%`;
  const svgPoints = (pts: Point[]) => pts.map(([x, y]) => `${x * 1000},${y * 1000}`).join(" ");
  const showLine = editing?.kind === "line" && draftPoints.length === 2;
  const guide = showLine ? tripwireGuide(draftPoints[0], draftPoints[1]) : null;
  return (
    <>
      <svg viewBox="0 0 1000 1000" preserveAspectRatio="none" className="nvr-zs-overlay-svg" aria-hidden="true">
        {stateRois.map((r) => (
          <polygon
            key={`roi-${r.name}`}
            points={svgPoints(rectFromCorners([r.crop[0], r.crop[1]], [r.crop[2], r.crop[3]]))}
            fill="rgba(245,158,11,0.10)"
            stroke={KIND_COLOR.state}
            strokeWidth="2"
            vectorEffect="non-scaling-stroke"
          />
        ))}
        {selected && selected.outline.length >= 2 && (
          selected.ref.kind === "line" ? (
            <line
              x1={selected.outline[0][0] * 1000}
              y1={selected.outline[0][1] * 1000}
              x2={selected.outline[1][0] * 1000}
              y2={selected.outline[1][1] * 1000}
              stroke="#ffffff"
              strokeWidth="3"
              strokeDasharray="10 6"
              vectorEffect="non-scaling-stroke"
            />
          ) : (
            <polygon
              points={svgPoints(selected.outline)}
              fill="none"
              stroke="#ffffff"
              strokeWidth="3"
              strokeDasharray="10 6"
              vectorEffect="non-scaling-stroke"
            />
          )
        )}
        {rulerPoints.length === 2 && (
          <polygon
            points={svgPoints(rectFromCorners(rulerPoints[0], rulerPoints[1]))}
            fill="rgba(56,189,248,0.10)"
            stroke="#38bdf8"
            strokeWidth="2"
            strokeDasharray="6 4"
            vectorEffect="non-scaling-stroke"
          />
        )}
      </svg>
      {stateRois.map((r) => (
        <span
          key={`roi-tag-${r.name}`}
          className="nvr-zs-tag"
          style={{
            left: pct(r.crop[0]),
            top: pct(r.crop[1]),
            background: KIND_COLOR.state,
            transform: r.crop[1] < 0.06 ? "translate(0, 4px)" : undefined,
          }}
        >
          {r.name}
          {r.current ? ` · ${r.current}` : ""}
        </span>
      ))}
      {selected && selected.outline[0] && (
        <span
          className="nvr-zs-tag"
          style={{
            left: pct(selected.outline[0][0]),
            top: pct(selected.outline[0][1]),
            background: KIND_COLOR[selected.ref.kind],
            transform: selected.outline[0][1] < 0.06 ? "translate(0, 4px)" : undefined,
          }}
        >
          {selected.title}
        </span>
      )}
      {guide && (
        <>
          <span className="nvr-zs-side nvr-zs-side-a" style={{ left: pct(guide.sideA[0]), top: pct(guide.sideA[1]) }}>
            {pt ? "LADO A" : "SIDE A"}
          </span>
          <span className="nvr-zs-side nvr-zs-side-b" style={{ left: pct(guide.sideB[0]), top: pct(guide.sideB[1]) }}>
            {pt ? "LADO B" : "SIDE B"}
          </span>
          <span className="nvr-zs-point-num" style={{ left: pct(draftPoints[0][0]), top: pct(draftPoints[0][1]) }}>1</span>
          <span className="nvr-zs-point-num" style={{ left: pct(draftPoints[1][0]), top: pct(draftPoints[1][1]) }}>2</span>
        </>
      )}
      {rulerPoints.length === 2 && (
        <span
          className="nvr-zs-tag nvr-zs-tag-ruler mono"
          style={{
            left: pct(Math.min(rulerPoints[0][0], rulerPoints[1][0])),
            top: pct(Math.max(rulerPoints[0][1], rulerPoints[1][1])),
          }}
        >
          {measureBox(rulerPoints[0], rulerPoints[1], detW, detH).widthPx} ×{" "}
          {measureBox(rulerPoints[0], rulerPoints[1], detW, detH).heightPx} px
        </span>
      )}
    </>
  );
};

// ------------------------------------------------------------------------------------------------
// Item list
// ------------------------------------------------------------------------------------------------

const GROUP_ORDER: VisionItemGroup[] = ["alerts", "ignored", "privacy"];

const ItemList: React.FC<{
  pt: boolean;
  items: VisionItem[];
  selected: VisionItemRef | null;
  isUnsaved: (item: VisionItem) => boolean;
  onSelect: (ref: VisionItemRef) => void;
  onNew: () => void;
  profileText: string;
  onOpenMotion: () => void;
}> = ({ pt, items, selected, isUnsaved, onSelect, onNew, profileText, onOpenMotion }) => {
  const groupMeta: Record<VisionItemGroup, { title: string; hint: string; empty: string }> = {
    alerts: {
      title: pt ? "GERAM ALERTAS" : "RAISE ALERTS",
      hint: pt ? "zona, linha, estado" : "zone, line, state",
      empty: pt ? "Nenhum alerta configurado. A câmera alerta em qualquer lugar da imagem." : "No alert areas. The camera alerts anywhere in the frame.",
    },
    ignored: {
      title: pt ? "IGNORADAS" : "IGNORED",
      hint: pt ? "reduzem falso alarme" : "reduce false alarms",
      empty: pt ? "Nenhuma área ignorada." : "No ignored areas.",
    },
    privacy: {
      title: pt ? "PRIVACIDADE" : "PRIVACY",
      hint: pt ? "tarja no console, fotos e exportações" : "redacted in console, snapshots, exports",
      empty: pt ? "Nenhuma tarja nesta câmera." : "No redaction on this camera.",
    },
  };
  return (
    <div className="nvr-zs-scroll">
      <button type="button" className="nvr-zs-new" onClick={onNew}>
        {pt ? "+ Nova área ou linha" : "+ New area or line"}
      </button>
      {GROUP_ORDER.map((group) => {
        const list = items.filter((i) => i.group === group);
        const meta = groupMeta[group];
        return (
          <section key={group} className="nvr-zs-group">
            <header className="nvr-zs-group-head">
              <h3>{meta.title}</h3>
              <span>{meta.hint}</span>
            </header>
            {list.length === 0 ? (
              <p className="nvr-zs-muted">{meta.empty}</p>
            ) : (
              <ul className="nvr-zs-items">
                {list.map((item) => {
                  const active = sameRef(item.ref, selected);
                  return (
                    <li key={`${item.ref.kind}:${item.ref.key}`}>
                      <button
                        type="button"
                        className={`nvr-zs-item ${active ? "is-active" : ""}`}
                        aria-pressed={active}
                        onClick={() => onSelect(item.ref)}
                      >
                        <i style={{ background: KIND_COLOR[item.ref.kind] }} aria-hidden="true" />
                        <span className="nvr-zs-item-text">
                          <strong>{item.title}</strong>
                          <span>{item.summary}</span>
                        </span>
                        {isUnsaved(item) && <span className="nvr-zs-unsaved">{pt ? "não salvo" : "unsaved"}</span>}
                      </button>
                    </li>
                  );
                })}
              </ul>
            )}
          </section>
        );
      })}
      <p className="nvr-zs-muted">
        {pt ? "Movimento" : "Motion"}: <strong>{profileText}</strong>.{" "}
        <button type="button" className="nvr-zs-link" onClick={onOpenMotion}>
          {pt ? "Ajustar sensibilidade" : "Tune sensitivity"}
        </button>
      </p>
    </div>
  );
};

// ------------------------------------------------------------------------------------------------
// Type picker
// ------------------------------------------------------------------------------------------------

const PickPanel: React.FC<{ pt: boolean; onPick: (k: VisionItemKind) => void; onBack: () => void }> = ({
  pt,
  onPick,
  onBack,
}) => {
  const groups: Array<{ title: string; hint: string; options: Array<{ kind: VisionItemKind; title: string; tag: string; desc: string; example: string }> }> = [
    {
      title: pt ? "RECEBER ALERTAS" : "GET ALERTS",
      hint: pt ? "gera eventos e notificações" : "creates events and notifications",
      options: [
        {
          kind: "zone",
          title: pt ? "Avisar quando algo entrar numa área" : "Alert when something enters an area",
          tag: pt ? "ZONA" : "ZONE",
          desc: pt ? "Desenhe a área. Escolha quais objetos contam e se o alerta é na entrada ou só se ficar parado." : "Draw the area, choose which objects count and whether to alert on entry or after a while.",
          example: pt ? "Ex.: portão, garagem, piscina" : "e.g. gate, driveway, pool",
        },
        {
          kind: "line",
          title: pt ? "Avisar quando algo cruzar uma linha" : "Alert when something crosses a line",
          tag: pt ? "LINHA" : "LINE",
          desc: pt ? "Dispara no momento da travessia, num sentido ou nos dois." : "Fires at the moment of crossing, one way or both.",
          example: pt ? "Ex.: entrada da garagem, calçada" : "e.g. driveway entrance, sidewalk",
        },
        {
          kind: "state",
          title: pt ? "Acompanhar o estado de um local" : "Track the state of a spot",
          tag: pt ? "ESTADO" : "STATE",
          desc: pt ? "Lê se a porta está aberta, a vaga ocupada, se chegou pacote." : "Reads whether a door is open, a spot taken, a package delivered.",
          example: pt ? "Ex.: alerta se o portão ficar aberto 5 min" : "e.g. alert if the gate stays open 5 min",
        },
      ],
    },
    {
      title: pt ? "REDUZIR FALSO ALARME" : "REDUCE FALSE ALARMS",
      hint: pt ? "não gera alerta" : "never alerts",
      options: [
        {
          kind: "motion_mask",
          title: pt ? "Ignorar movimento numa área" : "Ignore motion in an area",
          tag: pt ? "MOVIMENTO" : "MOTION",
          desc: pt ? "Movimento ali não acorda a detecção. Um objeto que vem de fora continua sendo visto." : "Motion there does not wake detection. Objects coming from outside are still seen.",
          example: pt ? "Ex.: relógio da câmera, árvore, TV" : "e.g. camera clock, tree, TV",
        },
        {
          kind: "object_mask",
          title: pt ? "Ignorar objetos numa área" : "Ignore objects in an area",
          tag: pt ? "OBJETOS" : "OBJECTS",
          desc: pt ? "A detecção roda, mas objetos com a base (pés, rodas) dentro da área são descartados." : "Detection runs, but objects whose base (feet, wheels) is inside are dropped.",
          example: pt ? "Ex.: rua do vizinho, cartaz, estátua" : "e.g. neighbour's street, poster, statue",
        },
      ],
    },
    {
      title: pt ? "PROTEGER PRIVACIDADE" : "PROTECT PRIVACY",
      hint: pt ? "aplicado na imagem" : "applied to the image",
      options: [
        {
          kind: "privacy_mask",
          title: pt ? "Cobrir parte da imagem" : "Cover part of the image",
          tag: pt ? "TARJA" : "REDACT",
          desc: pt ? "Tarja preta no ao vivo do console, nas fotos de eventos e nas exportações. O arquivo gravado não é alterado." : "Black box in the console live view, event snapshots and exports. The recorded file is not altered.",
          example: pt ? "Ex.: janela do vizinho" : "e.g. neighbour's window",
        },
      ],
    },
  ];
  return (
    <div className="nvr-zs-scroll">
      <div className="nvr-zs-editor-head">
        <div>
          <span className="nvr-zs-kicker mono">{pt ? "PASSO 1 DE 3" : "STEP 1 OF 3"}</span>
          <strong>{pt ? "O que você quer fazer?" : "What do you want to do?"}</strong>
        </div>
        <button type="button" className="nvr-zs-link" onClick={onBack}>
          {pt ? "Voltar" : "Back"}
        </button>
      </div>
      {groups.map((g) => (
        <section key={g.title} className="nvr-zs-group">
          <header className="nvr-zs-group-head">
            <h3>{g.title}</h3>
            <span>{g.hint}</span>
          </header>
          <ul className="nvr-zs-items">
            {g.options.map((o) => (
              <li key={o.kind}>
                <button type="button" className="nvr-zs-option" onClick={() => onPick(o.kind)}>
                  <i style={{ background: KIND_COLOR[o.kind] }} aria-hidden="true" />
                  <span className="nvr-zs-item-text">
                    <span className="nvr-zs-option-title">
                      <strong>{o.title}</strong>
                      <span className="nvr-zs-option-tag mono">{o.tag}</span>
                    </span>
                    <span>{o.desc}</span>
                    <span className="nvr-zs-muted">{o.example}</span>
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      ))}
      <p className="nvr-zs-muted">
        {pt
          ? "Precisa medir um objeto? Use a Régua na barra do vídeo."
          : "Need to measure an object? Use the Ruler on the video bar."}
      </p>
    </div>
  );
};

// ------------------------------------------------------------------------------------------------
// Editor
// ------------------------------------------------------------------------------------------------

const EditorPanel: React.FC<{
  pt: boolean;
  editor: VisionEditorState;
  points: Point[];
  onPatch: (p: Partial<VisionEditorState>) => void;
  onSetPoints: React.Dispatch<React.SetStateAction<Point[]>>;
  takenNames: string[];
  onCommit: () => void;
  onCancel: () => void;
  onChangeType: () => void;
  detW: number;
  detH: number;
}> = ({ pt, editor, points, onPatch, onSetPoints, takenNames, onCommit, onCancel, onChangeType }) => {
  const tr = (a: string, b: string) => (pt ? a : b);
  const isNew = editor.originalKey === null;
  const k = editor.kind;
  const need = minPointsFor(k);
  const drawn = points.length >= need;

  const heads: Record<VisionItemKind, [string, string]> = {
    zone: [tr("ZONA", "ZONE"), tr("Avisar quando algo entrar numa área", "Alert when something enters an area")],
    line: [tr("LINHA", "LINE"), tr("Avisar quando algo cruzar uma linha", "Alert when something crosses a line")],
    state: [tr("ESTADO", "STATE"), tr("Acompanhar o estado de um local", "Track the state of a spot")],
    motion_mask: [tr("MÁSCARA", "MASK"), tr("Reduzir falso alarme ou esconder uma área", "Reduce false alarms or hide an area")],
    object_mask: [tr("MÁSCARA", "MASK"), tr("Reduzir falso alarme ou esconder uma área", "Reduce false alarms or hide an area")],
    privacy_mask: [tr("MÁSCARA", "MASK"), tr("Reduzir falso alarme ou esconder uma área", "Reduce false alarms or hide an area")],
  };
  const [kicker, title] = heads[k];

  const drawStatus = (() => {
    if (k === "line") {
      return drawn
        ? tr("Linha marcada · 2 pontos", "Line marked · 2 points")
        : tr(`Marque o ponto ${points.length + 1} de 2`, `Mark point ${points.length + 1} of 2`);
    }
    if (k === "state") {
      return drawn
        ? tr("Retângulo marcado", "Rectangle marked")
        : points.length === 1
        ? tr("Clique no canto oposto", "Click the opposite corner")
        : tr("Clique no primeiro canto", "Click the first corner");
    }
    return drawn
      ? tr(`Área com ${points.length} pontos`, `Area with ${points.length} points`)
      : tr(`${points.length} de pelo menos 3 pontos`, `${points.length} of at least 3 points`);
  })();

  const slug = slugifyItemName(editor.name);
  const nameTaken = slug.length >= 2 && slug !== editor.originalKey && takenNames.includes(slug);
  const coordText =
    k === "line" && points.length === 2
      ? `${formatCoordPoints(points, 4)} · direction: ${editor.direction}`
      : k === "state" && points.length >= 2
      ? `crop: ${formatCoordPoints([points[0], points[2] ?? points[1]], 3)}`
      : formatCoordPoints(points);

  return (
    <>
      <div className="nvr-zs-editor-head">
        <div>
          <span className="nvr-zs-kicker mono" style={{ color: KIND_COLOR[k] }}>
            {kicker} · {isNew ? tr("NOVO", "NEW") : tr("EDITANDO", "EDITING")}
          </span>
          <strong>{title}</strong>
        </div>
        {isNew && (
          <button type="button" className="nvr-zs-link" onClick={onChangeType}>
            {tr("Trocar tipo", "Change type")}
          </button>
        )}
      </div>
      <div className="nvr-zs-scroll">
        {isMaskKind(k) && (
          <MaskKindPicker pt={pt} kind={k} locked={!isNew} onChange={(kind) => onPatch({ kind })} />
        )}

        {k === "state" && (
          <section className="nvr-zs-section">
            <h4>{tr("O QUE É ESTE LOCAL?", "WHAT IS THIS SPOT?")}</h4>
            <div className="nvr-zs-grid2">
              {STATE_ROI_SCENARIO_PRESETS.map((preset) => {
                const classes = preset.classes.split(",").map((s) => s.trim());
                const active =
                  preset.id === "custom"
                    ? false
                    : classes.join(",") === editor.roiClasses.join(",");
                return (
                  <button
                    key={preset.id}
                    type="button"
                    aria-pressed={active}
                    className={`nvr-zs-card ${active ? "is-active" : ""}`}
                    onClick={() =>
                      onPatch(
                        preset.id === "custom"
                          ? { roiClasses: ["", ""], roiAlertState: "" }
                          : {
                              roiClasses: classes,
                              roiAlertState: preset.anomalyState,
                              roiTimeoutSec: preset.anomalyTimeoutSec,
                              roiIntervalSec: preset.intervalSec,
                              roiHysteresis: preset.hysteresis,
                              name: editor.name || (pt ? preset.titlePt.split(" / ")[0] : preset.titleEn.split(" / ")[0]),
                            }
                      )
                    }
                  >
                    <strong>{pt ? preset.titlePt : preset.titleEn}</strong>
                    {preset.id === "custom" ? (
                      <span>{tr("você define os estados", "you define the states")}</span>
                    ) : (
                      <>
                        <span>{classes.map((c) => (pt ? ROI_STATE_PT[c] ?? c : c)).join(" ↔ ")}</span>
                        <span className="mono nvr-zs-muted">{classes.join(" · ")}</span>
                      </>
                    )}
                  </button>
                );
              })}
            </div>
            <p className="nvr-zs-muted">
              {tr("O cenário preenche os estados e o alerta. Você pode mudar tudo abaixo.", "The scenario fills states and alert. You can change everything below.")}
            </p>
          </section>
        )}

        <section className="nvr-zs-section">
          <h4>{tr("DESENHE NO VÍDEO", "DRAW ON THE VIDEO")}</h4>
          {k === "motion_mask" && (
            <div className="nvr-zs-callout">
              <span>{tr("Atalho", "Shortcut")}</span>
              <button type="button" className="nvr-btn sm" onClick={() => onSetPoints(OSD_CLOCK_POINTS)}>
                {tr("Cobrir o relógio da câmera (canto superior esquerdo)", "Cover the camera clock (top-left corner)")}
              </button>
              <span className="nvr-zs-muted">
                {tr("Ajuste os pontos se o relógio da sua câmera estiver em outro lugar.", "Adjust the points if your camera's clock sits elsewhere.")}
              </span>
            </div>
          )}
          <div className={`nvr-zs-draw ${drawn ? "is-done" : ""}`}>
            <span>{drawStatus}</span>
            <span className="nvr-zs-row">
              {k === "line" && (
                <button
                  type="button"
                  className="nvr-btn sm"
                  disabled={points.length !== 2}
                  onClick={() => onSetPoints((prev) => [...prev].reverse())}
                >
                  {tr("Inverter A e B", "Swap A and B")}
                </button>
              )}
              {k !== "line" && k !== "state" && (
                <button
                  type="button"
                  className="nvr-btn sm"
                  disabled={points.length === 0}
                  onClick={() => onSetPoints((prev) => prev.slice(0, -1))}
                >
                  {tr("Desfazer ponto", "Undo point")}
                </button>
              )}
              <button type="button" className="nvr-btn sm" disabled={points.length === 0} onClick={() => onSetPoints([])}>
                {tr("Recomeçar", "Start over")}
              </button>
            </span>
          </div>
          {k === "line" && (
            <p className="nvr-zs-muted">
              {tr(
                "O lado A fica à esquerda de quem anda do ponto 1 para o ponto 2. Os dois lados aparecem marcados no vídeo.",
                "Side A is on the left when walking from point 1 to point 2. Both sides are labelled on the video."
              )}
            </p>
          )}
          {k === "state" && (
            <p className="nvr-zs-muted">
              {tr(
                "Enquadre só o local. Quanto menos fundo dentro do retângulo, mais confiável a leitura.",
                "Frame only the spot. The less background inside, the more reliable the reading."
              )}
            </p>
          )}
        </section>

        {!isMaskKind(k) && (
          <section className="nvr-zs-section">
            <h4>{tr("CONFIGURE", "CONFIGURE")}</h4>
            <label className="nvr-zs-field">
              <span>{tr("Nome", "Name")}</span>
              <input
                className="nvr-input"
                aria-invalid={nameTaken}
                value={editor.name}
                placeholder={k === "zone" ? tr("Ex.: Entrada", "e.g. Entrance") : tr("Ex.: Portão da garagem", "e.g. Garage gate")}
                onChange={(e) => onPatch({ name: e.target.value })}
              />
              {nameTaken ? (
                <span className="nvr-zs-error" role="alert">
                  {tr("Já existe um item com esse nome nesta câmera.", "This camera already has an item with that name.")}
                </span>
              ) : (
                <span className="nvr-zs-muted">
                  {tr("Aparece nos eventos e no MQTT como", "Shown in events and MQTT as")}{" "}
                  <span className="mono">{slug || "…"}</span>
                </span>
              )}
            </label>

            {(k === "zone" || k === "line") && (
              <div className="nvr-zs-field">
                <span>{tr("Quais objetos contam", "Which objects count")}</span>
                <div className="nvr-zs-chips">
                  {ZONE_OBJECT_CHOICES.map((lbl) => {
                    const on = editor.objects.includes(lbl);
                    return (
                      <button
                        key={lbl}
                        type="button"
                        aria-pressed={on}
                        className={`nvr-btn sm mono ${on ? "active" : ""}`}
                        onClick={() =>
                          onPatch({ objects: on ? editor.objects.filter((o) => o !== lbl) : [...editor.objects, lbl] })
                        }
                      >
                        {on ? "✓ " : ""}
                        {objectLabel(lbl, pt)}
                      </button>
                    );
                  })}
                </div>
                {editor.objects.length === 0 && (
                  <span className="nvr-zs-muted">{tr("Nenhum marcado: vale para qualquer objeto.", "None selected: any object counts.")}</span>
                )}
              </div>
            )}

            {k === "zone" && (
              <>
                <fieldset className="nvr-zs-field">
                  <legend>{tr("Quando alertar", "When to alert")}</legend>
                  <label className="nvr-zs-radio">
                    <input type="radio" name="zs-when" checked={editor.alertMode === "enter"} onChange={() => onPatch({ alertMode: "enter" })} />
                    {tr("Assim que entrar", "As soon as it enters")}
                  </label>
                  <label className="nvr-zs-radio">
                    <input type="radio" name="zs-when" checked={editor.alertMode === "dwell"} onChange={() => onPatch({ alertMode: "dwell" })} />
                    {tr("Se ficar na área por mais de", "If it stays longer than")}
                    <input
                      type="number"
                      min={1}
                      max={3600}
                      className="nvr-input nvr-zs-num"
                      aria-label={tr("Segundos na área", "Seconds inside")}
                      value={editor.dwellSec}
                      onChange={(e) => onPatch({ dwellSec: Math.max(1, Number(e.target.value) || 1), alertMode: "dwell" })}
                    />
                    s
                  </label>
                </fieldset>
                <details className="nvr-zs-details">
                  <summary>{tr("Avançado", "Advanced")}</summary>
                  <label className="nvr-zs-field">
                    <span>{tr("Confirmar entrada após", "Confirm entry after")}</span>
                    <span className="nvr-zs-row">
                      <input
                        type="number"
                        min={1}
                        max={60}
                        className="nvr-input nvr-zs-num"
                        value={editor.inertia}
                        onChange={(e) => onPatch({ inertia: Math.max(1, Number(e.target.value) || 1) })}
                      />
                      {tr("quadros seguidos", "consecutive frames")}
                    </span>
                    <span className="nvr-zs-muted">
                      {tr("Evita alerta por um objeto que só encosta na borda. Padrão: 3.", "Avoids alerts from objects that only touch the edge. Default: 3.")}{" "}
                      <span className="mono">inertia</span>
                    </span>
                  </label>
                  <label className="nvr-zs-field">
                    <span>{tr("Só valer com a PTZ no preset", "Only active at PTZ preset")}</span>
                    <input
                      className="nvr-input mono"
                      placeholder={tr("nenhum (sempre vale)", "none (always active)")}
                      value={editor.preset}
                      onChange={(e) => onPatch({ preset: e.target.value })}
                    />
                  </label>
                </details>
              </>
            )}

            {k === "line" && (
              <>
                <fieldset className="nvr-zs-field">
                  <legend>{tr("Qual travessia gera alerta", "Which crossing alerts")}</legend>
                  {(["any", "a_to_b", "b_to_a"] as const).map((dir) => (
                    <label key={dir} className="nvr-zs-radio">
                      <input type="radio" name="zs-dir" checked={editor.direction === dir} onChange={() => onPatch({ direction: dir })} />
                      {dir === "any"
                        ? tr("Nos dois sentidos", "Both ways")
                        : dir === "a_to_b"
                        ? tr("Só do lado A para o lado B", "Only from side A to side B")
                        : tr("Só do lado B para o lado A", "Only from side B to side A")}
                    </label>
                  ))}
                </fieldset>
                <p className="nvr-zs-muted">
                  {tr(
                    "Diferente da zona: a linha só dispara no momento em que o objeto cruza, não enquanto ele está parado perto dela.",
                    "Unlike a zone, a line fires only at the moment of crossing, not while an object waits near it."
                  )}
                </p>
              </>
            )}

            {(k === "zone" || k === "line") && (
              <div className="nvr-zs-field">
                <label className="nvr-zs-radio" style={{ alignItems: "flex-start" }}>
                  <input
                    type="checkbox"
                    checked={Boolean(editor.siren)}
                    onChange={(e) => onPatch({ siren: e.target.checked })}
                  />
                  <span>
                    <strong>
                      {tr(
                        "Disparar sirene sonora da câmera ao alertar",
                        "Sound camera audible siren on alert"
                      )}
                    </strong>
                    <br />
                    <span className="nvr-zs-muted">
                      {tr(
                        "Toca a sirene no alto-falante da câmera (Tapo :8800 / Intelbras CGI) quando um objeto permitido entrar na zona ou cruzar a linha.",
                        "Plays the siren on the camera speaker (Tapo :8800 / Intelbras CGI) when a permitted object enters the zone or crosses the line."
                      )}
                    </span>
                  </span>
                </label>
              </div>
            )}

            {k === "state" && <StateFields pt={pt} editor={editor} onPatch={onPatch} />}
          </section>
        )}

        <details className="nvr-zs-details">
          <summary>{tr("Coordenadas (para quem edita o YAML)", "Coordinates (for YAML editing)")}</summary>
          <div className="nvr-zs-row">
            <code className="nvr-zs-code mono">{coordText || "—"}</code>
            <button
              type="button"
              className="nvr-btn sm"
              disabled={!coordText}
              onClick={() => navigator.clipboard?.writeText(coordText)}
            >
              {tr("Copiar", "Copy")}
            </button>
          </div>
        </details>
      </div>
      <div className="nvr-zs-footer">
        <div className="nvr-zs-row">
          <button type="button" className="nvr-btn primary nvr-zs-grow" onClick={onCommit}>
            {isNew ? tr("Adicionar à câmera", "Add to camera") : tr("Aplicar alterações", "Apply changes")}
          </button>
          <button type="button" className="nvr-btn" onClick={onCancel}>
            {tr("Cancelar", "Cancel")}
          </button>
        </div>
        <span className="nvr-zs-muted">
          {tr("Fica como alteração não salva até você revisar e salvar.", "Stays an unsaved change until you review and save.")}
        </span>
      </div>
    </>
  );
};

const MaskKindPicker: React.FC<{
  pt: boolean;
  kind: MaskKind;
  locked: boolean;
  onChange: (k: MaskKind) => void;
}> = ({ pt, kind, locked, onChange }) => {
  const rows: Array<[MaskKind, string, string, string]> = [
    [
      "motion_mask",
      "motion.mask",
      pt ? "Pixels da área não acordam a detecção. Um objeto que vem de fora continua sendo detectado ao passar por ela." : "Pixels here do not wake detection. An object coming from outside is still detected as it passes.",
      pt ? "relógio da câmera, árvore, TV" : "camera clock, tree, TV",
    ],
    [
      "object_mask",
      "objects.mask",
      pt ? "A detecção roda, mas um objeto cuja base (os pés, as rodas) fica dentro da área é descartado." : "Detection runs, but an object whose base (feet, wheels) is inside is dropped.",
      pt ? "rua do vizinho, cartaz, estátua" : "neighbour's street, poster, statue",
    ],
    [
      "privacy_mask",
      "privacy_mask",
      pt ? "Tarja preta no ao vivo do console, nas fotos de eventos e nas exportações. O arquivo gravado não é alterado." : "Black box in the console live view, event snapshots and exports. The recorded file is not altered.",
      pt ? "janela do vizinho" : "neighbour's window",
    ],
  ];
  return (
    <section className="nvr-zs-section">
      <h4>{pt ? "TIPO DE MÁSCARA" : "MASK TYPE"}</h4>
      <div className="nvr-segmented nvr-zs-masktabs" role="group" aria-label={pt ? "Tipo de máscara" : "Mask type"}>
        {rows.map(([k, key]) => (
          <button
            key={k}
            type="button"
            aria-pressed={kind === k}
            disabled={locked && kind !== k}
            className={`nvr-btn ${kind === k ? "active" : ""}`}
            onClick={() => onChange(k)}
          >
            <span>{maskKindLabel(k, pt)}</span>
            <span className="mono nvr-zs-muted">{key}</span>
          </button>
        ))}
      </div>
      <details className="nvr-zs-details" open>
        <summary>{pt ? "Qual a diferença?" : "What's the difference?"}</summary>
        {rows.map(([k, , desc, ex]) => (
          <div key={k} className="nvr-zs-compare">
            <strong>{maskKindLabel(k, pt)}</strong>
            <span>{desc}</span>
            <span className="nvr-zs-muted">
              {pt ? "Ex.: " : "e.g. "}
              {ex}
            </span>
          </div>
        ))}
      </details>
    </section>
  );
};

const StateFields: React.FC<{
  pt: boolean;
  editor: VisionEditorState;
  onPatch: (p: Partial<VisionEditorState>) => void;
}> = ({ pt, editor, onPatch }) => {
  const tr = (a: string, b: string) => (pt ? a : b);
  const [editingStates, setEditingStates] = useState(false);
  const classes = editor.roiClasses.filter(Boolean);
  const custom = editingStates || classes.length < 2;
  const pills = ROI_ALERT_DURATION_PILLS.map((p) => p.sec);
  const isPill = pills.includes(editor.roiTimeoutSec);
  return (
    <>
      <div className="nvr-zs-field">
        <span>{tr("Estados possíveis", "Possible states")}</span>
        {custom ? (
          <input
            className="nvr-input mono"
            placeholder="closed, open"
            aria-label={tr("Estados separados por vírgula", "Comma-separated states")}
            value={editor.roiClasses.join(", ")}
            onChange={(e) => onPatch({ roiClasses: e.target.value.split(",").map((s) => s.trim()) })}
          />
        ) : (
          <span className="nvr-zs-row">
            {classes.map((c, i) => (
              <React.Fragment key={c}>
                {i > 0 && <span aria-hidden="true">↔</span>}
                <span className="nvr-zs-state mono">{c}</span>
              </React.Fragment>
            ))}
            <button type="button" className="nvr-zs-link" onClick={() => setEditingStates(true)}>
              {tr("Editar estados", "Edit states")}
            </button>
          </span>
        )}
      </div>
      <fieldset className="nvr-zs-field">
        <legend>{tr("Quando alertar", "When to alert")}</legend>
        <label className="nvr-zs-radio">
          <input type="radio" name="zs-roi" checked={!editor.roiAlertState} onChange={() => onPatch({ roiAlertState: "" })} />
          {tr("Nunca. Só mostrar o estado atual no vídeo", "Never. Just show the current state")}
        </label>
        <label className="nvr-zs-radio">
          <input
            type="radio"
            name="zs-roi"
            checked={Boolean(editor.roiAlertState)}
            onChange={() => onPatch({ roiAlertState: classes[classes.length - 1] || "" })}
          />
          {tr("Se ficar", "If it stays")}
          <select
            className="nvr-select sm mono"
            aria-label={tr("Estado de alerta", "Alert state")}
            value={editor.roiAlertState || classes[classes.length - 1] || ""}
            onChange={(e) => onPatch({ roiAlertState: e.target.value })}
          >
            {classes.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </select>
          {tr("por mais de", "for more than")}
        </label>
        {editor.roiAlertState && (
          <div className="nvr-zs-chips">
            {ROI_ALERT_DURATION_PILLS.map((p) => (
              <button
                key={p.sec}
                type="button"
                aria-pressed={editor.roiTimeoutSec === p.sec}
                className={`nvr-btn sm mono ${editor.roiTimeoutSec === p.sec ? "active" : ""}`}
                onClick={() => onPatch({ roiTimeoutSec: p.sec })}
              >
                {formatDurationShort(p.sec)}
              </button>
            ))}
            <label className="nvr-zs-row">
              <span className="nvr-zs-muted">{tr("outro", "other")}</span>
              <input
                type="number"
                min={5}
                max={86400}
                className={`nvr-input nvr-zs-num ${isPill ? "" : "is-active"}`}
                aria-label={tr("Segundos até alertar", "Seconds before alert")}
                value={editor.roiTimeoutSec}
                onChange={(e) => onPatch({ roiTimeoutSec: Math.max(5, Number(e.target.value) || 5) })}
              />
              s
            </label>
          </div>
        )}
      </fieldset>
      <details className="nvr-zs-details">
        <summary>{tr("Avançado", "Advanced")}</summary>
        <label className="nvr-zs-field">
          <span>{tr("Verificar o local a cada", "Check the spot every")}</span>
          <span className="nvr-zs-row">
            <input
              type="number"
              min={1}
              max={300}
              className="nvr-input nvr-zs-num"
              value={editor.roiIntervalSec}
              onChange={(e) => onPatch({ roiIntervalSec: Math.max(1, Number(e.target.value) || 1) })}
            />
            {tr("segundos", "seconds")}
          </span>
          <span className="nvr-zs-muted">
            {tr("Menor reage mais rápido e gasta mais CPU.", "Lower reacts faster and uses more CPU.")} <span className="mono">interval_sec</span>
          </span>
        </label>
        <label className="nvr-zs-field">
          <span>{tr("Só mudar de estado após", "Only change state after")}</span>
          <span className="nvr-zs-row">
            <input
              type="number"
              min={1}
              max={30}
              className="nvr-input nvr-zs-num"
              value={editor.roiHysteresis}
              onChange={(e) => onPatch({ roiHysteresis: Math.max(1, Number(e.target.value) || 1) })}
            />
            {tr("leituras iguais seguidas", "matching readings in a row")}
          </span>
          <span className="nvr-zs-muted">
            {tr("Evita trocar de estado por uma leitura errada.", "Avoids flipping state on one bad reading.")} <span className="mono">hysteresis</span>
          </span>
        </label>
      </details>
      {editor.roiAlertState && (
        <p className="nvr-zs-summary">
          {tr(
            `Verifica a cada ${editor.roiIntervalSec} s. Alerta se ficar ${editor.roiAlertState} por mais de ${formatDurationShort(editor.roiTimeoutSec)}.`,
            `Checks every ${editor.roiIntervalSec} s. Alerts if ${editor.roiAlertState} for more than ${formatDurationShort(editor.roiTimeoutSec)}.`
          )}
        </p>
      )}
    </>
  );
};

// ------------------------------------------------------------------------------------------------
// Motion sensitivity
// ------------------------------------------------------------------------------------------------

const MotionPanel: React.FC<{
  pt: boolean;
  motion: CameraMotionConfig;
  profile: MotionProfileId | "custom";
  motionNow: boolean | undefined;
  onPatch: (p: Partial<CameraMotionConfig>) => void;
  onProfile: (id: MotionProfileId) => void;
  onApplyAll: () => void;
}> = ({ pt, motion, profile, motionNow, onPatch, onProfile, onApplyAll }) => {
  const tr = (a: string, b: string) => (pt ? a : b);
  const enabled = motion.enabled ?? true;
  const descs: Record<MotionProfileId, string> = {
    indoor: tr("Ambiente fechado, luz estável. Pega movimentos pequenos.", "Indoors, steady light. Catches small movement."),
    balanced: tr("Padrão para a maioria das câmeras.", "Default for most cameras."),
    outdoor: tr("Árvores, vento, sol e sombra. Ignora movimentos pequenos.", "Trees, wind, sun and shade. Ignores small movement."),
  };
  const slider = (
    label: string,
    key: string,
    hint: string,
    value: number,
    min: number,
    max: number,
    step: number,
    unit: string,
    onChange: (v: number) => void
  ) => (
    <label className="nvr-zs-field">
      <span className="nvr-zs-slider-head">
        <span>{label}</span>
        <strong className="mono">
          {value}
          {unit}
        </strong>
      </span>
      <input type="range" min={min} max={max} step={step} value={value} disabled={!enabled} onChange={(e) => onChange(Number(e.target.value))} />
      <span className="nvr-zs-muted">
        {hint} <span className="mono">{key}</span>
      </span>
    </label>
  );
  return (
    <div className="nvr-zs-scroll">
      <section className="nvr-zs-section">
        <div className="nvr-zs-slider-head">
          <h4>{tr("DETECÇÃO DE MOVIMENTO", "MOTION DETECTION")}</h4>
          <ToggleSwitch checked={enabled} label={tr("Detecção de movimento", "Motion detection")} onChange={(v) => onPatch({ enabled: v })} />
        </div>
        <p className="nvr-zs-muted">
          {enabled
            ? tr(
                "O movimento acorda a IA. Sem movimento, a câmera não procura objetos, e isso economiza CPU.",
                "Motion wakes the AI. Without motion the camera does not look for objects, which saves CPU."
              )
            : tr(
                "Desligado: esta câmera não acorda a IA por movimento.",
                "Off: this camera does not wake the AI on motion."
              )}
        </p>
        <div className={`nvr-zs-live ${motionNow ? "is-on" : ""}`} role="status">
          <span>{tr("Movimento agora", "Motion now")}:</span>
          <strong>
            {motionNow === undefined
              ? tr("sem sinal da câmera", "no camera signal")
              : motionNow
              ? tr("detectado", "detected")
              : tr("nenhum", "none")}
          </strong>
          <span className="nvr-zs-muted">
            {tr("com a configuração salva", "with the saved settings")}
          </span>
        </div>
      </section>

      <section className="nvr-zs-section">
        <h4>{tr("COMO É A CENA?", "WHAT IS THE SCENE LIKE?")}</h4>
        <div className="nvr-zs-profiles">
          {MOTION_PROFILES.map((p) => (
            <button
              key={p.id}
              type="button"
              aria-pressed={profile === p.id}
              disabled={!enabled}
              className={`nvr-zs-card ${profile === p.id ? "is-active" : ""}`}
              onClick={() => onProfile(p.id)}
            >
              <strong>{profileLabel(p.id, pt)}</strong>
              <span>{descs[p.id]}</span>
            </button>
          ))}
        </div>
        {profile === "custom" && (
          <p className="nvr-zs-muted">{tr("Valores personalizados no ajuste fino.", "Custom values in fine tuning.")}</p>
        )}
      </section>

      <details className="nvr-zs-details" open={profile === "custom"}>
        <summary>{tr("Ajuste fino", "Fine tuning")}</summary>
        {slider(
          tr("Quanto um pixel precisa mudar", "How much a pixel must change"),
          "threshold",
          tr("Maior: menos sensível a sombra e ruído.", "Higher: less sensitive to shade and noise."),
          motion.threshold ?? 30,
          5,
          120,
          1,
          "",
          (v) => onPatch({ threshold: v })
        )}
        {slider(
          tr("Tamanho mínimo do movimento", "Minimum motion size"),
          "contour_area",
          tr("Ignora manchas menores que isso.", "Ignores blobs smaller than this."),
          motion.contour_area ?? 10,
          3,
          150,
          1,
          " px",
          (v) => onPatch({ contour_area: v })
        )}
        {slider(
          tr("Ignorar mudança brusca de luz", "Ignore sudden light changes"),
          "lightning_threshold",
          tr("Se essa fração da imagem mudar de uma vez, é tratado como luz, não movimento.", "If this fraction of the image changes at once, it's treated as light, not motion."),
          Number((motion.lightning_threshold ?? 0.8).toFixed(2)),
          0.2,
          1,
          0.05,
          "",
          (v) => onPatch({ lightning_threshold: v })
        )}
        <div className="nvr-zs-field">
          <span className="nvr-zs-slider-head">
            <span>{tr("Melhorar contraste à noite", "Boost contrast at night")}</span>
            <ToggleSwitch
              checked={motion.improve_contrast ?? true}
              label={tr("Melhorar contraste à noite", "Boost contrast at night")}
              onChange={(v) => onPatch({ improve_contrast: v })}
            />
          </span>
          <span className="nvr-zs-muted">
            <span className="mono">improve_contrast</span>
          </span>
        </div>
        <label className="nvr-zs-field">
          <span>{tr("Resolução da análise", "Analysis resolution")}</span>
          <span className="nvr-zs-row">
            <input
              type="number"
              min={32}
              max={360}
              className="nvr-input nvr-zs-num"
              disabled={!enabled}
              value={motion.frame_height ?? 100}
              onChange={(e) => onPatch({ frame_height: Math.max(32, Math.min(360, Number(e.target.value) || 100)) })}
            />
            px {tr("de altura", "tall")}
          </span>
          <span className="nvr-zs-muted">
            {tr("Maior pega coisas menores e usa mais CPU.", "Higher catches smaller things and uses more CPU.")} <span className="mono">frame_height</span>
          </span>
        </label>
      </details>

      <button type="button" className="nvr-btn" onClick={onApplyAll}>
        {tr("Usar estes valores em todas as câmeras", "Use these values on every camera")}
      </button>
    </div>
  );
};

// ------------------------------------------------------------------------------------------------
// Save status (always visible above the list)
// ------------------------------------------------------------------------------------------------

const SaveStatus: React.FC<{
  pt: boolean;
  pending: number;
  isDirty: boolean;
  isSaving: boolean;
  outcome: SaveOutcome | null;
  onReview: () => void;
}> = ({ pt, pending, isDirty, isSaving, outcome, onReview }) => {
  const tr = (a: string, b: string) => (pt ? a : b);
  const kind = resolveSaveStatus({ isSaving, isDirty, outcome });
  if (kind === "busy") {
    return (
      <div className="nvr-zs-status is-busy" role="status">
        <span>{tr("Salvando…", "Saving…")}</span>
      </div>
    );
  }
  if (kind === "error" && outcome) {
    return (
      <div className="nvr-zs-status is-error" role="alert">
        <span>
          <strong>{tr("Não foi salvo.", "Not saved.")}</strong> {outcome.message}
        </span>
        <button type="button" className="nvr-btn sm" onClick={onReview}>
          {tr("Revisar e tentar de novo", "Review and retry")}
        </button>
      </div>
    );
  }
  if (kind === "pending") {
    return (
      <div className="nvr-zs-status is-pending" role="status">
        <span>
          {pending > 0
            ? tr(
                `${pending} ${pending === 1 ? "alteração não salva" : "alterações não salvas"}. Ainda não vale nas câmeras.`,
                `${pending} unsaved ${pending === 1 ? "change" : "changes"}. Not live on cameras yet.`
              )
            : tr("Há alterações não salvas em outras abas.", "There are unsaved changes in other tabs.")}
        </span>
        <button type="button" className="nvr-btn sm primary" onClick={onReview}>
          {tr("Revisar e salvar", "Review and save")}
        </button>
      </div>
    );
  }
  if (kind === "ok" && outcome) {
    return (
      <div className="nvr-zs-status is-ok" role="status">
        <span>
          {tr(
            `Salvo às ${formatClock(outcome.at, pt)}. Aplicado nas câmeras.`,
            `Saved at ${formatClock(outcome.at, pt)}. Applied to cameras.`
          )}
        </span>
      </div>
    );
  }
  return null;
};

// ------------------------------------------------------------------------------------------------
// Review and save
// ------------------------------------------------------------------------------------------------

const ReviewPanel: React.FC<{
  pt: boolean;
  changes: VisionChange[];
  onUndo: (c: VisionChange) => void;
  onBack: () => void;
  onSave: () => Promise<void> | void;
  onDiscard: () => void;
  isSaving: boolean;
  isDirty: boolean;
  saveOutcome: SaveOutcome | null;
  notify: (msg: string) => void;
}> = ({ pt, changes, onUndo, onBack, onSave, onDiscard, isSaving, isDirty, saveOutcome, notify }) => {
  const tr = (a: string, b: string) => (pt ? a : b);
  const cams = countChangedCameras(changes);
  const yaml = changes.flatMap((c) => [...c.yaml, ""]).join("\n").trim();
  const sign = (t: VisionChange["type"]) => (t === "added" ? "+" : t === "removed" ? "−" : "~");
  return (
    <div className="nvr-zs nvr-zs-review">
      <section className="nvr-zs-panel nvr-zs-review-list">
        <div className="nvr-zs-editor-head">
          <div>
            <span className="nvr-zs-kicker mono">{tr("REVISAR E SALVAR", "REVIEW AND SAVE")}</span>
            <strong>
              {changes.length === 0
                ? tr("Nenhuma alteração pendente", "No pending changes")
                : tr(
                    `${changes.length} ${changes.length === 1 ? "alteração" : "alterações"} em ${cams} ${cams === 1 ? "câmera" : "câmeras"}`,
                    `${changes.length} ${changes.length === 1 ? "change" : "changes"} on ${cams} ${cams === 1 ? "camera" : "cameras"}`
                  )}
            </strong>
            <span className="nvr-zs-muted">
              {changes.length === 0
                ? tr("Zonas, máscaras e movimento estão iguais ao que está gravado.", "Zones, masks and motion match what is saved.")
                : tr("Nada disso está valendo ainda. Confira antes de salvar.", "None of this is live yet. Check before saving.")}
            </span>
          </div>
          <button type="button" className="nvr-zs-link" onClick={onBack}>
            {tr("Voltar e continuar editando", "Back to editing")}
          </button>
        </div>
        <ul className="nvr-zs-changes nvr-zs-scroll">
          {changes.map((c) => (
            <li key={c.id}>
              <span className={`nvr-zs-sign mono is-${c.type}`} aria-hidden="true">
                {sign(c.type)}
              </span>
              <span className="nvr-zs-item-text">
                <span>
                  <span className="nvr-zs-muted">{visionKindLabel(c.kind, pt)} ·</span> <strong>{c.title}</strong>
                </span>
                <span>{c.detail}</span>
              </span>
              <span className="mono nvr-zs-muted">{c.camera}</span>
              <button type="button" className="nvr-btn sm" onClick={() => onUndo(c)}>
                {tr("Desfazer", "Undo")}
              </button>
            </li>
          ))}
        </ul>
        <div className="nvr-zs-footer">
          {saveOutcome && !isSaving && (
            <div className={`nvr-zs-outcome ${saveOutcome.ok ? "is-ok" : "is-error"}`} role={saveOutcome.ok ? "status" : "alert"}>
              <strong>
                {saveOutcome.ok
                  ? tr(`Salvo às ${formatClock(saveOutcome.at, pt)}`, `Saved at ${formatClock(saveOutcome.at, pt)}`)
                  : tr("Não foi salvo", "Not saved")}
              </strong>
              <span>
                {saveOutcome.ok
                  ? tr(
                      "Gravado no config.yml. As câmeras aplicam em cerca de 1 segundo.",
                      "Written to config.yml. Cameras apply it within about a second."
                    )
                  : saveOutcome.message}
              </span>
              {saveOutcome.ok && !isDirty && (
                <button type="button" className="nvr-btn sm" onClick={onBack}>
                  {tr("Voltar para a câmera", "Back to the camera")}
                </button>
              )}
            </div>
          )}
          <button
            type="button"
            className="nvr-zs-save"
            disabled={isSaving || !isDirty}
            onClick={() => {
              void onSave();
            }}
          >
            <strong>
              {isSaving
                ? tr("Salvando…", "Saving…")
                : !isDirty
                ? tr("Nada pendente", "Nothing pending")
                : saveOutcome && !saveOutcome.ok
                ? tr("Tentar salvar de novo", "Try saving again")
                : tr("Salvar e aplicar", "Save and apply")}
            </strong>
            <span>
              {tr(
                "Grava o config.yml e aplica nas câmeras em cerca de 1 segundo, sem reiniciar e sem parar a gravação.",
                "Writes config.yml and applies it to the cameras within about a second, without a restart or recording gap."
              )}
            </span>
          </button>
          <div className="nvr-zs-row nvr-zs-between">
            <span className="nvr-zs-muted">
              {tr("Alterações de outras abas também entram neste salvamento.", "Changes from other tabs are saved too.")}
            </span>
            <button
              type="button"
              className="nvr-btn sm danger"
              disabled={isSaving || !isDirty}
              onClick={() => {
                onDiscard();
                onBack();
              }}
            >
              {tr("Descartar todas as alterações", "Discard all changes")}
            </button>
          </div>
        </div>
      </section>
      <section className="nvr-zs-yaml">
        <div className="nvr-zs-editor-head">
          <div>
            <strong>{tr("O que muda no config.yml", "What changes in config.yml")}</strong>
            <span className="nvr-zs-muted">{tr("Para quem confere ou versiona o YAML", "For anyone reviewing or versioning the YAML")}</span>
          </div>
          <button
            type="button"
            className="nvr-btn sm"
            disabled={!yaml}
            onClick={() => {
              navigator.clipboard?.writeText(yaml);
              notify(tr("Trecho copiado", "Snippet copied"));
            }}
          >
            {tr("Copiar trecho", "Copy snippet")}
          </button>
        </div>
        <pre className="nvr-zs-diff mono">
          {yaml
            ? yaml.split("\n").map((line, i) => (
                <span
                  key={i}
                  className={line.startsWith("+") ? "is-add" : line.startsWith("-") ? "is-del" : undefined}
                >
                  {line}
                  {"\n"}
                </span>
              ))
            : tr("Sem diferenças.", "No differences.")}
        </pre>
      </section>
    </div>
  );
};

