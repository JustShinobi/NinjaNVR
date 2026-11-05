import { GenAiConfig, NinjaConfigResponse } from "../types/nvr";

/**
 * Parses a single-line YAML scalar value, preserving `#` characters inside single or double quotes
 * and unescaping JSON double-quoted strings or YAML single-quoted strings (`''` -> `'`).
 */
export function parseYamlScalarValue(rawField: string): string {
  const trimmed = rawField.trim();
  if (!trimmed) return "";

  if (trimmed.startsWith('"')) {
    let escaped = false;
    for (let i = 1; i < trimmed.length; i++) {
      const ch = trimmed[i];
      if (escaped) {
        escaped = false;
        continue;
      }
      if (ch === "\\") {
        escaped = true;
        continue;
      }
      if (ch === '"') {
        const quotedLiteral = trimmed.slice(0, i + 1);
        try {
          const parsed = JSON.parse(quotedLiteral);
          return typeof parsed === "string" ? parsed : String(parsed);
        } catch {
          return trimmed.slice(1, i);
        }
      }
    }
    return trimmed.slice(1);
  }

  if (trimmed.startsWith("'")) {
    for (let i = 1; i < trimmed.length; i++) {
      if (trimmed[i] === "'") {
        if (trimmed[i + 1] === "'") {
          i += 1;
          continue;
        }
        return trimmed.slice(1, i).replace(/''/g, "'");
      }
    }
    return trimmed.slice(1).replace(/''/g, "'");
  }

  return trimmed.replace(/\s+#.*$/, "").trim();
}

/**
 * Extracts all raw `rtsp://...`, `rtsps://...`, `http://...`, `https://...`, `rtmp://...` URLs
 * from an existing raw YAML string so scrubbed `*:*@` URLs in `draftConfig` can be restored losslessly.
 */
function extractOriginalUrls(rawYaml: string): string[] {
  const matches = rawYaml.match(/(?:rtsps?|https?|rtmps?):\/\/[^\s"'#]+[^\s"']*/gi);
  if (!matches) return [];
  const cleaned = matches.map((u) => u.replace(/\\"/g, '"').replace(/\\\\/g, "\\"));
  return Array.from(new Set(cleaned));
}

/**
 * Extracts original camera ONVIF passwords from raw YAML so scrubbed `"***"`
 * passwords in `draft.cameras[cam].onvif.password` can be restored losslessly.
 */
function extractOriginalOnvifPasswords(rawYaml: string): Record<string, string> {
  const result: Record<string, string> = {};
  const lines = rawYaml.split(/\r?\n/);
  let inCameras = false;
  let currentCam = "";
  let inOnvif = false;

  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const indent = line.search(/\S/);
    if (indent === 0) {
      inCameras = /^cameras\s*:/.test(trimmed);
      currentCam = "";
      inOnvif = false;
      continue;
    }
    if (inCameras && indent === 2) {
      const m = trimmed.match(/^(?:"([^"]+)"|'([^']+)'|([a-zA-Z0-9_.-]+))\s*:/);
      currentCam = m ? m[1] || m[2] || m[3] || "" : "";
      inOnvif = false;
      continue;
    }
    if (inCameras && currentCam && indent === 4) {
      inOnvif = /^onvif\s*:/.test(trimmed);
      continue;
    }
    if (inCameras && currentCam && inOnvif && indent >= 6) {
      const pwMatch = trimmed.match(/^password\s*:\s*(.+)$/);
      if (pwMatch) {
        const unquoted = parseYamlScalarValue(pwMatch[1]);
        if (unquoted && unquoted !== "***" && unquoted !== "*" && unquoted !== "null") {
          result[currentCam] = unquoted;
        }
      }
    }
  }
  return result;
}

function extractOriginalGenAiApiKey(rawYaml: string): string | undefined {
  const lines = rawYaml.split(/\r?\n/);
  let inGenAi = false;
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const indent = line.search(/\S/);
    if (indent === 0) {
      inGenAi = /^genai\s*:/.test(trimmed);
      continue;
    }
    if (inGenAi && indent >= 2) {
      const m = trimmed.match(/^api_key\s*:\s*(.+)$/);
      if (m) {
        const unquoted = parseYamlScalarValue(m[1]);
        if (unquoted && unquoted !== "***" && unquoted !== "*" && unquoted !== "null") {
          return unquoted;
        }
      }
    }
  }
  return undefined;
}

function extractOriginalMqttPassword(rawYaml: string): string | undefined {
  const lines = rawYaml.split(/\r?\n/);
  let inMqtt = false;
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const indent = line.search(/\S/);
    if (indent === 0) {
      inMqtt = /^mqtt\s*:/.test(trimmed);
      continue;
    }
    if (inMqtt && indent >= 2) {
      const m = trimmed.match(/^password\s*:\s*(.+)$/);
      if (m) {
        const unquoted = parseYamlScalarValue(m[1]);
        if (unquoted && unquoted !== "***" && unquoted !== "*" && unquoted !== "null") {
          return unquoted;
        }
      }
    }
  }
  return undefined;
}

export function buildGenAiProviderChangePatch(
  provider: string,
  currentBaseUrl?: string | null
): Partial<GenAiConfig> {
  const p = provider.trim().toLowerCase();
  if (p === "openai") {
    return {
      provider: "openai",
      api_key_env: "OPENAI_API_KEY",
      model: "",
    };
  }
  if (p === "ollama") {
    return {
      provider: "ollama",
      api_key_env: "OLLAMA_API_KEY",
      base_url: currentBaseUrl || "http://127.0.0.1:11434",
      model: "",
    };
  }
  return {
    provider: "gemini",
    api_key_env: "GEMINI_API_KEY",
    model: "",
  };
}

export function normalizeDiscoveredGenAiModels(
  payload: unknown,
  provider?: string
): string[] {
  const prov = (provider || "").trim().toLowerCase();
  const obj =
    payload && typeof payload === "object"
      ? (payload as { models?: unknown[]; data?: unknown[] })
      : null;
  const rawList: unknown[] = Array.isArray(payload)
    ? payload
    : obj && Array.isArray(obj.models)
    ? obj.models
    : obj && Array.isArray(obj.data)
    ? obj.data
    : [];
  const out: string[] = [];
  for (const item of rawList) {
    if (
      prov === "gemini" &&
      item &&
      typeof item === "object" &&
      Array.isArray(
        (item as { supportedGenerationMethods?: unknown[] })
          .supportedGenerationMethods
      )
    ) {
      const methods = (item as { supportedGenerationMethods: unknown[] })
        .supportedGenerationMethods;
      if (methods.length > 0 && !methods.includes("generateContent")) {
        continue;
      }
    }
    const str =
      typeof item === "string"
        ? item
        : item && typeof item === "object"
        ? String(
            (item as { id?: string; name?: string; model?: string }).id ||
              (item as { id?: string; name?: string; model?: string }).name ||
              (item as { id?: string; name?: string; model?: string }).model ||
              ""
          )
        : "";
    const cleaned = str.trim().replace(/^models\//, "");
    if (!cleaned) continue;
    const lower = cleaned.toLowerCase();
    if (
      lower.startsWith("embedding-") ||
      lower.startsWith("text-embedding-") ||
      lower === "aqa" ||
      lower.startsWith("imagen-") ||
      lower.startsWith("veo-") ||
      lower.startsWith("whisper-") ||
      lower.startsWith("tts-") ||
      lower.startsWith("dall-e-") ||
      lower.startsWith("omni-moderation-") ||
      lower.startsWith("babbage-") ||
      lower.startsWith("davinci-")
    ) {
      continue;
    }
    if (!out.includes(cleaned)) {
      out.push(cleaned);
    }
  }
  return out;
}

function scrubUrl(input: string): string {
  const schemeIdx = input.indexOf("://");
  if (schemeIdx === -1) return input;
  const afterScheme = input.slice(schemeIdx + 3);
  const slashIdx = afterScheme.search(/[/?#]/);
  const authority = slashIdx === -1 ? afterScheme : afterScheme.slice(0, slashIdx);
  const atIdx = authority.lastIndexOf("@");
  if (atIdx === -1) return input;
  const userinfo = authority.slice(0, atIdx);
  const mask = userinfo.includes(":") ? "*:*" : "*";
  return `${input.slice(0, schemeIdx + 3)}${mask}@${afterScheme.slice(atIdx + 1)}`;
}

function extractHostFromAuthorityRest(rest: string): string {
  const slashIdx = rest.search(/[/?#]/);
  const hostPort = slashIdx === -1 ? rest : rest.slice(0, slashIdx);
  if (hostPort.startsWith("[")) {
    const closeIdx = hostPort.indexOf("]");
    if (closeIdx !== -1) {
      return hostPort.slice(0, closeIdx + 1);
    }
  }
  return hostPort.split(":")[0];
}

function restoreUrl(candidate: string, originals: string[]): string {
  if (!candidate.includes("://*:*@") && !candidate.includes("://*@")) {
    return candidate;
  }
  for (const orig of originals) {
    if (scrubUrl(orig) === candidate) {
      return orig;
    }
  }
  // Host-based match if path was modified
  const schemeIdx = candidate.indexOf("://");
  if (schemeIdx !== -1) {
    const afterScheme = candidate.slice(schemeIdx + 3);
    const atIdx = afterScheme.indexOf("@");
    if (atIdx !== -1) {
      const rest = afterScheme.slice(atIdx + 1);
      const targetHost = extractHostFromAuthorityRest(rest);
      for (const orig of originals) {
        const oScheme = orig.indexOf("://");
        if (oScheme !== -1) {
          const oAfter = orig.slice(oScheme + 3);
          const oSlash = oAfter.search(/[/?#]/);
          const oAuth = oSlash === -1 ? oAfter : oAfter.slice(0, oSlash);
          const oAt = oAuth.lastIndexOf("@");
          if (oAt !== -1) {
            const creds = oAuth.slice(0, oAt);
            const oHost = extractHostFromAuthorityRest(oAuth.slice(oAt + 1));
            if (oHost === targetHost) {
              return `${candidate.slice(0, schemeIdx + 3)}${creds}@${rest}`;
            }
          }
        }
      }
    }
  }
  return candidate;
}

const YAML_RESERVED_SCALAR_RE =
  /^(true|false|null|yes|no|on|off|~|\.nan|\.inf|-\.inf|\+\.inf)$/i;
const YAML_NUMERIC_STRING_RE =
  /^(?:[+-]?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?|0[xXoObB][0-9a-fA-F_]+|0\d+)$/;

function yamlScalar(val: unknown): string {
  if (val === null || val === undefined) return "null";
  if (typeof val === "boolean") return String(val);
  if (typeof val === "number") {
    return Number.isFinite(val) ? String(val) : "0";
  }
  const s = String(val);
  if (
    s === "" ||
    /[\r\n\t\u0000-\u001f\u007f:#[\]{}&*!|>'"%@`,?]/.test(s) ||
    /^\s|\s$/.test(s) ||
    /^[-?]/.test(s) ||
    YAML_RESERVED_SCALAR_RE.test(s) ||
    YAML_NUMERIC_STRING_RE.test(s)
  ) {
    return JSON.stringify(s);
  }
  return s;
}

function yamlKey(key: string): string {
  if (
    !key ||
    !/^[a-zA-Z0-9_-]+$/.test(key) ||
    key.startsWith("-") ||
    YAML_RESERVED_SCALAR_RE.test(key) ||
    YAML_NUMERIC_STRING_RE.test(key)
  ) {
    return JSON.stringify(key);
  }
  return key;
}

function toYamlLines(val: unknown, indent = 0): string[] {
  const pad = "  ".repeat(indent);
  if (val === null || val === undefined) {
    return [`${pad}null`];
  }
  if (typeof val !== "object") {
    return [`${pad}${yamlScalar(val)}`];
  }
  if (Array.isArray(val)) {
    if (val.length === 0) return [`${pad}[]`];
    const lines: string[] = [];
    for (const item of val) {
      if (item !== null && typeof item === "object" && !Array.isArray(item)) {
        const entries = Object.entries(item as Record<string, unknown>).filter(
          ([, v]) => v !== undefined && v !== null
        );
        if (entries.length === 0) {
          lines.push(`${pad}- {}`);
        } else {
          const [rawFirstKey, firstVal] = entries[0];
          const firstKey = yamlKey(rawFirstKey);
          if (firstVal !== null && typeof firstVal === "object") {
            if (Array.isArray(firstVal) && firstVal.length === 0) {
              lines.push(`${pad}- ${firstKey}: []`);
            } else if (!Array.isArray(firstVal) && Object.keys(firstVal as object).length === 0) {
              lines.push(`${pad}- ${firstKey}: {}`);
            } else {
              lines.push(`${pad}- ${firstKey}:`);
              lines.push(...toYamlLines(firstVal, indent + 2));
            }
          } else {
            lines.push(`${pad}- ${firstKey}: ${yamlScalar(firstVal)}`);
          }
          for (let i = 1; i < entries.length; i++) {
            const [rawK, v] = entries[i];
            const k = yamlKey(rawK);
            if (v !== null && typeof v === "object") {
              if (Array.isArray(v) && v.length === 0) {
                lines.push(`${pad}  ${k}: []`);
              } else if (!Array.isArray(v) && Object.keys(v as object).length === 0) {
                lines.push(`${pad}  ${k}: {}`);
              } else {
                lines.push(`${pad}  ${k}:`);
                lines.push(...toYamlLines(v, indent + 2));
              }
            } else {
              lines.push(`${pad}  ${k}: ${yamlScalar(v)}`);
            }
          }
        }
      } else {
        lines.push(`${pad}- ${yamlScalar(item)}`);
      }
    }
    return lines;
  }

  const entries = Object.entries(val as Record<string, unknown>).filter(
    ([, v]) => v !== undefined && v !== null
  );
  if (entries.length === 0) return [`${pad}{}`];
  const lines: string[] = [];
  for (const [k, v] of entries) {
    const safeKey = yamlKey(k);
    if (v !== null && typeof v === "object") {
      if (Array.isArray(v) && v.length === 0) {
        lines.push(`${pad}${safeKey}: []`);
      } else if (!Array.isArray(v) && Object.keys(v as object).length === 0) {
        lines.push(`${pad}${safeKey}: {}`);
      } else {
        lines.push(`${pad}${safeKey}:`);
        lines.push(...toYamlLines(v, indent + 1));
      }
    } else {
      lines.push(`${pad}${safeKey}: ${yamlScalar(v)}`);
    }
  }
  return lines;
}

function finiteRound(
  val: number | undefined | null,
  fallback: number,
  decimals: number
): number {
  const safe = typeof val === "number" && Number.isFinite(val) ? val : fallback;
  return Number(safe.toFixed(decimals));
}

/**
 * Builds a clean, NinjaNVR-compatible `config.yml` string from `draftConfig`,
 * preserving any unscrubbed RTSP URLs from `originalRawYaml`.
 */
export function buildCleanYamlFromDraft(
  draft: NinjaConfigResponse,
  originalRawYaml: string = ""
): string {
  const originals = extractOriginalUrls(originalRawYaml);
  const originalOnvifPasswords = extractOriginalOnvifPasswords(originalRawYaml);
  const originalGenAiApiKey = extractOriginalGenAiApiKey(originalRawYaml);
  const originalMqttPassword = extractOriginalMqttPassword(originalRawYaml);

  const streamsOut: Record<string, unknown> = {};
  for (const [sName, sVal] of Object.entries(draft.go2rtc?.streams || {})) {
    if (Array.isArray(sVal)) {
      streamsOut[sName] = sVal.map((u) =>
        typeof u === "string" ? restoreUrl(u, originals) : u
      );
    } else if (typeof sVal === "string") {
      streamsOut[sName] = [restoreUrl(sVal, originals)];
    } else {
      streamsOut[sName] = sVal;
    }
  }

  const camerasOut: Record<string, unknown> = {};
  for (const [camName, cam] of Object.entries(draft.cameras || {})) {
    const inputs = (cam.ffmpeg?.inputs || []).map((inp) => ({
      path: restoreUrl(inp.path, originals),
      input_args: inp.input_args || "preset-rtsp-restream",
      roles: inp.roles && inp.roles.length > 0 ? inp.roles : ["detect"],
    }));

    camerasOut[camName] = {
      enabled: cam.enabled,
      ffmpeg: {
        inputs,
        output_args: {
          record: "preset-record-generic-audio-aac",
        },
      },
      live: cam.live || {
        streams: {
          "Stream 1": `${camName}_sub`,
          "Stream 2": camName,
        },
      },
      detect: {
        enabled: cam.detect?.enabled ?? true,
        width: cam.detect?.width ?? 1280,
        height: cam.detect?.height ?? 720,
        fps: cam.detect?.fps ?? 5,
        min_initialized: cam.detect?.min_initialized ?? 2,
        max_disappeared: cam.detect?.max_disappeared ?? 25,
        stationary: {
          interval: cam.detect?.stationary?.interval ?? 50,
          threshold: cam.detect?.stationary?.threshold ?? 50,
          classifier: cam.detect?.stationary?.classifier ?? true,
        },
      },
      motion: {
        enabled: cam.motion?.enabled ?? true,
        threshold: cam.motion?.threshold ?? 30,
        lightning_threshold: finiteRound(
          cam.motion?.lightning_threshold,
          0.8,
          2
        ),
        improve_contrast: cam.motion?.improve_contrast ?? true,
        contour_area: cam.motion?.contour_area ?? 10,
        frame_height: cam.motion?.frame_height ?? 100,
        mask: cam.motion?.mask || {},
      },
      objects: {
        track: cam.objects?.track || draft.objects?.track || ["person"],
        ...(cam.objects?.attribute_labels && cam.objects.attribute_labels.length > 0
          ? { attribute_labels: cam.objects.attribute_labels }
          : draft.objects?.attribute_labels && draft.objects.attribute_labels.length > 0
          ? { attribute_labels: draft.objects.attribute_labels }
          : {}),
        filters: cam.objects?.filters || {},
        mask: cam.objects?.mask || {},
      },
      record: {
        enabled: cam.record?.enabled ?? true,
        continuous: { days: cam.record?.continuous?.days ?? 14 },
        motion: { days: cam.record?.motion?.days ?? 0 },
        alerts: cam.record?.alerts || {
          pre_capture: 5,
          post_capture: 5,
          retain: { days: 14, mode: "motion" },
        },
        detections: cam.record?.detections || {
          pre_capture: 5,
          post_capture: 5,
          retain: { days: 14, mode: "motion" },
        },
        preview: cam.record?.preview || { quality: "medium" },
        ...(cam.record?.cold_compaction
          ? { cold_compaction: cam.record.cold_compaction }
          : {}),
      },
      snapshots: {
        enabled: cam.snapshots?.enabled ?? true,
        timestamp: cam.snapshots?.timestamp ?? false,
        bounding_box: cam.snapshots?.bounding_box ?? true,
        crop: cam.snapshots?.crop ?? false,
        quality: cam.snapshots?.quality ?? 70,
        selection:
          cam.snapshots?.selection ||
          draft.snapshots?.selection ||
          "highest_confidence",
      },
      review: cam.review || {
        alerts: {
          enabled: true,
          labels: ["person", "car"],
          ...(draft.review?.alerts?.sub_labels &&
          draft.review.alerts.sub_labels.length > 0
            ? { sub_labels: draft.review.alerts.sub_labels }
            : {}),
          ...(draft.review?.alerts?.exclude_sub_labels &&
          draft.review.alerts.exclude_sub_labels.length > 0
            ? { exclude_sub_labels: draft.review.alerts.exclude_sub_labels }
            : {}),
          required_zones: [],
          cutoff_time: 40,
        },
        detections: {
          enabled: true,
          required_zones: [],
          cutoff_time: 30,
        },
      },
      zones: Object.fromEntries(
        Object.entries(cam.zones || {}).map(([zName, zCfg]) => {
          const cleanPreset = zCfg.preset ? zCfg.preset.trim() : "";
          return [
            zName,
            {
              coordinates: zCfg.coordinates,
              ...(zCfg.objects ? { objects: zCfg.objects } : {}),
              ...(zCfg.inertia !== undefined ? { inertia: zCfg.inertia } : {}),
              ...(zCfg.loitering_time !== undefined
                ? { loitering_time: zCfg.loitering_time }
                : {}),
              ...(zCfg.min_area !== undefined ? { min_area: zCfg.min_area } : {}),
              ...(zCfg.max_area !== undefined ? { max_area: zCfg.max_area } : {}),
              ...(zCfg.min_score !== undefined ? { min_score: zCfg.min_score } : {}),
              ...(cleanPreset ? { preset: cleanPreset } : {}),
            },
          ];
        })
      ),
      lines: cam.lines || {},
      ...(cam.state_rois && Object.keys(cam.state_rois).length > 0
        ? {
            state_rois: Object.fromEntries(
              Object.entries(cam.state_rois).map(([roiName, roiCfg]) => [
                roiName,
                {
                  crop: (roiCfg.crop || [0.25, 0.25, 0.75, 0.75]).map((v) =>
                    finiteRound(Number(v), 0, 4)
                  ),
                  classes:
                    roiCfg.classes && roiCfg.classes.length > 0
                      ? roiCfg.classes
                      : ["closed", "open"],
                  interval_sec: roiCfg.interval_sec ?? 5,
                  hysteresis: roiCfg.hysteresis ?? 3,
                  ...(roiCfg.anomaly_state
                    ? { anomaly_state: roiCfg.anomaly_state }
                    : {}),
                  anomaly_timeout_sec: roiCfg.anomaly_timeout_sec ?? 300,
                },
              ])
            ),
          }
        : {}),
      privacy_mask: cam.privacy_mask || [],
      ...(cam.onvif &&
      ((cam.onvif.host && cam.onvif.host.trim().length > 0) ||
        (cam.onvif.user && cam.onvif.user.trim().length > 0) ||
        Boolean(cam.onvif.autotracking) ||
        (cam.onvif.return_preset && cam.onvif.return_preset.trim().length > 0))
        ? (() => {
            const rawPass = cam.onvif.clear_password
              ? undefined
              : cam.onvif.password &&
                cam.onvif.password !== "***" &&
                cam.onvif.password !== "*"
              ? cam.onvif.password
              : originalOnvifPasswords[camName];
            return {
              onvif: {
                ...(cam.onvif.host && cam.onvif.host.trim()
                  ? { host: restoreUrl(cam.onvif.host.trim(), originals) }
                  : {}),
                port: cam.onvif.port ?? 80,
                ...(cam.onvif.user && cam.onvif.user.trim()
                  ? { user: cam.onvif.user.trim() }
                  : {}),
                ...(rawPass ? { password: rawPass } : {}),
                autotracking: Boolean(cam.onvif.autotracking),
                ...(cam.onvif.return_preset && cam.onvif.return_preset.trim()
                  ? { return_preset: cam.onvif.return_preset.trim() }
                  : {}),
                timeout_sec: cam.onvif.timeout_sec ?? 5,
              },
            };
          })()
        : {}),
      ...(cam.ui &&
      (cam.ui.order !== undefined || cam.ui.dashboard !== undefined)
        ? {
            ui: {
              order: cam.ui.order ?? 0,
              dashboard: cam.ui.dashboard ?? true,
            },
          }
        : {}),
      ...(cam.map
        ? {
            map: {
              x: finiteRound(cam.map.x, 0.5, 3),
              y: finiteRound(cam.map.y, 0.5, 3),
              angle_deg: finiteRound(cam.map.angle_deg, 0, 1),
              fov_deg: finiteRound(cam.map.fov_deg, 90, 1),
              range: finiteRound(cam.map.range, 0.25, 3),
            },
          }
        : {}),
    };
  }

  const detectorsOut: Record<string, unknown> = {};
  for (const [dName, dCfg] of Object.entries(
    draft.detectors || { detector01: { type: "openvino", device: "CPU" } }
  )) {
    detectorsOut[dName] = {
      type: dCfg.type || "openvino",
      device: dCfg.device || "CPU",
      ...(dCfg.model ? { model: dCfg.model } : {}),
    };
  }

  const rawMqttPassword = draft.mqtt?.clear_password
    ? undefined
    : draft.mqtt?.password &&
      draft.mqtt.password !== "***" &&
      draft.mqtt.password !== "*"
    ? draft.mqtt.password
    : originalMqttPassword;

  const rootObj: Record<string, unknown> = {
    mqtt: {
      enabled: draft.mqtt?.enabled ?? true,
      host: draft.mqtt?.host || "127.0.0.1",
      port: draft.mqtt?.port ?? 1883,
      topic_prefix: draft.mqtt?.topic_prefix || "ninjanvr",
      client_id: draft.mqtt?.client_id || "ninjanvr",
      stats_interval: draft.mqtt?.stats_interval ?? 60,
      ...(draft.mqtt?.user ? { user: draft.mqtt.user } : {}),
      ...(rawMqttPassword ? { password: rawMqttPassword } : {}),
    },
    auth: {
      enabled: draft.auth?.enabled ?? true,
      cookie_name: draft.auth?.cookie_name || "ninjanvr_token",
      cookie_secure: draft.auth?.cookie_secure ?? false,
      session_length: draft.auth?.session_length ?? 86400,
      refresh_time: draft.auth?.refresh_time ?? 1800,
      hash_iterations: draft.auth?.hash_iterations ?? 600000,
    },
    tls: {
      enabled: draft.tls?.enabled ?? true,
    },
    database: {
      path: draft.database?.path || "/config/ninjanvr.db",
    },
    detectors: detectorsOut,
    model: {
      width: draft.model?.width ?? 300,
      height: draft.model?.height ?? 300,
      input_tensor: draft.model?.input_tensor || "nhwc",
      input_pixel_format: draft.model?.input_pixel_format || "bgr",
      path: draft.model?.path || "/openvino-model/ssdlite_mobilenet_v2.xml",
      labelmap_path:
        draft.model?.labelmap_path || "/openvino-model/coco_91cl_bkgr.txt",
      model_type: draft.model?.model_type || "ssd",
    },
    ...(draft.genai
      ? (() => {
          const rawApiKey = draft.genai.clear_api_key
            ? undefined
            : draft.genai.api_key &&
              draft.genai.api_key.trim() !== "" &&
              draft.genai.api_key.trim() !== "***" &&
              draft.genai.api_key.trim() !== "*"
            ? draft.genai.api_key.trim()
            : originalGenAiApiKey;
          return {
            genai: {
              enabled: draft.genai.enabled ?? false,
              provider: draft.genai.provider || "gemini",
              ...(draft.genai.base_url
                ? { base_url: restoreUrl(draft.genai.base_url, originals) }
                : {}),
              ...(rawApiKey ? { api_key: rawApiKey } : {}),
              api_key_env: draft.genai.api_key_env || "GEMINI_API_KEY",
              model: draft.genai.model ?? "",
              verify_ambiguous_alerts:
                draft.genai.verify_ambiguous_alerts ?? false,
              ambiguous_score_min: finiteRound(
                draft.genai.ambiguous_score_min,
                0.45,
                2
              ),
              ambiguous_score_max: finiteRound(
                draft.genai.ambiguous_score_max,
                0.72,
                2
              ),
              auto_suppress_false_positives:
                draft.genai.auto_suppress_false_positives ?? true,
              ...(draft.genai.timeout_ms
                ? { timeout_ms: draft.genai.timeout_ms }
                : {}),
            },
          };
        })()
      : {}),
    ...(draft.face_recognition
      ? {
          face_recognition: {
            enabled: draft.face_recognition.enabled ?? false,
            model_path:
              draft.face_recognition.model_path ||
              "/config/models/arcface_r50.onnx",
            similarity_threshold: finiteRound(
              draft.face_recognition.similarity_threshold,
              0.68,
              2
            ),
            min_face_area: draft.face_recognition.min_face_area ?? 750,
            unknown_alert: draft.face_recognition.unknown_alert ?? false,
          },
        }
      : {}),
    ...(draft.review ? { review: draft.review } : {}),
    detect: {
      enabled: draft.detect?.enabled ?? true,
      width: draft.detect?.width ?? 1280,
      height: draft.detect?.height ?? 720,
      fps: draft.detect?.fps ?? 5,
      min_initialized: draft.detect?.min_initialized ?? 2,
      max_disappeared: draft.detect?.max_disappeared ?? 25,
    },
    objects: {
      track: draft.objects?.track || ["person"],
      ...(draft.objects?.attribute_labels && draft.objects.attribute_labels.length > 0
        ? { attribute_labels: draft.objects.attribute_labels }
        : {}),
      filters: draft.objects?.filters || {},
    },
    ...(draft.lpr
      ? {
          lpr: {
            enabled: draft.lpr.enabled ?? false,
            ...(draft.lpr.detector_model_path
              ? { detector_model_path: draft.lpr.detector_model_path }
              : {}),
            ...(draft.lpr.ocr_model_path
              ? { ocr_model_path: draft.lpr.ocr_model_path }
              : {}),
            min_confidence: finiteRound(
              draft.lpr.min_confidence,
              0.75,
              2
            ),
            ...(draft.lpr.format_regex
              ? { format_regex: draft.lpr.format_regex }
              : {}),
            known_plates: draft.lpr.known_plates || {},
          },
        }
      : {}),
    record: {
      enabled: draft.record?.enabled ?? true,
      expire_interval: draft.record?.expire_interval ?? 60,
      continuous: { days: draft.record?.continuous?.days ?? 14 },
      motion: { days: draft.record?.motion?.days ?? 0 },
      alerts: draft.record?.alerts || {
        pre_capture: 5,
        post_capture: 5,
        retain: { days: 14, mode: "motion" },
      },
      detections: draft.record?.detections || {
        pre_capture: 5,
        post_capture: 5,
        retain: { days: 14, mode: "motion" },
      },
      ...(draft.record?.cold_compaction
        ? { cold_compaction: draft.record.cold_compaction }
        : {}),
    },
    snapshots: {
      enabled: draft.snapshots?.enabled ?? true,
      timestamp: draft.snapshots?.timestamp ?? false,
      bounding_box: draft.snapshots?.bounding_box ?? true,
      crop: draft.snapshots?.crop ?? false,
      quality: draft.snapshots?.quality ?? 70,
      ...(draft.snapshots?.selection
        ? { selection: draft.snapshots.selection }
        : {}),
    },
    go2rtc: {
      streams: streamsOut,
    },
    ...(draft.camera_groups && Object.keys(draft.camera_groups).length > 0
      ? { camera_groups: draft.camera_groups }
      : {}),
    ...(draft.ui &&
    (draft.ui.floorplan ||
      draft.ui.time_format !== undefined ||
      draft.ui.unit_system !== undefined ||
      draft.ui.theme !== undefined ||
      draft.ui.high_contrast !== undefined)
      ? {
          ui: {
            ...(draft.ui.time_format ? { time_format: draft.ui.time_format } : {}),
            ...(draft.ui.unit_system ? { unit_system: draft.ui.unit_system } : {}),
            ...(draft.ui.theme ? { theme: draft.ui.theme } : {}),
            ...(draft.ui.high_contrast !== undefined
              ? { high_contrast: Boolean(draft.ui.high_contrast) }
              : {}),
            ...(draft.ui.floorplan
              ? {
                  floorplan: {
                    ...(draft.ui.floorplan.background_data_url
                      ? {
                          background_data_url:
                            draft.ui.floorplan.background_data_url,
                        }
                      : {}),
                    opacity: finiteRound(
                      draft.ui.floorplan.opacity,
                      0.45,
                      2
                    ),
                    ...(draft.ui.floorplan.width_m !== undefined
                      ? { width_m: finiteRound(draft.ui.floorplan.width_m, 20, 2) }
                      : {}),
                    ...(draft.ui.floorplan.height_m !== undefined
                      ? { height_m: finiteRound(draft.ui.floorplan.height_m, 12, 2) }
                      : {}),
                    rooms: (draft.ui.floorplan.rooms || []).map((r) => ({
                      id: r.id,
                      label: r.label,
                      x: finiteRound(r.x, 0, 3),
                      y: finiteRound(r.y, 0, 3),
                      w: finiteRound(r.w, 0.1, 3),
                      h: finiteRound(r.h, 0.1, 3),
                      ...(r.color ? { color: r.color } : {}),
                    })),
                    walls: (draft.ui.floorplan.walls || []).map((w) => ({
                      points: w.points.map((p) => finiteRound(p, 0, 3)),
                      ...(w.label ? { label: w.label } : {}),
                      ...(w.kind && w.kind !== "wall" ? { kind: w.kind } : {}),
                    })),
                  },
                }
              : {}),
          },
        }
      : {}),
    cameras: camerasOut,
    version: draft.version || "0.18-0",
  };

  return `${toYamlLines(rootObj).join("\n")}\n`;
}

export const DEFAULT_LPR_FORMAT_REGEX =
  "^([A-Z]{3}[0-9][A-Z0-9][0-9]{2}|[A-Z]{2,3}[0-9]{3,4}|[A-Z]{1,3}[0-9]{1,4}[A-Z]{1,3}|[0-9][A-Z]{3}[0-9]{3})$";

/**
 * Normalizes a raw license plate string by keeping ASCII alphanumeric characters and uppercasing.
 */
export function normalizeLicensePlate(raw: string): string {
  return (raw || "")
    .replace(/[^a-zA-Z0-9]/g, "")
    .toUpperCase();
}

/**
 * Validates a license plate string against `formatRegex` (or `DEFAULT_LPR_FORMAT_REGEX` supporting Mercosul & legacy formats).
 */
export function validateLicensePlateFormat(
  plate: string,
  formatRegex?: string | null
): boolean {
  const norm = normalizeLicensePlate(plate);
  if (norm.length < 4 || norm.length > 10) return false;
  const trimmedRegex =
    formatRegex && formatRegex.trim().length > 0 && formatRegex.trim().length <= 256
      ? formatRegex.trim()
      : DEFAULT_LPR_FORMAT_REGEX;
  try {
    const anchored = `^(?:${trimmedRegex.replace(/^\^/, "").replace(/\$$/, "")})$`;
    return new RegExp(anchored).test(norm);
  } catch {
    return new RegExp(DEFAULT_LPR_FORMAT_REGEX).test(norm);
  }
}

/**
 * Looks up a friendly label for a plate in `knownPlates` (case- and hyphen-insensitive).
 */
export function resolveKnownPlateLabel(
  plate: string,
  knownPlates?: Record<string, string> | null
): string | null {
  const norm = normalizeLicensePlate(plate);
  if (!norm || !knownPlates) return null;
  if (knownPlates[norm]) return knownPlates[norm];
  for (const [k, v] of Object.entries(knownPlates)) {
    if (normalizeLicensePlate(k) === norm) {
      return v;
    }
  }
  return null;
}

/**
 * Formats a monospace license plate badge string with optional `knownPlates` or `subLabel` match.
 */
export function formatLicensePlateBadge(
  plate: string,
  knownPlates?: Record<string, string> | null,
  subLabel?: string | null
): string {
  const norm = normalizeLicensePlate(plate);
  if (!norm) return "";
  const resolved =
    resolveKnownPlateLabel(norm, knownPlates) ||
    (subLabel && subLabel.trim().length > 0 ? subLabel.trim() : null);
  return resolved ? `${norm} · ${resolved.toUpperCase()}` : norm;
}
