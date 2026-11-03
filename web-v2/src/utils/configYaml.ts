import { NinjaConfigResponse } from "../types/nvr";

/**
 * Extracts all raw `rtsp://...` or `rtsps://...` URLs from an existing raw YAML string
 * so scrubbed `*:*@` URLs in `draftConfig` can be restored losslessly.
 */
function extractOriginalUrls(rawYaml: string): string[] {
  const matches = rawYaml.match(/rtsps?:\/\/[^\s"'#]+[^\s"']*/gi);
  return matches ? Array.from(new Set(matches)) : [];
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
      const targetHost = rest.split(/[/?#:]/)[0];
      for (const orig of originals) {
        const oScheme = orig.indexOf("://");
        if (oScheme !== -1) {
          const oAfter = orig.slice(oScheme + 3);
          const oSlash = oAfter.search(/[/?#]/);
          const oAuth = oSlash === -1 ? oAfter : oAfter.slice(0, oSlash);
          const oAt = oAuth.lastIndexOf("@");
          if (oAt !== -1) {
            const creds = oAuth.slice(0, oAt);
            const oHost = oAuth.slice(oAt + 1).split(":")[0];
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

function yamlScalar(val: unknown): string {
  if (val === null || val === undefined) return "null";
  if (typeof val === "boolean" || typeof val === "number") return String(val);
  const s = String(val);
  if (
    s === "" ||
    /[:#[\]{}&*!|>'"%@`]/.test(s) ||
    /^\s|\s$/.test(s) ||
    /^(true|false|null|yes|no|on|off)$/i.test(s)
  ) {
    return JSON.stringify(s);
  }
  return s;
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
          const [firstKey, firstVal] = entries[0];
          if (firstVal !== null && typeof firstVal === "object") {
            lines.push(`${pad}- ${firstKey}:`);
            lines.push(...toYamlLines(firstVal, indent + 2));
          } else {
            lines.push(`${pad}- ${firstKey}: ${yamlScalar(firstVal)}`);
          }
          for (let i = 1; i < entries.length; i++) {
            const [k, v] = entries[i];
            if (v !== null && typeof v === "object") {
              if (Array.isArray(v) && v.length === 0) {
                lines.push(`${pad}  ${k}: []`);
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
    const safeKey = /^[a-zA-Z0-9_-]+$/.test(k) ? k : JSON.stringify(k);
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

/**
 * Builds a clean, NinjaNVR-compatible `config.yml` string from `draftConfig`,
 * preserving any unscrubbed RTSP URLs from `originalRawYaml`.
 */
export function buildCleanYamlFromDraft(
  draft: NinjaConfigResponse,
  originalRawYaml: string
): string {
  const originals = extractOriginalUrls(originalRawYaml);

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
        lightning_threshold: Number(
          (cam.motion?.lightning_threshold ?? 0.8).toFixed(2)
        ),
        improve_contrast: cam.motion?.improve_contrast ?? true,
        contour_area: cam.motion?.contour_area ?? 10,
        frame_height: cam.motion?.frame_height ?? 100,
        mask: cam.motion?.mask || {},
      },
      objects: {
        track: cam.objects?.track || draft.objects?.track || ["person"],
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
      },
      snapshots: {
        enabled: cam.snapshots?.enabled ?? true,
        timestamp: cam.snapshots?.timestamp ?? false,
        bounding_box: cam.snapshots?.bounding_box ?? true,
        crop: cam.snapshots?.crop ?? false,
        quality: cam.snapshots?.quality ?? 70,
        selection: cam.snapshots?.selection || "highest_confidence",
      },
      review: cam.review || {
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
      },
      zones: cam.zones || {},
      lines: cam.lines || {},
      privacy_mask: cam.privacy_mask || [],
    };
  }

  const detectorsOut: Record<string, unknown> = {};
  for (const [dName, dCfg] of Object.entries(
    draft.detectors || { detector01: { type: "openvino", device: "CPU" } }
  )) {
    detectorsOut[dName] = {
      type: dCfg.type || "openvino",
      device: dCfg.device || "CPU",
    };
  }

  const rootObj: Record<string, unknown> = {
    mqtt: {
      enabled: draft.mqtt?.enabled ?? true,
      host: draft.mqtt?.host || "127.0.0.1",
      port: draft.mqtt?.port ?? 1883,
      topic_prefix: draft.mqtt?.topic_prefix || "ninjanvr",
      client_id: draft.mqtt?.client_id || "ninjanvr",
      stats_interval: draft.mqtt?.stats_interval ?? 60,
      ...(draft.mqtt?.user ? { user: draft.mqtt.user } : {}),
      ...(draft.mqtt?.password ? { password: draft.mqtt.password } : {}),
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
      filters: draft.objects?.filters || {},
    },
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
    },
    snapshots: {
      enabled: draft.snapshots?.enabled ?? true,
      timestamp: draft.snapshots?.timestamp ?? false,
      bounding_box: draft.snapshots?.bounding_box ?? true,
      crop: draft.snapshots?.crop ?? false,
      quality: draft.snapshots?.quality ?? 70,
    },
    go2rtc: {
      streams: streamsOut,
    },
    cameras: camerasOut,
    version: draft.version || "0.18-0",
  };

  return `${toYamlLines(rootObj).join("\n")}\n`;
}
