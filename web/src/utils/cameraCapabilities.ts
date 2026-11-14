import {
  CameraConfig,
  CameraHardwareCapabilities,
  CameraHardwareInfoResponse,
  CameraHardwareState,
  CameraPtzInfoResponse,
  OnvifPresetItem,
} from "../types/nvr";

export const HARDWARE_CONFIG_ONLY_PATCH_KEYS = [
  "apply_suggested_onvif",
  "apply_suggested_substream",
  "cloud_password",
  "protocol",
  "profile_token",
  "video_source_token",
  "reboot",
] as const;

export type HardwareConfigOnlyPatchKey =
  (typeof HARDWARE_CONFIG_ONLY_PATCH_KEYS)[number];

const HARDWARE_CONFIG_ONLY_PATCH_KEY_SET: ReadonlySet<string> = new Set<string>(
  HARDWARE_CONFIG_ONLY_PATCH_KEYS
);

export interface CameraProtocolDescriptor {
  id: string;
  label: string;
  aliases: readonly string[];
  defaultOnvifPort: number;
  panTiltOnlyDefault: boolean;
  defaultTalkbackMode: "webrtc" | "cgi_alaw" | null;
}

export const CAMERA_PROTOCOL_CATALOG: readonly CameraProtocolDescriptor[] = [
  {
    id: "",
    label: "Auto-Detect (ONVIF + CGI)",
    aliases: ["auto"],
    defaultOnvifPort: 80,
    panTiltOnlyDefault: false,
    defaultTalkbackMode: null,
  },
  {
    id: "tapo",
    label: "TP-Link Tapo (ONVIF :2020 + Tapo Backchannel)",
    aliases: ["tp-link", "tplink", "tapo_onvif"],
    defaultOnvifPort: 2020,
    panTiltOnlyDefault: true,
    defaultTalkbackMode: null,
  },
  {
    id: "dahua_cgi",
    label: "Intelbras / Dahua / Amcrest (ONVIF :80 + HTTP CGI)",
    aliases: ["intelbras", "dahua", "amcrest", "imou", "mibo"],
    defaultOnvifPort: 80,
    panTiltOnlyDefault: true,
    defaultTalkbackMode: "cgi_alaw",
  },
  {
    id: "hikvision_isapi",
    label: "Hikvision / HiLook (ONVIF :80 + ISAPI)",
    aliases: ["hikvision", "isapi", "hilook", "annke"],
    defaultOnvifPort: 80,
    panTiltOnlyDefault: false,
    defaultTalkbackMode: null,
  },
  {
    id: "reolink",
    label: "Reolink (ONVIF :8000)",
    aliases: ["reolink_cgi", "baichuan"],
    defaultOnvifPort: 8000,
    panTiltOnlyDefault: false,
    defaultTalkbackMode: null,
  },
  {
    id: "axis_vapix",
    label: "Axis VAPIX / ONVIF (:80)",
    aliases: ["axis", "vapix"],
    defaultOnvifPort: 80,
    panTiltOnlyDefault: false,
    defaultTalkbackMode: null,
  },
  {
    id: "onvif",
    label: "Standard ONVIF (Profile S/T)",
    aliases: ["onvif_s", "onvif_t", "generic_onvif"],
    defaultOnvifPort: 80,
    panTiltOnlyDefault: false,
    defaultTalkbackMode: null,
  },
  {
    id: "rtsp_only",
    label: "RTSP Only",
    aliases: ["rtsp", "none"],
    defaultOnvifPort: 80,
    panTiltOnlyDefault: false,
    defaultTalkbackMode: null,
  },
] as const;

export const GO2RTC_TWO_WAY_BACKCHANNEL_SCHEMES: readonly string[] = [
  "tapo://",
  "dvrip://",
  "isapi://",
  "roborock://",
  "ring://",
  "tuya://",
  "xiaomi://",
  "doorbird://",
] as const;

export function findCameraProtocolDescriptor(
  rawProtocol?: string | null
): CameraProtocolDescriptor | undefined {
  const lc = (rawProtocol || "").trim().toLowerCase();
  if (!lc || lc === "auto") return undefined;
  return CAMERA_PROTOCOL_CATALOG.find(
    (spec) => spec.id === lc || spec.aliases.includes(lc)
  );
}

export interface CameraHudControlOption {
  value: string | number;
  label: string;
}

export interface CameraHudControlDescriptor {
  id: keyof CameraHardwareState;
  capability: keyof CameraHardwareCapabilities;
  kind: "toggle" | "select";
  shortLabel?: string;
  ariaLabel: (camera: string) => string;
  title: string;
  resolveSelectOptions?: (state: CameraHardwareState) => CameraHudControlOption[];
  resolveSelectValue?: (state: CameraHardwareState) => string;
  parseSelectPatch?: (raw: string) => Record<string, unknown>;
}

export const CAMERA_HUD_CONTROL_DESCRIPTORS: readonly CameraHudControlDescriptor[] = [
  {
    id: "day_night_mode",
    capability: "day_night_mode",
    kind: "select",
    ariaLabel: (cam) => `Day Night IR Mode ${cam}`,
    title: "Day / Night IR Cut Filter Mode",
    resolveSelectValue: (state) => String(state.day_night_mode || "auto"),
    resolveSelectOptions: () => [
      { value: "auto", label: "IR:AUTO" },
      { value: "day", label: "IR:DAY" },
      { value: "night", label: "IR:NIGHT" },
    ],
    parseSelectPatch: (raw) => ({ day_night_mode: raw }),
  },
  {
    id: "hardware_autotracking",
    capability: "hardware_autotracking",
    kind: "toggle",
    shortLabel: "TRK",
    ariaLabel: (cam) => `Toggle SmartTrack ${cam}`,
    title: "Toggle camera hardware SmartTrack",
  },
  {
    id: "status_led",
    capability: "status_led",
    kind: "toggle",
    shortLabel: "LED",
    ariaLabel: (cam) => `Toggle Status LED ${cam}`,
    title: "Toggle camera status indicator LED",
  },
  {
    id: "mirror",
    capability: "flip_mirror",
    kind: "toggle",
    shortLabel: "MIR",
    ariaLabel: (cam) => `Mirror Image ${cam}`,
    title: "Mirror image horizontally",
  },
  {
    id: "flip",
    capability: "flip_mirror",
    kind: "toggle",
    shortLabel: "FLP",
    ariaLabel: (cam) => `Flip Image ${cam}`,
    title: "Flip image 180° vertically",
  },
  {
    id: "wdr",
    capability: "wdr",
    kind: "toggle",
    shortLabel: "WDR",
    ariaLabel: (cam) => `Toggle WDR ${cam}`,
    title: "Toggle Wide Dynamic Range (WDR)",
  },
  {
    id: "siren_on_motion",
    capability: "siren_on_event",
    kind: "toggle",
    shortLabel: "SRN",
    ariaLabel: (cam) => `Toggle Motion Siren ${cam}`,
    title: "Toggle camera audible alarm on motion",
  },
  {
    id: "brightness",
    capability: "imaging",
    kind: "select",
    ariaLabel: (cam) => `Brightness ${cam}`,
    title: "Camera sensor brightness",
    resolveSelectValue: (state) => String(state.brightness ?? 50),
    resolveSelectOptions: (state) =>
      Array.from(new Set([25, 40, 50, 60, 75, 90, state.brightness ?? 50]))
        .sort((a, b) => a - b)
        .map((b) => ({ value: b, label: `BRI:${b}%` })),
    parseSelectPatch: (raw) => ({ brightness: Number(raw) }),
  },
  {
    id: "speaker_volume",
    capability: "audio_volume",
    kind: "select",
    ariaLabel: (cam) => `Speaker Volume ${cam}`,
    title: "Camera built-in speaker volume",
    resolveSelectValue: (state) => String(state.speaker_volume ?? 85),
    resolveSelectOptions: (state) =>
      Array.from(new Set([25, 50, 75, 85, 100, state.speaker_volume ?? 85]))
        .sort((a, b) => a - b)
        .map((v) => ({ value: v, label: `VOL:${v}%` })),
    parseSelectPatch: (raw) => ({ speaker_volume: Number(raw) }),
  },
];

export function isCameraPtzEnabled(
  camCfg?: CameraConfig,
  ptzInfo?: CameraPtzInfoResponse | null,
  hwInfo?: CameraHardwareInfoResponse | null
): boolean {
  if (
    hwInfo?.online &&
    hwInfo.capabilities &&
    hwInfo.capabilities.ptz === false &&
    ptzInfo?.online !== true
  ) {
    return false;
  }
  if (hwInfo?.online && hwInfo.capabilities?.ptz === true) {
    return true;
  }
  if (ptzInfo?.enabled) return true;
  return Boolean(camCfg?.onvif?.host && camCfg.onvif.host.trim().length > 0);
}

export function resolveCameraZoomSupported(
  camCfg?: CameraConfig,
  hwInfo?: CameraHardwareInfoResponse | null
): boolean {
  if (hwInfo?.capabilities?.ptz_zoom !== undefined) {
    return Boolean(hwInfo.capabilities.ptz_zoom);
  }
  const protoSpec = findCameraProtocolDescriptor(
    hwInfo?.protocol_family ||
      camCfg?.hardware?.protocol ||
      camCfg?.onvif?.protocol
  );
  if (protoSpec) {
    return !protoSpec.panTiltOnlyDefault;
  }
  return camCfg?.onvif?.port !== 2020;
}

export function resolveCameraPushToTalkCapability(
  camCfg?: CameraConfig,
  hwInfo?: CameraHardwareInfoResponse | null,
  go2rtcStreams?: Record<string, unknown> | null,
  camName?: string
): { supported: boolean; mode: "webrtc" | "cgi_alaw" | null } {
  if (
    camCfg?.onvif?.has_cloud_password ||
    camCfg?.hardware?.has_cloud_password ||
    Boolean(
      camCfg?.onvif?.cloud_password &&
        camCfg.onvif.cloud_password.trim() !== ""
    ) ||
    Boolean(
      camCfg?.hardware?.cloud_password &&
        camCfg.hardware.cloud_password.trim() !== ""
    )
  ) {
    return { supported: true, mode: "webrtc" };
  }
  const hasTwoWayProducer = (rawUrl: string): boolean => {
    const lc = rawUrl.trim().toLowerCase();
    if (lc.includes("#backchannel=0")) return false;
    return (
      GO2RTC_TWO_WAY_BACKCHANNEL_SCHEMES.some((scheme) =>
        lc.startsWith(scheme)
      ) || lc.includes("#backchannel=1")
    );
  };
  if (camName && go2rtcStreams) {
    for (const key of [camName, `${camName}_talk`]) {
      const entry = go2rtcStreams[key];
      if (typeof entry === "string" && hasTwoWayProducer(entry)) {
        return { supported: true, mode: "webrtc" };
      }
      if (
        Array.isArray(entry) &&
        entry.some(
          (item) => typeof item === "string" && hasTwoWayProducer(item)
        )
      ) {
        return { supported: true, mode: "webrtc" };
      }
    }
  }
  if (hwInfo && hwInfo.capabilities) {
    if (hwInfo.capabilities.push_to_talk) {
      return {
        supported: true,
        mode: hwInfo.capabilities.push_to_talk_mode || "webrtc",
      };
    }
    return { supported: false, mode: null };
  }
  const protoSpec = findCameraProtocolDescriptor(
    camCfg?.hardware?.protocol || camCfg?.onvif?.protocol
  );
  if (protoSpec?.defaultTalkbackMode) {
    return { supported: true, mode: protoSpec.defaultTalkbackMode };
  }
  return { supported: false, mode: null };
}

export function isCameraHardwareControlsSupported(
  hwInfo?: CameraHardwareInfoResponse | null
): boolean {
  if (!hwInfo?.online || !hwInfo.capabilities) return false;
  const c = hwInfo.capabilities;
  return Boolean(
    CAMERA_HUD_CONTROL_DESCRIPTORS.some((desc) => Boolean(c[desc.capability])) ||
      c.manual_siren ||
      c.ir_lighting ||
      c.audio_anomaly_detect ||
      c.reboot
  );
}

export function resolveCameraPtzPresets(
  camCfg?: CameraConfig,
  ptzInfo?: CameraPtzInfoResponse | null
): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  const pushUnique = (val?: string | null) => {
    const clean = val ? val.trim() : "";
    if (clean && !seen.has(clean)) {
      seen.add(clean);
      out.push(clean);
    }
  };
  for (const p of ptzInfo?.presets || []) {
    pushUnique(p.name || p.token);
    if (p.token) seen.add(p.token.trim());
  }
  for (const name of ptzInfo?.preset_names || []) {
    pushUnique(name);
  }
  pushUnique(camCfg?.onvif?.return_preset);
  for (const z of Object.values(camCfg?.zones || {})) {
    pushUnique(z.preset);
  }
  return out;
}

export interface CameraCapabilitySnapshot {
  ptzEnabled: boolean;
  zoomSupported: boolean;
  hardwareControlsSupported: boolean;
  canManagePresets: boolean;
  presets: string[];
  activePreset: string | null;
  pushToTalk: {
    supported: boolean;
    mode: "webrtc" | "cgi_alaw" | null;
  };
}

export function resolveCameraCapabilitySnapshot(params: {
  camera?: string;
  cameraConfig?: CameraConfig;
  ptzInfo?: CameraPtzInfoResponse | null;
  hardwareInfo?: CameraHardwareInfoResponse | null;
  go2rtcStreams?: Record<string, unknown> | null;
}): CameraCapabilitySnapshot {
  const { camera, cameraConfig, ptzInfo, hardwareInfo, go2rtcStreams } = params;
  const ptzEnabled = isCameraPtzEnabled(cameraConfig, ptzInfo, hardwareInfo);
  const zoomSupported = resolveCameraZoomSupported(cameraConfig, hardwareInfo);
  const hardwareControlsSupported =
    isCameraHardwareControlsSupported(hardwareInfo);
  const canManagePresets =
    hardwareInfo?.online &&
    hardwareInfo?.capabilities?.ptz_presets_manage === false
      ? false
      : Boolean(ptzInfo?.features?.includes("presets_manage")) ||
        Boolean(hardwareInfo?.capabilities?.ptz_presets_manage);
  const presets = resolveCameraPtzPresets(cameraConfig, ptzInfo);
  const activePreset =
    ptzInfo?.active_preset !== undefined
      ? ptzInfo.active_preset
      : cameraConfig?.onvif?.return_preset || null;
  const pushToTalk = resolveCameraPushToTalkCapability(
    cameraConfig,
    hardwareInfo,
    go2rtcStreams,
    camera
  );
  return {
    ptzEnabled,
    zoomSupported,
    hardwareControlsSupported,
    canManagePresets,
    presets,
    activePreset,
    pushToTalk,
  };
}

export function isConfigPersistenceHardwarePatch(
  patch: Record<string, unknown>
): boolean {
  return (
    patch.apply_suggested_onvif === true ||
    patch.apply_suggested_substream === true ||
    typeof patch.cloud_password === "string"
  );
}

export function mergeOptimisticCameraHardwareInfo(
  prev: CameraHardwareInfoResponse | null | undefined,
  patch: Record<string, unknown>,
  responseData?: Partial<CameraHardwareInfoResponse> | null
): CameraHardwareInfoResponse | null {
  const hasCompleteResponse =
    responseData &&
    typeof responseData.camera === "string" &&
    responseData.capabilities !== undefined &&
    responseData.state !== undefined;
  const base =
    prev ?? (hasCompleteResponse ? (responseData as CameraHardwareInfoResponse) : null);
  if (!base) {
    return null;
  }

  const stateOnlyPatch: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(patch || {})) {
    if (
      v !== undefined &&
      k !== "__proto__" &&
      k !== "constructor" &&
      k !== "prototype" &&
      !HARDWARE_CONFIG_ONLY_PATCH_KEY_SET.has(k)
    ) {
      stateOnlyPatch[k] = v;
    }
  }

  const applySuggestedOnvif = patch?.apply_suggested_onvif === true;
  const hasNonEmptyCloudPassword =
    typeof patch?.cloud_password === "string" &&
    patch.cloud_password.trim().length > 0;

  const rawMissing = Array.isArray(responseData?.missing_requirements)
    ? responseData.missing_requirements
    : base.missing_requirements || [];
  const nextMissing = rawMissing.filter(
    (req) =>
      !(applySuggestedOnvif && req === "onvif_not_configured") &&
      !(hasNonEmptyCloudPassword && req === "tapo_cloud_password")
  );

  const responseState =
    responseData?.state && typeof responseData.state === "object"
      ? responseData.state
      : {};

  return {
    ...base,
    camera:
      typeof responseData?.camera === "string" ? responseData.camera : base.camera,
    online:
      typeof responseData?.online === "boolean" ? responseData.online : base.online,
    protocol_family:
      typeof responseData?.protocol_family === "string"
        ? responseData.protocol_family
        : base.protocol_family,
    onvif_configured:
      typeof responseData?.onvif_configured === "boolean"
        ? responseData.onvif_configured
        : applySuggestedOnvif
        ? true
        : base.onvif_configured,
    device_info: responseData?.device_info
      ? { ...base.device_info, ...responseData.device_info }
      : base.device_info,
    capabilities: responseData?.capabilities
      ? { ...base.capabilities, ...responseData.capabilities }
      : base.capabilities,
    missing_requirements: nextMissing,
    ...(responseData?.suggested_config !== undefined
      ? { suggested_config: responseData.suggested_config }
      : {}),
    state: {
      ...(base.state || {}),
      ...stateOnlyPatch,
      ...responseState,
    },
  };
}

function deduplicatePresetItems(items: OnvifPresetItem[]): OnvifPresetItem[] {
  const out: OnvifPresetItem[] = [];
  const seenTokens = new Set<string>();
  const seenNames = new Set<string>();
  for (const item of items) {
    const tok = (item?.token || "").trim();
    const nm = (item?.name || tok).trim();
    if (!tok && !nm) continue;
    const effectiveToken = tok || nm;
    const effectiveName = nm || tok;
    if (seenTokens.has(effectiveToken) || seenNames.has(effectiveName)) {
      continue;
    }
    seenTokens.add(effectiveToken);
    seenNames.add(effectiveName);
    out.push({ token: effectiveToken, name: effectiveName });
  }
  return out;
}

export type CameraHardwareInfo = CameraHardwareInfoResponse;
export type CameraPtzInfo = CameraPtzInfoResponse;

export interface SavedPresetInput {
  name?: string;
  token?: string;
  mode?: string;
  presets?: string[];
  preset_names?: Record<string, string>;
}

export function applySavedPresetToPtzInfo(
  prev: CameraPtzInfoResponse | null | undefined,
  tokenOrInput: string | SavedPresetInput,
  name?: string,
  serverPresets?: string[],
  serverPresetNames?: Record<string, string>
): CameraPtzInfoResponse | null {
  if (!prev) {
    return null;
  }
  const isObj =
    typeof tokenOrInput === "object" && tokenOrInput !== null;
  const rawToken = isObj ? tokenOrInput.token || "" : tokenOrInput || "";
  const rawName = isObj ? tokenOrInput.name : name;
  const resolvedServerPresets = isObj
    ? tokenOrInput.presets ?? serverPresets
    : serverPresets;
  const resolvedServerPresetNames = isObj
    ? tokenOrInput.preset_names ?? serverPresetNames
    : serverPresetNames;

  const cleanToken = rawToken.trim();
  const cleanName = (rawName ?? rawToken ?? "").trim() || cleanToken;
  const effectiveToken = cleanToken || cleanName;
  const effectiveName =
    (effectiveToken && resolvedServerPresetNames?.[effectiveToken]?.trim()) ||
    cleanName ||
    effectiveToken;

  if (!effectiveToken && !effectiveName) {
    return { ...prev };
  }

  const existingByKey = new Map<string, OnvifPresetItem>();
  for (const p of prev.presets || []) {
    if (p.token) existingByKey.set(p.token.trim(), p);
    if (p.name) existingByKey.set(p.name.trim(), p);
  }

  let rawItems: OnvifPresetItem[];
  if (Array.isArray(resolvedServerPresets)) {
    rawItems = resolvedServerPresets
      .map((rawTok) => (rawTok || "").trim())
      .filter(Boolean)
      .map((tok) => {
        const mappedName =
          resolvedServerPresetNames?.[tok]?.trim() ||
          (tok === effectiveToken ? effectiveName : undefined) ||
          existingByKey.get(tok)?.name ||
          tok;
        return { token: tok, name: mappedName };
      });
    if (
      !rawItems.some(
        (p) => p.token === effectiveToken || p.name === effectiveName
      )
    ) {
      rawItems.push({ token: effectiveToken, name: effectiveName });
    }
  } else {
    rawItems = (prev.presets || []).map((p) => {
      const tok = (p.token || "").trim();
      const mapped = tok && resolvedServerPresetNames?.[tok]?.trim();
      if (tok === effectiveToken || p.name.trim() === effectiveName) {
        return {
          token: effectiveToken || tok,
          name: mapped || effectiveName || p.name,
        };
      }
      return {
        token: tok || p.name,
        name: mapped || p.name,
      };
    });
    if (
      !rawItems.some(
        (p) => p.token === effectiveToken || p.name === effectiveName
      )
    ) {
      rawItems.push({ token: effectiveToken, name: effectiveName });
    }
  }

  const nextPresets = deduplicatePresetItems(rawItems);
  return {
    ...prev,
    ...(isObj && typeof tokenOrInput.mode === "string"
      ? { mode: tokenOrInput.mode }
      : {}),
    presets: nextPresets,
    preset_names: nextPresets.map((p) => p.name),
    active_preset: effectiveName,
  };
}

export const applyPresetSavedToPtzInfo = applySavedPresetToPtzInfo;

export function applyDeletedPresetToPtzInfo(
  prev: CameraPtzInfoResponse | null | undefined,
  deletedToken: string,
  serverPresets?: string[],
  serverPresetNames?: Record<string, string>
): CameraPtzInfoResponse | null {
  if (!prev) {
    return null;
  }
  const cleanDeleted = (deletedToken || "").trim();
  if (!cleanDeleted) {
    return { ...prev };
  }

  const existingPresets = prev.presets || [];
  const matchedItem = existingPresets.find(
    (p) => p.token.trim() === cleanDeleted || p.name.trim() === cleanDeleted
  );
  const matchedToken = matchedItem?.token?.trim() || cleanDeleted;
  const matchedName =
    matchedItem?.name?.trim() ||
    serverPresetNames?.[cleanDeleted]?.trim() ||
    cleanDeleted;

  const existingByKey = new Map<string, OnvifPresetItem>();
  for (const p of existingPresets) {
    if (p.token) existingByKey.set(p.token.trim(), p);
    if (p.name) existingByKey.set(p.name.trim(), p);
  }

  let nextPresets: OnvifPresetItem[];
  if (Array.isArray(serverPresets)) {
    const rawItems = serverPresets
      .map((rawTok) => (rawTok || "").trim())
      .filter(
        (tok) =>
          Boolean(tok) &&
          tok !== cleanDeleted &&
          tok !== matchedToken &&
          tok !== matchedName
      )
      .map((tok) => ({
        token: tok,
        name:
          serverPresetNames?.[tok]?.trim() ||
          existingByKey.get(tok)?.name ||
          tok,
      }));
    nextPresets = deduplicatePresetItems(rawItems);
  } else {
    nextPresets = deduplicatePresetItems(
      existingPresets
        .filter(
          (p) =>
            p.token.trim() !== cleanDeleted &&
            p.name.trim() !== cleanDeleted &&
            p.token.trim() !== matchedToken &&
            p.name.trim() !== matchedName
        )
        .map((p) => {
          const tok = p.token.trim();
          const mapped = tok && serverPresetNames?.[tok]?.trim();
          return {
            token: tok,
            name: mapped || p.name,
          };
        })
    );
  }

  const nextPresetNames =
    existingPresets.length === 0 &&
    !Array.isArray(serverPresets) &&
    Array.isArray(prev.preset_names)
      ? prev.preset_names.filter(
          (nm) =>
            nm.trim() !== cleanDeleted &&
            nm.trim() !== matchedToken &&
            nm.trim() !== matchedName
        )
      : nextPresets.map((p) => p.name);

  const curActive = prev.active_preset?.trim() || null;
  const shouldClearActive =
    !curActive ||
    curActive === cleanDeleted ||
    curActive === matchedToken ||
    curActive === matchedName;

  return {
    ...prev,
    presets: nextPresets,
    preset_names: nextPresetNames,
    active_preset: shouldClearActive ? null : prev.active_preset,
  };
}

export const applyPresetDeletedToPtzInfo = applyDeletedPresetToPtzInfo;

export interface ProbeCameraHardwareOptions {
  cameraConfig?: CameraConfig;
  force?: boolean;
  timeoutMs?: number;
  includePtz?: boolean;
  fetchImpl?: typeof fetch;
}

export interface CameraHardwareProbeSnapshot {
  hardware: CameraHardwareInfoResponse | null;
  ptz: CameraPtzInfoResponse | null;
  fromCache: boolean;
  networkError?: boolean;
}

export type CameraHardwareCacheListener = (camera?: string) => void;

export function buildCameraHardwareCacheKey(
  camName: string,
  camCfg?: CameraConfig
): string {
  const cleanCam = (camName || "").trim();
  if (!camCfg) return cleanCam;
  const profTok =
    camCfg.onvif?.profile_token || camCfg.hardware?.profile_token || "";
  const vsTok =
    camCfg.onvif?.video_source_token ||
    camCfg.hardware?.video_source_token ||
    "";
  const proto = camCfg.onvif?.protocol || camCfg.hardware?.protocol || "";
  return `${cleanCam}:${camCfg.onvif?.host || ""}:${
    camCfg.onvif?.port || 80
  }:${camCfg.onvif?.user || ""}:${Boolean(
    camCfg.onvif?.has_password
  )}:${Boolean(
    camCfg.onvif?.has_cloud_password || camCfg.hardware?.has_cloud_password
  )}:${profTok}:${vsTok}:${proto}`;
}

export interface ApplyCameraHardwarePatchOptions {
  onConfigPersisted?: () => Promise<void> | void;
  fetchImpl?: typeof fetch;
}

export interface CameraHardwareMutationResult {
  ok: boolean;
  status: number;
  data: Record<string, unknown>;
  hardware: CameraHardwareInfo | null;
}

export interface CameraPtzMutationResult {
  ok: boolean;
  status: number;
  data: Record<string, unknown>;
  ptz: CameraPtzInfo | null;
}

export interface CameraHardwareCacheController {
  getCachedCameraHardware: (cam: string) => CameraHardwareInfoResponse | null;
  setCachedCameraHardware: (
    cam: string,
    info: CameraHardwareInfoResponse | null
  ) => void;
  getHardware: (cam: string) => CameraHardwareInfoResponse | null;
  setHardware: (cam: string, info: CameraHardwareInfoResponse | null) => void;
  getCachedCameraPtzInfo: (cam: string) => CameraPtzInfoResponse | null;
  setCachedCameraPtzInfo: (
    cam: string,
    info: CameraPtzInfoResponse | null
  ) => void;
  getPtzInfo: (cam: string) => CameraPtzInfoResponse | null;
  setPtzInfo: (cam: string, info: CameraPtzInfoResponse | null) => void;
  invalidateCameraHardwareCache: (cam?: string) => void;
  invalidate: (cam?: string) => void;
  probeCameraHardwareAndPtz: (
    cam: string,
    options?: ProbeCameraHardwareOptions,
    fetchImpl?: typeof fetch
  ) => Promise<CameraHardwareProbeSnapshot>;
  probe: (
    cam: string,
    options?: ProbeCameraHardwareOptions,
    fetchImpl?: typeof fetch
  ) => Promise<CameraHardwareProbeSnapshot>;
  applyHardwarePatch: (
    camera: string,
    patch: Record<string, unknown>,
    options?: ApplyCameraHardwarePatchOptions
  ) => Promise<CameraHardwareMutationResult>;
  gotoPtzPreset: (
    camera: string,
    preset: string,
    options?: { fetchImpl?: typeof fetch }
  ) => Promise<CameraPtzMutationResult>;
  savePtzPreset: (
    camera: string,
    name: string,
    token?: string,
    options?: { fetchImpl?: typeof fetch }
  ) => Promise<CameraPtzMutationResult>;
  deletePtzPreset: (
    camera: string,
    tokenOrName: string,
    options?: { fetchImpl?: typeof fetch }
  ) => Promise<CameraPtzMutationResult>;
  getSnapshot: () => {
    hardwareByCam: Record<string, CameraHardwareInfoResponse>;
    ptzByCam: Record<string, CameraPtzInfoResponse>;
  };
  subscribe: (listener: CameraHardwareCacheListener) => () => void;
}

export function createCameraHardwareCacheController(
  defaultFetchOrOptions?:
    | typeof fetch
    | {
        fetchImpl?: typeof fetch;
      }
): CameraHardwareCacheController {
  const defaultFetchImpl: typeof fetch | undefined =
    typeof defaultFetchOrOptions === "function"
      ? defaultFetchOrOptions
      : defaultFetchOrOptions?.fetchImpl;
  const hardwareByCam = new Map<string, CameraHardwareInfoResponse>();
  const ptzByCam = new Map<string, CameraPtzInfoResponse>();
  const configKeyByCam = new Map<string, string>();
  const inFlightByCam = new Map<string, Promise<CameraHardwareProbeSnapshot>>();
  const listeners = new Set<CameraHardwareCacheListener>();

  const resolveFetch = (override?: typeof fetch): typeof fetch =>
    override ?? defaultFetchImpl ?? ((input, init) => fetch(input, init));

  const notify = (cam?: string) => {
    for (const listener of listeners) {
      try {
        listener(cam);
      } catch {
        // ignore listener errors
      }
    }
  };

  const getCachedCameraHardware = (
    cam: string
  ): CameraHardwareInfoResponse | null => {
    const clean = (cam || "").trim();
    if (!clean) return null;
    return hardwareByCam.get(clean) ?? null;
  };

  const setCachedCameraHardware = (
    cam: string,
    info: CameraHardwareInfoResponse | null
  ): void => {
    const clean = (cam || "").trim();
    if (!clean) return;
    if (info === null) {
      hardwareByCam.delete(clean);
    } else {
      hardwareByCam.set(clean, info);
    }
    notify(clean);
  };

  const getCachedCameraPtzInfo = (cam: string): CameraPtzInfoResponse | null => {
    const clean = (cam || "").trim();
    if (!clean) return null;
    return ptzByCam.get(clean) ?? null;
  };

  const setCachedCameraPtzInfo = (
    cam: string,
    info: CameraPtzInfoResponse | null
  ): void => {
    const clean = (cam || "").trim();
    if (!clean) return;
    if (info === null) {
      ptzByCam.delete(clean);
    } else {
      ptzByCam.set(clean, info);
    }
    notify(clean);
  };

  const invalidateCameraHardwareCache = (cam?: string): void => {
    const clean = (cam || "").trim();
    if (clean) {
      hardwareByCam.delete(clean);
      ptzByCam.delete(clean);
      configKeyByCam.delete(clean);
      inFlightByCam.delete(clean);
      notify(clean);
    } else {
      hardwareByCam.clear();
      ptzByCam.clear();
      configKeyByCam.clear();
      inFlightByCam.clear();
      notify(undefined);
    }
  };

  const probeCameraHardwareAndPtz = async (
    cam: string,
    options: ProbeCameraHardwareOptions = {},
    fetchImpl?: typeof fetch
  ): Promise<CameraHardwareProbeSnapshot> => {
    const cleanCam = (cam || "").trim();
    if (!cleanCam) {
      return { hardware: null, ptz: null, fromCache: false };
    }

    const resolvedOptions: ProbeCameraHardwareOptions =
      typeof options === "function"
        ? { fetchImpl: options as unknown as typeof fetch }
        : options || {};
    const fetchFn: typeof fetch = resolveFetch(
      fetchImpl ?? resolvedOptions.fetchImpl
    );
    const timeoutMs = resolvedOptions.timeoutMs ?? 3500;
    const includePtz = resolvedOptions.includePtz ?? true;
    const force = Boolean(resolvedOptions.force);
    const camCfg = resolvedOptions.cameraConfig;

    if (camCfg) {
      const nextCfgKey = buildCameraHardwareCacheKey(cleanCam, camCfg);
      const prevCfgKey = configKeyByCam.get(cleanCam);
      if (prevCfgKey !== undefined && prevCfgKey !== nextCfgKey) {
        hardwareByCam.delete(cleanCam);
        ptzByCam.delete(cleanCam);
        inFlightByCam.delete(cleanCam);
      }
      configKeyByCam.set(cleanCam, nextCfgKey);
    }

    if (!force && hardwareByCam.has(cleanCam)) {
      const cachedHw = hardwareByCam.get(cleanCam) ?? null;
      const needsPtz =
        includePtz &&
        ((cachedHw?.online && Boolean(cachedHw.capabilities?.ptz)) ||
          isCameraPtzEnabled(camCfg, undefined, cachedHw));
      if (!needsPtz || ptzByCam.has(cleanCam)) {
        return {
          hardware: cachedHw,
          ptz: ptzByCam.get(cleanCam) ?? null,
          fromCache: true,
        };
      }
    }

    if (!force) {
      const existingInFlight = inFlightByCam.get(cleanCam);
      if (existingInFlight) {
        return existingInFlight;
      }
    }

    const task = (async (): Promise<CameraHardwareProbeSnapshot> => {
      let hwData: CameraHardwareInfoResponse | null = force
        ? null
        : hardwareByCam.get(cleanCam) ?? null;
      let networkError = false;

      if (force || !hwData) {
        try {
          const hwRes = await fetchFn(
            `/api/${encodeURIComponent(cleanCam)}/hardware?timeout_ms=${timeoutMs}`,
            { credentials: "include" }
          );
          if (hwRes.ok) {
            const parsed = (await hwRes
              .json()
              .catch(() => null)) as CameraHardwareInfoResponse | null;
            if (parsed) {
              hwData = parsed;
              hardwareByCam.set(cleanCam, parsed);
            }
          }
        } catch {
          networkError = true;
        }
      }

      let ptzData: CameraPtzInfoResponse | null = force
        ? null
        : ptzByCam.get(cleanCam) ?? null;
      const shouldProbePtz =
        includePtz &&
        ((hwData?.online && Boolean(hwData.capabilities?.ptz)) ||
          isCameraPtzEnabled(camCfg, undefined, hwData));

      if (shouldProbePtz && (force || !ptzData)) {
        try {
          const ptzRes = await fetchFn(
            `/api/${encodeURIComponent(cleanCam)}/ptz/info`,
            { credentials: "include" }
          );
          if (ptzRes.ok) {
            const parsedPtz = (await ptzRes
              .json()
              .catch(() => null)) as CameraPtzInfoResponse | null;
            if (parsedPtz) {
              ptzData = parsedPtz;
              ptzByCam.set(cleanCam, parsedPtz);
            }
          }
        } catch {
          // ignore PTZ fetch failure if hardware succeeded
        }
      }

      notify(cleanCam);
      return {
        hardware: hwData ?? hardwareByCam.get(cleanCam) ?? null,
        ptz: ptzData ?? ptzByCam.get(cleanCam) ?? null,
        fromCache: false,
        ...(networkError ? { networkError: true } : {}),
      };
    })();

    inFlightByCam.set(cleanCam, task);
    try {
      return await task;
    } finally {
      if (inFlightByCam.get(cleanCam) === task) {
        inFlightByCam.delete(cleanCam);
      }
    }
  };

  const applyHardwarePatch = async (
    camera: string,
    patch: Record<string, unknown>,
    options?: ApplyCameraHardwarePatchOptions
  ): Promise<CameraHardwareMutationResult> => {
    const cleanCam = (camera || "").trim();
    if (!cleanCam) {
      return { ok: false, status: 0, data: {}, hardware: null };
    }
    const fetchFn = resolveFetch(options?.fetchImpl);
    const res = await fetchFn(
      `/api/${encodeURIComponent(cleanCam)}/hardware`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(patch),
      }
    );
    const rawData = await res.json().catch(() => null);
    const data: Record<string, unknown> =
      rawData && typeof rawData === "object"
        ? (rawData as Record<string, unknown>)
        : {};
    const ok = Boolean(res.ok && data.success !== false);
    if (!ok) {
      return {
        ok: false,
        status: res.status,
        data,
        hardware: getCachedCameraHardware(cleanCam),
      };
    }

    const existing = getCachedCameraHardware(cleanCam);
    const merged = mergeOptimisticCameraHardwareInfo(
      existing,
      patch,
      data as Partial<CameraHardwareInfoResponse>
    );
    if (merged) {
      setCachedCameraHardware(cleanCam, merged);
    }

    if (isConfigPersistenceHardwarePatch(patch)) {
      invalidateCameraHardwareCache(cleanCam);
      await options?.onConfigPersisted?.();
      const reprobed = await probeCameraHardwareAndPtz(
        cleanCam,
        { force: true },
        fetchFn
      );
      return {
        ok: true,
        status: res.status,
        data,
        hardware: reprobed.hardware ?? merged,
      };
    }

    return {
      ok: true,
      status: res.status,
      data,
      hardware: getCachedCameraHardware(cleanCam) ?? merged,
    };
  };

  const gotoPtzPreset = async (
    camera: string,
    preset: string,
    options?: { fetchImpl?: typeof fetch }
  ): Promise<CameraPtzMutationResult> => {
    const cleanCam = (camera || "").trim();
    const cleanPreset = (preset || "").trim();
    if (!cleanCam || !cleanPreset) {
      return {
        ok: false,
        status: 0,
        data: {},
        ptz: getCachedCameraPtzInfo(cleanCam),
      };
    }
    const fetchFn = resolveFetch(options?.fetchImpl);
    const res = await fetchFn(
      `/api/${encodeURIComponent(cleanCam)}/ptz/preset`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ preset: cleanPreset }),
      }
    );
    const rawData = await res.json().catch(() => null);
    const data: Record<string, unknown> =
      rawData && typeof rawData === "object"
        ? (rawData as Record<string, unknown>)
        : {};
    const ok = Boolean(res.ok && data.success !== false);
    if (ok) {
      const existing = getCachedCameraPtzInfo(cleanCam) || { enabled: true };
      const nextPtz: CameraPtzInfoResponse = {
        ...existing,
        active_preset: cleanPreset,
      };
      setCachedCameraPtzInfo(cleanCam, nextPtz);
      return { ok: true, status: res.status, data, ptz: nextPtz };
    }
    return {
      ok: false,
      status: res.status,
      data,
      ptz: getCachedCameraPtzInfo(cleanCam),
    };
  };

  const savePtzPreset = async (
    camera: string,
    name: string,
    token?: string,
    options?: { fetchImpl?: typeof fetch }
  ): Promise<CameraPtzMutationResult> => {
    const cleanCam = (camera || "").trim();
    const cleanName = (name || "").trim();
    const cleanToken = token?.trim();
    if (!cleanCam || !cleanName) {
      return {
        ok: false,
        status: 0,
        data: {},
        ptz: getCachedCameraPtzInfo(cleanCam),
      };
    }
    const fetchFn = resolveFetch(options?.fetchImpl);
    const res = await fetchFn(
      `/api/${encodeURIComponent(cleanCam)}/ptz/preset/save`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({
          name: cleanName,
          ...(cleanToken ? { token: cleanToken } : {}),
        }),
      }
    );
    const rawData = await res.json().catch(() => null);
    const data: Record<string, unknown> =
      rawData && typeof rawData === "object"
        ? (rawData as Record<string, unknown>)
        : {};
    const ok = Boolean(res.ok && data.success !== false);
    if (ok) {
      const existing = getCachedCameraPtzInfo(cleanCam) || { enabled: true };
      const returnedToken =
        typeof data.token === "string" && data.token.trim()
          ? data.token.trim()
          : cleanToken || cleanName;
      const returnedMode =
        typeof data.mode === "string" && data.mode.trim()
          ? data.mode.trim()
          : undefined;
      const nextPtz = applySavedPresetToPtzInfo(existing, {
        name: cleanName,
        token: returnedToken,
        mode: returnedMode,
        presets: Array.isArray(data.presets)
          ? (data.presets as string[])
          : undefined,
        preset_names:
          data.preset_names &&
          typeof data.preset_names === "object" &&
          !Array.isArray(data.preset_names)
            ? (data.preset_names as Record<string, string>)
            : undefined,
      });
      if (nextPtz) {
        setCachedCameraPtzInfo(cleanCam, nextPtz);
      }
      return {
        ok: true,
        status: res.status,
        data,
        ptz: nextPtz ?? getCachedCameraPtzInfo(cleanCam),
      };
    }
    return {
      ok: false,
      status: res.status,
      data,
      ptz: getCachedCameraPtzInfo(cleanCam),
    };
  };

  const deletePtzPreset = async (
    camera: string,
    tokenOrName: string,
    options?: { fetchImpl?: typeof fetch }
  ): Promise<CameraPtzMutationResult> => {
    const cleanCam = (camera || "").trim();
    const cleanToken = (tokenOrName || "").trim();
    if (!cleanCam || !cleanToken) {
      return {
        ok: false,
        status: 0,
        data: {},
        ptz: getCachedCameraPtzInfo(cleanCam),
      };
    }
    const fetchFn = resolveFetch(options?.fetchImpl);
    let res = await fetchFn(
      `/api/${encodeURIComponent(cleanCam)}/ptz/preset/${encodeURIComponent(
        cleanToken
      )}`,
      {
        method: "DELETE",
        credentials: "include",
      }
    );
    if (res.status === 404 || res.status === 405) {
      res = await fetchFn(
        `/api/${encodeURIComponent(cleanCam)}/ptz/preset/delete`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ token: cleanToken }),
        }
      );
    }
    const rawData = await res.json().catch(() => null);
    const data: Record<string, unknown> =
      rawData && typeof rawData === "object"
        ? (rawData as Record<string, unknown>)
        : {};
    const ok = Boolean(res.ok && data.success !== false);
    if (ok) {
      const existing = getCachedCameraPtzInfo(cleanCam) || { enabled: true };
      const nextPtz = applyDeletedPresetToPtzInfo(
        existing,
        cleanToken,
        Array.isArray(data.presets) ? (data.presets as string[]) : undefined,
        data.preset_names &&
          typeof data.preset_names === "object" &&
          !Array.isArray(data.preset_names)
          ? (data.preset_names as Record<string, string>)
          : undefined
      );
      if (nextPtz) {
        setCachedCameraPtzInfo(cleanCam, nextPtz);
      }
      return {
        ok: true,
        status: res.status,
        data,
        ptz: nextPtz ?? getCachedCameraPtzInfo(cleanCam),
      };
    }
    return {
      ok: false,
      status: res.status,
      data,
      ptz: getCachedCameraPtzInfo(cleanCam),
    };
  };

  const getSnapshot = () => {
    const hwObj: Record<string, CameraHardwareInfoResponse> = {};
    for (const [k, v] of hardwareByCam.entries()) {
      hwObj[k] = v;
    }
    const ptzObj: Record<string, CameraPtzInfoResponse> = {};
    for (const [k, v] of ptzByCam.entries()) {
      ptzObj[k] = v;
    }
    return { hardwareByCam: hwObj, ptzByCam: ptzObj };
  };

  const subscribe = (listener: CameraHardwareCacheListener) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  };

  return {
    getCachedCameraHardware,
    setCachedCameraHardware,
    getHardware: getCachedCameraHardware,
    setHardware: setCachedCameraHardware,
    getCachedCameraPtzInfo,
    setCachedCameraPtzInfo,
    getPtzInfo: getCachedCameraPtzInfo,
    setPtzInfo: setCachedCameraPtzInfo,
    invalidateCameraHardwareCache,
    invalidate: invalidateCameraHardwareCache,
    probeCameraHardwareAndPtz,
    probe: probeCameraHardwareAndPtz,
    applyHardwarePatch,
    gotoPtzPreset,
    savePtzPreset,
    deletePtzPreset,
    getSnapshot,
    subscribe,
  };
}

export const cameraHardwareCache: CameraHardwareCacheController =
  createCameraHardwareCacheController();

export const getCachedCameraHardware =
  cameraHardwareCache.getCachedCameraHardware;
export const setCachedCameraHardware =
  cameraHardwareCache.setCachedCameraHardware;
export const getCachedCameraPtzInfo =
  cameraHardwareCache.getCachedCameraPtzInfo;
export const setCachedCameraPtzInfo =
  cameraHardwareCache.setCachedCameraPtzInfo;
export const invalidateCameraHardwareCache =
  cameraHardwareCache.invalidateCameraHardwareCache;
export const probeCameraHardwareAndPtz =
  cameraHardwareCache.probeCameraHardwareAndPtz;
export const applyCameraHardwarePatch =
  cameraHardwareCache.applyHardwarePatch;
export const gotoCameraPtzPreset = cameraHardwareCache.gotoPtzPreset;
export const saveCameraPtzPreset = cameraHardwareCache.savePtzPreset;
export const deleteCameraPtzPreset = cameraHardwareCache.deletePtzPreset;
export const subscribeCameraHardwareCache = cameraHardwareCache.subscribe;
