import {
  CameraConfig,
  CameraHardwareCapabilities,
  CameraHardwareInfoResponse,
  CameraHardwareState,
  CameraPtzInfoResponse,
} from "../types/nvr";

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
  if (camName && go2rtcStreams && go2rtcStreams[camName] !== undefined) {
    const entry = go2rtcStreams[camName];
    if (typeof entry === "string" && hasTwoWayProducer(entry)) {
      return { supported: true, mode: "webrtc" };
    }
    if (
      Array.isArray(entry) &&
      entry.some((item) => typeof item === "string" && hasTwoWayProducer(item))
    ) {
      return { supported: true, mode: "webrtc" };
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
      c.ir_lighting ||
      c.audio_anomaly_detect ||
      c.reboot
  );
}
