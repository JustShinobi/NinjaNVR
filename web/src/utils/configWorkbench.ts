import React, { createContext, useContext, useSyncExternalStore } from "react";
import type {
  CameraConfig,
  CameraHardwareConfig,
  GenAiConfig,
  MqttConfig,
  NinjaConfigResponse,
  OnvifCameraConfig,
} from "../types/nvr";
import {
  buildCleanYamlFromDraft,
  isSecretMaskSentinel,
  parseYamlScalarValue,
  resolveSecretFieldAction,
} from "./configYaml";
import {
  applyStructuredDraftMutation,
  type StructuredDraftMutationResult,
  synthesizeDraftYamlIfNeeded,
} from "./scopedConfigDraft";
import { sanitizeConfigGeometryDraft } from "./visionGeometry";

const UNSAFE_OBJECT_KEYS = new Set(["__proto__", "constructor", "prototype"]);

const TOP_LEVEL_REPLACE_MAP_KEYS = new Set([
  "detectors",
  "profiles",
  "peers",
  "camera_groups",
]);

const CAMERA_REPLACE_MAP_KEYS = new Set([
  "zones",
  "lines",
  "state_rois",
  "soft_presets",
  "filters",
  "streams",
  "mask",
  "privacy_mask",
  "per_camera_dir",
]);

const NESTED_REPLACE_MAP_KEYS = new Set([
  ...CAMERA_REPLACE_MAP_KEYS,
  "known_plates",
]);

function isPlainObject(val: unknown): val is Record<string, unknown> {
  return val !== null && typeof val === "object" && !Array.isArray(val);
}

function cloneSafe<T>(val: T): T {
  if (val === null || val === undefined || typeof val !== "object") {
    return val;
  }
  return JSON.parse(JSON.stringify(val)) as T;
}

export function deepEqualConfigValue(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (a === null || b === null || a === undefined || b === undefined) {
    return a === b;
  }
  if (typeof a !== typeof b) return false;
  if (typeof a !== "object") return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a) && Array.isArray(b)) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) {
      if (!deepEqualConfigValue(a[i], b[i])) return false;
    }
    return true;
  }
  const objA = a as Record<string, unknown>;
  const objB = b as Record<string, unknown>;
  const keysA = Object.keys(objA).filter(
    (k) => !UNSAFE_OBJECT_KEYS.has(k) && objA[k] !== undefined
  );
  const keysB = Object.keys(objB).filter(
    (k) => !UNSAFE_OBJECT_KEYS.has(k) && objB[k] !== undefined
  );
  if (keysA.length !== keysB.length) return false;
  for (const k of keysA) {
    if (!Object.prototype.hasOwnProperty.call(objB, k)) return false;
    if (!deepEqualConfigValue(objA[k], objB[k])) return false;
  }
  return true;
}

function mergeJsonTree(
  base: Record<string, unknown>,
  patch: Record<string, unknown>,
  replaceMapKeys: ReadonlySet<string>
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(base)) {
    if (UNSAFE_OBJECT_KEYS.has(k)) continue;
    out[k] = cloneSafe(v);
  }
  for (const [k, v] of Object.entries(patch)) {
    if (UNSAFE_OBJECT_KEYS.has(k) || v === undefined) continue;
    if (replaceMapKeys.has(k)) {
      out[k] = cloneSafe(v);
    } else if (isPlainObject(out[k]) && isPlainObject(v)) {
      out[k] = mergeJsonTree(
        out[k] as Record<string, unknown>,
        v,
        replaceMapKeys
      );
    } else {
      out[k] = cloneSafe(v);
    }
  }
  return out;
}

function hasExplicitProperty(
  obj: Record<string, unknown> | undefined,
  key: string
): boolean {
  return Boolean(
    obj &&
      Object.prototype.hasOwnProperty.call(obj, key) &&
      obj[key] !== undefined
  );
}

function normalizeScrubbedMqttSecret(
  prevMqtt: MqttConfig | undefined,
  mergedMqtt: MqttConfig,
  patchMqtt: Record<string, unknown>,
  onlyPatchedSecrets = false
): MqttConfig {
  const patchTouchedPassword =
    hasExplicitProperty(patchMqtt, "password") ||
    hasExplicitProperty(patchMqtt, "clear_password");
  if (!patchTouchedPassword && (onlyPatchedSecrets || prevMqtt !== undefined)) {
    return mergedMqtt;
  }
  const clearPass = Boolean(patchMqtt.clear_password);
  const prevHas = Boolean(
    prevMqtt?.has_password ||
      (prevMqtt?.password && prevMqtt.password.trim().length > 0)
  );
  const resolved = resolveSecretFieldAction({
    rawInput:
      typeof patchMqtt.password === "string"
        ? patchMqtt.password
        : mergedMqtt.password,
    hasExistingSecret: prevHas,
    clearRequested: clearPass,
  });
  const next: MqttConfig = { ...mergedMqtt };
  delete next.clear_password;
  if (resolved.action === "clear") {
    delete next.password;
    next.has_password = false;
  } else if (resolved.action === "replace") {
    next.password = "***";
    next.has_password = true;
  } else {
    if (resolved.hasExisting) {
      next.password = "***";
      next.has_password = true;
    } else {
      delete next.password;
      next.has_password = false;
    }
  }
  return next;
}

function normalizeScrubbedGenAiSecret(
  prevGenAi: GenAiConfig | undefined,
  mergedGenAi: GenAiConfig,
  patchGenAi: Record<string, unknown>,
  onlyPatchedSecrets = false
): GenAiConfig {
  const patchTouchedApiKey =
    hasExplicitProperty(patchGenAi, "api_key") ||
    hasExplicitProperty(patchGenAi, "clear_api_key");
  if (!patchTouchedApiKey && (onlyPatchedSecrets || prevGenAi !== undefined)) {
    return mergedGenAi;
  }
  const clearKey = Boolean(patchGenAi.clear_api_key);
  const prevHas = Boolean(
    prevGenAi?.has_api_key ||
      (prevGenAi?.api_key && prevGenAi.api_key.trim().length > 0)
  );
  const resolved = resolveSecretFieldAction({
    rawInput:
      typeof patchGenAi.api_key === "string"
        ? patchGenAi.api_key
        : mergedGenAi.api_key,
    hasExistingSecret: prevHas,
    clearRequested: clearKey,
    trimValue: true,
  });
  const next: GenAiConfig = { ...mergedGenAi };
  delete next.clear_api_key;
  if (resolved.action === "clear") {
    next.api_key = null;
    next.has_api_key = false;
  } else if (resolved.action === "replace") {
    next.api_key = "***";
    next.has_api_key = true;
  } else {
    next.api_key = resolved.hasExisting ? "***" : null;
    next.has_api_key = resolved.hasExisting;
  }
  return next;
}

function normalizeScrubbedCameraSecrets(
  prevCam: CameraConfig | undefined,
  mergedCam: CameraConfig,
  patchCam: Record<string, unknown>,
  onlyPatchedSecrets = false
): CameraConfig {
  const nextCam: CameraConfig = { ...mergedCam };
  const onvifPatch = isPlainObject(patchCam.onvif) ? patchCam.onvif : undefined;
  const hwPatch = isPlainObject(patchCam.hardware)
    ? patchCam.hardware
    : undefined;

  const patchTouchedOnvifPassword =
    hasExplicitProperty(onvifPatch, "password") ||
    hasExplicitProperty(onvifPatch, "clear_password");
  const shouldScrubOnvifPassword = onlyPatchedSecrets
    ? patchTouchedOnvifPassword
    : patchTouchedOnvifPassword || (!prevCam?.onvif && Boolean(onvifPatch));

  if (shouldScrubOnvifPassword) {
    const prevOnvif = prevCam?.onvif;
    const prevHasPass = Boolean(
      prevOnvif?.has_password ||
        (prevOnvif?.password && prevOnvif.password.trim().length > 0)
    );
    const passResolved = resolveSecretFieldAction({
      rawInput:
        typeof onvifPatch?.password === "string"
          ? onvifPatch.password
          : nextCam.onvif?.password,
      hasExistingSecret: prevHasPass,
      clearRequested: Boolean(onvifPatch?.clear_password),
    });
    const nextOnvif: OnvifCameraConfig = { ...(nextCam.onvif || {}) };
    delete nextOnvif.clear_password;
    if (passResolved.action === "clear") {
      nextOnvif.password = null;
      nextOnvif.has_password = false;
    } else if (passResolved.action === "replace") {
      nextOnvif.password = "***";
      nextOnvif.has_password = true;
    } else {
      nextOnvif.password = passResolved.hasExisting ? "***" : null;
      nextOnvif.has_password = passResolved.hasExisting;
    }
    nextCam.onvif = nextOnvif;
  }

  const patchTouchedCloudPassword =
    hasExplicitProperty(hwPatch, "cloud_password") ||
    hasExplicitProperty(hwPatch, "clear_cloud_password") ||
    hasExplicitProperty(onvifPatch, "cloud_password") ||
    hasExplicitProperty(onvifPatch, "clear_cloud_password");

  const patchTouchedMirroredFields =
    hasExplicitProperty(hwPatch, "protocol") ||
    hasExplicitProperty(hwPatch, "profile_token") ||
    hasExplicitProperty(hwPatch, "video_source_token") ||
    hasExplicitProperty(onvifPatch, "protocol") ||
    hasExplicitProperty(onvifPatch, "profile_token") ||
    hasExplicitProperty(onvifPatch, "video_source_token");

  const shouldScrubCloudPassword = onlyPatchedSecrets
    ? patchTouchedCloudPassword
    : patchTouchedCloudPassword ||
      (!prevCam?.hardware && !prevCam?.onvif && Boolean(hwPatch));

  if (shouldScrubCloudPassword || patchTouchedMirroredFields) {
    const mirroredString = (
      field: "protocol" | "profile_token" | "video_source_token"
    ): string | null | undefined => {
      const hasHw = hasExplicitProperty(hwPatch, field);
      const hasOnv = hasExplicitProperty(onvifPatch, field);
      const chosen = hasHw
        ? hwPatch![field]
        : hasOnv
        ? onvifPatch![field]
        : nextCam.hardware?.[field] ?? nextCam.onvif?.[field];
      if (chosen === null) return null;
      if (typeof chosen === "string") {
        const trimmed = chosen.trim();
        return trimmed.length > 0 ? trimmed : null;
      }
      return undefined;
    };

    const resolvedProto = mirroredString("protocol");
    const resolvedProfile = mirroredString("profile_token");
    const resolvedVs = mirroredString("video_source_token");

    const nextHw: CameraHardwareConfig = { ...(nextCam.hardware || {}) };
    if (resolvedProto !== undefined) nextHw.protocol = resolvedProto;
    if (resolvedProfile !== undefined) nextHw.profile_token = resolvedProfile;
    if (resolvedVs !== undefined) nextHw.video_source_token = resolvedVs;

    if (shouldScrubCloudPassword) {
      const prevHasCloud = Boolean(
        prevCam?.hardware?.has_cloud_password ||
          prevCam?.onvif?.has_cloud_password ||
          (prevCam?.hardware?.cloud_password &&
            prevCam.hardware.cloud_password.trim().length > 0) ||
          (prevCam?.onvif?.cloud_password &&
            prevCam.onvif.cloud_password.trim().length > 0)
      );
      const clearCloud = Boolean(
        hwPatch?.clear_cloud_password || onvifPatch?.clear_cloud_password
      );
      const rawCloudCandidate =
        typeof hwPatch?.cloud_password === "string" &&
        !isSecretMaskSentinel(hwPatch.cloud_password) &&
        hwPatch.cloud_password.trim().length > 0
          ? hwPatch.cloud_password
          : typeof onvifPatch?.cloud_password === "string" &&
            !isSecretMaskSentinel(onvifPatch.cloud_password) &&
            onvifPatch.cloud_password.trim().length > 0
          ? onvifPatch.cloud_password
          : typeof hwPatch?.cloud_password === "string"
          ? hwPatch.cloud_password
          : typeof onvifPatch?.cloud_password === "string"
          ? onvifPatch.cloud_password
          : nextCam.hardware?.cloud_password ?? nextCam.onvif?.cloud_password;

      const cloudResolved = resolveSecretFieldAction({
        rawInput: rawCloudCandidate,
        hasExistingSecret: prevHasCloud,
        clearRequested: clearCloud,
      });
      const hasCloudAfter =
        cloudResolved.action === "replace" ||
        (cloudResolved.action === "preserve" && cloudResolved.hasExisting);

      delete nextHw.clear_cloud_password;
      nextHw.cloud_password = hasCloudAfter ? "***" : null;
      nextHw.has_cloud_password = hasCloudAfter;

      if (nextCam.onvif) {
        const nextOnvif: OnvifCameraConfig = { ...nextCam.onvif };
        delete nextOnvif.clear_cloud_password;
        nextOnvif.cloud_password = hasCloudAfter ? "***" : null;
        nextOnvif.has_cloud_password = hasCloudAfter;
        if (resolvedProto !== undefined) nextOnvif.protocol = resolvedProto;
        if (resolvedProfile !== undefined) {
          nextOnvif.profile_token = resolvedProfile;
        }
        if (resolvedVs !== undefined) {
          nextOnvif.video_source_token = resolvedVs;
        }
        nextCam.onvif = nextOnvif;
      }
    } else if (nextCam.onvif) {
      const nextOnvif: OnvifCameraConfig = { ...nextCam.onvif };
      if (resolvedProto !== undefined) nextOnvif.protocol = resolvedProto;
      if (resolvedProfile !== undefined) {
        nextOnvif.profile_token = resolvedProfile;
      }
      if (resolvedVs !== undefined) {
        nextOnvif.video_source_token = resolvedVs;
      }
      nextCam.onvif = nextOnvif;
    }

    nextCam.hardware = nextHw;
  }

  return nextCam;
}

/**
 * Applies a partial structured configuration patch onto `base` with the same
 * map-replacement and secret-mask semantics as `NinjaConfig::apply_structured_json`.
 */
export function mergeConfigStructuredPatch(
  base: NinjaConfigResponse | null,
  patch: Record<string, unknown>,
  options?: {
    scrubSecretsForClient?: boolean;
    onlyScrubPatchedSecrets?: boolean;
    replaceCamerasKeyset?: boolean;
  }
): NinjaConfigResponse {
  const scrubSecrets = options?.scrubSecretsForClient ?? false;
  const onlyScrubPatched = options?.onlyScrubPatchedSecrets ?? false;
  const replaceCamerasKeyset = options?.replaceCamerasKeyset ?? false;
  const seed: Record<string, unknown> = base
    ? (cloneSafe(base) as unknown as Record<string, unknown>)
    : { version: "0.18-0", safe_mode: false, cameras: {} };

  for (const [k, v] of Object.entries(patch)) {
    if (UNSAFE_OBJECT_KEYS.has(k) || v === undefined) continue;
    if (k === "cameras" && isPlainObject(v)) {
      const baseCams = isPlainObject(seed.cameras)
        ? (seed.cameras as Record<string, CameraConfig>)
        : {};
      const nextCams: Record<string, CameraConfig> = replaceCamerasKeyset
        ? {}
        : { ...cloneSafe(baseCams) };
      for (const [camName, camPatch] of Object.entries(v)) {
        if (UNSAFE_OBJECT_KEYS.has(camName)) continue;
        if (camPatch === null) {
          if (!replaceCamerasKeyset) {
            delete nextCams[camName];
          }
          continue;
        }
        if (!isPlainObject(camPatch)) continue;
        const prevCam = baseCams[camName];
        const mergedCamObj = prevCam
          ? (mergeJsonTree(
              prevCam as unknown as Record<string, unknown>,
              camPatch,
              CAMERA_REPLACE_MAP_KEYS
            ) as unknown as CameraConfig)
          : (cloneSafe(camPatch) as unknown as CameraConfig);
        nextCams[camName] = scrubSecrets
          ? normalizeScrubbedCameraSecrets(
              prevCam,
              mergedCamObj,
              camPatch,
              onlyScrubPatched
            )
          : mergedCamObj;
      }
      seed.cameras = nextCams;
      continue;
    }

    if (k === "floorplan" && isPlainObject(v)) {
      const prevUi = isPlainObject(seed.ui)
        ? (seed.ui as Record<string, unknown>)
        : {};
      const prevFp = isPlainObject(prevUi.floorplan)
        ? (prevUi.floorplan as Record<string, unknown>)
        : {};
      seed.ui = {
        ...prevUi,
        floorplan: mergeJsonTree(prevFp, v, NESTED_REPLACE_MAP_KEYS),
      };
      continue;
    }

    if (TOP_LEVEL_REPLACE_MAP_KEYS.has(k)) {
      seed[k] = cloneSafe(v);
      continue;
    }

    if (
      isPlainObject(v) &&
      (isPlainObject(seed[k]) || (scrubSecrets && (k === "mqtt" || k === "genai")))
    ) {
      const prevSection = isPlainObject(seed[k])
        ? (seed[k] as Record<string, unknown>)
        : {};
      const mergedSection = mergeJsonTree(
        prevSection,
        v,
        NESTED_REPLACE_MAP_KEYS
      );
      if (scrubSecrets && k === "mqtt") {
        seed.mqtt = normalizeScrubbedMqttSecret(
          base?.mqtt,
          mergedSection as unknown as MqttConfig,
          v,
          onlyScrubPatched
        );
      } else if (scrubSecrets && k === "genai") {
        seed.genai = normalizeScrubbedGenAiSecret(
          base?.genai,
          mergedSection as unknown as GenAiConfig,
          v,
          onlyScrubPatched
        );
      } else {
        seed[k] = mergedSection;
      }
    } else {
      seed[k] = cloneSafe(v);
    }
  }

  return seed as unknown as NinjaConfigResponse;
}

/**
 * Three-way rebase of an active dirty `currentDraft` when `baselineConfig`
 * advances from `prevBaseline` to `nextBaseline`:
 * - Subtrees untouched on the server (`prev === next`) keep `currentDraft`'s dirty edits.
 * - Subtrees untouched by the operator (`prev === draft`) adopt `nextBaseline`.
 * - Distinct sibling keys inside objects merge cleanly without wiping dirty edits.
 */
export function rebaseDirtyConfigDraft(
  prevBaseline: NinjaConfigResponse | null,
  currentDraft: NinjaConfigResponse | null,
  nextBaseline: NinjaConfigResponse
): NinjaConfigResponse {
  if (!currentDraft) {
    return cloneSafe(nextBaseline);
  }
  if (!prevBaseline) {
    return cloneSafe(currentDraft);
  }

  const rebaseValue = (
    prevVal: unknown,
    draftVal: unknown,
    nextVal: unknown
  ): unknown => {
    if (deepEqualConfigValue(prevVal, nextVal)) {
      return cloneSafe(draftVal);
    }
    if (deepEqualConfigValue(prevVal, draftVal)) {
      return cloneSafe(nextVal);
    }
    if (
      isPlainObject(prevVal) &&
      isPlainObject(draftVal) &&
      isPlainObject(nextVal)
    ) {
      const out: Record<string, unknown> = {};
      const allKeys = new Set<string>([
        ...Object.keys(prevVal),
        ...Object.keys(draftVal),
        ...Object.keys(nextVal),
      ]);
      for (const k of allKeys) {
        if (UNSAFE_OBJECT_KEYS.has(k)) continue;
        const inPrev =
          Object.prototype.hasOwnProperty.call(prevVal, k) &&
          prevVal[k] !== undefined;
        const inDraft =
          Object.prototype.hasOwnProperty.call(draftVal, k) &&
          draftVal[k] !== undefined;
        const inNext =
          Object.prototype.hasOwnProperty.call(nextVal, k) &&
          nextVal[k] !== undefined;

        if (!inDraft && inPrev && deepEqualConfigValue(prevVal[k], nextVal[k])) {
          // Operator deleted key `k`, server did not change it -> keep deleted
          continue;
        }
        if (inPrev && inDraft && !inNext && deepEqualConfigValue(prevVal[k], draftVal[k])) {
          // Server deleted key `k`, operator did not touch it -> adopt deletion
          continue;
        }
        if (!inDraft && inNext && !inPrev) {
          // Server added new key `k`
          out[k] = cloneSafe(nextVal[k]);
          continue;
        }
        if (inDraft && !inNext && !inPrev) {
          // Operator added new key `k`
          out[k] = cloneSafe(draftVal[k]);
          continue;
        }
        const rebasedChild = rebaseValue(prevVal[k], draftVal[k], nextVal[k]);
        if (rebasedChild !== undefined) {
          out[k] = rebasedChild;
        }
      }
      return out;
    }
    // Both operator and server modified the exact same scalar/array leaf -> keep operator's dirty value
    return cloneSafe(draftVal);
  };

  return rebaseValue(
    prevBaseline,
    currentDraft,
    nextBaseline
  ) as NinjaConfigResponse;
}

export interface ConfigTransportCommitResult {
  ok: boolean;
  status: number;
  config?: NinjaConfigResponse;
  rawYaml?: string;
  message?: string;
}

export interface ConfigTransportPort {
  fetchStructuredConfig(): Promise<NinjaConfigResponse>;
  fetchRawYaml(): Promise<string>;
  commitStructured(
    draft: NinjaConfigResponse
  ): Promise<ConfigTransportCommitResult>;
  commitStructuredPatch(
    patch: Record<string, unknown>
  ): Promise<ConfigTransportCommitResult>;
  commitRawYaml(
    yaml: string,
    restart: boolean
  ): Promise<ConfigTransportCommitResult>;
  triggerRestart(): Promise<boolean>;
}

function parseServerCommitPayload(
  status: number,
  okHttp: boolean,
  data: unknown
): ConfigTransportCommitResult {
  const obj = isPlainObject(data) ? data : {};
  const ok = Boolean(okHttp && obj.success !== false);
  const config = isPlainObject(obj.config)
    ? (obj.config as unknown as NinjaConfigResponse)
    : undefined;
  const rawYaml =
    typeof obj.raw_yaml === "string"
      ? obj.raw_yaml
      : typeof obj.rawYaml === "string"
      ? obj.rawYaml
      : undefined;
  const message =
    typeof obj.message === "string"
      ? obj.message
      : typeof obj.error === "string"
      ? obj.error
      : undefined;
  return {
    ok,
    status,
    ...(config ? { config } : {}),
    ...(rawYaml !== undefined ? { rawYaml } : {}),
    ...(message !== undefined ? { message } : {}),
  };
}

/**
 * Production HTTP adapter implementing `ConfigTransportPort`.
 */
export function createFetchConfigTransport(
  fetchImpl?: typeof fetch
): ConfigTransportPort {
  const doFetch: typeof fetch =
    fetchImpl ?? ((input, init) => fetch(input, init));

  return {
    async fetchStructuredConfig(): Promise<NinjaConfigResponse> {
      const res = await doFetch("/api/config", { credentials: "include" });
      if (!res.ok) {
        throw new Error(`Failed to fetch /api/config (HTTP ${res.status})`);
      }
      return (await res.json()) as NinjaConfigResponse;
    },

    async fetchRawYaml(): Promise<string> {
      const res = await doFetch("/api/config/raw", { credentials: "include" });
      if (!res.ok) {
        return "";
      }
      const text = await res.text();
      try {
        const parsed = JSON.parse(text);
        return typeof parsed === "string" ? parsed : text;
      } catch {
        return text;
      }
    },

    async commitStructured(
      draft: NinjaConfigResponse
    ): Promise<ConfigTransportCommitResult> {
      const res = await doFetch("/api/config/structured", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(draft),
      });
      const data = await res.json().catch(() => ({}));
      return parseServerCommitPayload(res.status, res.ok, data);
    },

    async commitStructuredPatch(
      patch: Record<string, unknown>
    ): Promise<ConfigTransportCommitResult> {
      const res = await doFetch("/api/config/structured", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(patch),
      });
      const data = await res.json().catch(() => ({}));
      return parseServerCommitPayload(res.status, res.ok, data);
    },

    async commitRawYaml(
      yaml: string,
      restart: boolean
    ): Promise<ConfigTransportCommitResult> {
      const saveOption = restart ? "restart" : "saveonly";
      const res = await doFetch(
        `/api/config/save?save_option=${encodeURIComponent(saveOption)}`,
        {
          method: "POST",
          headers: { "Content-Type": "text/plain" },
          credentials: "include",
          body: yaml,
        }
      );
      const data = await res.json().catch(() => ({}));
      return parseServerCommitPayload(res.status, res.ok, data);
    },

    async triggerRestart(): Promise<boolean> {
      const res = await doFetch("/api/restart", {
        method: "POST",
        credentials: "include",
      });
      return res.ok;
    },
  };
}

const SENSITIVE_YAML_KEYS = new Set([
  "password",
  "cloud_password",
  "pass",
  "passwd",
  "pwd",
  "api_key",
  "apikey",
  "secret",
  "client_secret",
  "token",
  "access_token",
  "auth_token",
  "webhook_secret",
  "jwt_secret",
]);

export function scrubYamlSecretsForClient(rawYaml: string): string {
  const scrubbedUrls = rawYaml.replace(
    /((?:rtsps?|https?|rtmps?):\/\/)([^\s/@"']+)@([^\s"']+)/gi,
    (_m, scheme: string, userinfo: string, rest: string) => {
      const mask = userinfo.includes(":") ? "*:*" : "*";
      return `${scheme}${mask}@${rest}`;
    }
  );
  return scrubbedUrls
    .split(/\r?\n/)
    .map((line) => {
      const trimmed = line.trimStart();
      if (!trimmed || trimmed.startsWith("#")) return line;
      const indent = line.slice(0, line.length - trimmed.length);
      const listPrefix = trimmed.startsWith("- ") ? "- " : "";
      const afterList = listPrefix ? trimmed.slice(2).trimStart() : trimmed;
      const colonIdx = afterList.indexOf(":");
      if (colonIdx === -1) return line;
      const rawKey = afterList
        .slice(0, colonIdx)
        .trim()
        .replace(/^['"]|['"]$/g, "")
        .toLowerCase();
      if (!SENSITIVE_YAML_KEYS.has(rawKey)) return line;
      const rawVal = afterList.slice(colonIdx + 1).trim();
      const parsedVal = parseYamlScalarValue(rawVal);
      if (
        !parsedVal ||
        parsedVal === "***" ||
        parsedVal === "*" ||
        (!rawVal.startsWith('"') &&
          !rawVal.startsWith("'") &&
          /^(?:null|~)$/i.test(parsedVal))
      ) {
        return line;
      }
      const keySegment = afterList.slice(0, colonIdx);
      return `${indent}${listPrefix}${keySegment}: "***"`;
    })
    .join("\n");
}

function scrubConfigSecretsForClient(
  cfg: NinjaConfigResponse
): NinjaConfigResponse {
  const out = cloneSafe(cfg);
  if (out.mqtt) {
    const clearMqttPass = Boolean(out.mqtt.clear_password);
    const hasPass =
      !clearMqttPass &&
      Boolean(
        out.mqtt.has_password ||
          (out.mqtt.password &&
            out.mqtt.password.trim().length > 0 &&
            out.mqtt.password.trim() !== "null")
      );
    delete out.mqtt.clear_password;
    if (hasPass) {
      out.mqtt.password = "***";
      out.mqtt.has_password = true;
    } else {
      delete out.mqtt.password;
      out.mqtt.has_password = false;
    }
  }
  if (out.genai) {
    const clearApiKey = Boolean(out.genai.clear_api_key);
    const hasKey =
      !clearApiKey &&
      Boolean(
        out.genai.has_api_key ||
          (out.genai.api_key && out.genai.api_key.trim().length > 0)
      );
    delete out.genai.clear_api_key;
    out.genai.api_key = hasKey ? "***" : null;
    out.genai.has_api_key = hasKey;
  }
  for (const cam of Object.values(out.cameras || {})) {
    if (!cam) continue;
    if (cam.ffmpeg?.inputs) {
      cam.ffmpeg.inputs = cam.ffmpeg.inputs.map((inp) => ({
        ...inp,
        path: inp.path.replace(
          /((?:rtsps?|https?|rtmps?):\/\/)([^\s/@"']+)@/gi,
          (_m, scheme: string, userinfo: string) =>
            `${scheme}${userinfo.includes(":") ? "*:*" : "*"}@`
        ),
      }));
    }
    const clearCloud = Boolean(
      cam.hardware?.clear_cloud_password || cam.onvif?.clear_cloud_password
    );
    const hasCloud =
      !clearCloud &&
      Boolean(
        cam.hardware?.has_cloud_password ||
          cam.onvif?.has_cloud_password ||
          (cam.hardware?.cloud_password &&
            cam.hardware.cloud_password.trim().length > 0) ||
          (cam.onvif?.cloud_password &&
            cam.onvif.cloud_password.trim().length > 0)
      );
    const proto = cam.hardware?.protocol ?? cam.onvif?.protocol;
    const profTok = cam.hardware?.profile_token ?? cam.onvif?.profile_token;
    const vsTok =
      cam.hardware?.video_source_token ?? cam.onvif?.video_source_token;
    if (cam.onvif) {
      const clearOnvifPass = Boolean(cam.onvif.clear_password);
      const hasPass =
        !clearOnvifPass &&
        Boolean(
          cam.onvif.has_password ||
            (cam.onvif.password && cam.onvif.password.trim().length > 0)
        );
      delete cam.onvif.clear_password;
      delete cam.onvif.clear_cloud_password;
      cam.onvif.password = hasPass ? "***" : null;
      cam.onvif.has_password = hasPass;
      cam.onvif.cloud_password = hasCloud ? "***" : null;
      cam.onvif.has_cloud_password = hasCloud;
      if (proto !== undefined) cam.onvif.protocol = proto;
      if (profTok !== undefined) cam.onvif.profile_token = profTok;
      if (vsTok !== undefined) cam.onvif.video_source_token = vsTok;
    }
    if (
      cam.hardware ||
      hasCloud ||
      clearCloud ||
      proto !== undefined ||
      profTok !== undefined ||
      vsTok !== undefined
    ) {
      cam.hardware = {
        ...(cam.hardware || {}),
        ...(proto !== undefined ? { protocol: proto } : {}),
        ...(profTok !== undefined ? { profile_token: profTok } : {}),
        ...(vsTok !== undefined ? { video_source_token: vsTok } : {}),
        cloud_password: hasCloud ? "***" : null,
        has_cloud_password: hasCloud,
      };
      delete cam.hardware.clear_cloud_password;
    }
  }
  return out;
}

export interface InMemoryConfigTransportOptions {
  initialConfig?: NinjaConfigResponse;
  initialRawYaml?: string;
  structuredEndpointStatus?: number;
  returnConfigFromRawSave?: boolean;
  beforeCommitStructured?: (
    draft: NinjaConfigResponse
  ) => Promise<ConfigTransportCommitResult | void> | ConfigTransportCommitResult | void;
  beforeCommitStructuredPatch?: (
    patch: Record<string, unknown>
  ) => Promise<ConfigTransportCommitResult | void> | ConfigTransportCommitResult | void;
  beforeCommitRawYaml?: (
    yaml: string,
    restart: boolean
  ) => Promise<ConfigTransportCommitResult | void> | ConfigTransportCommitResult | void;
}

export interface InMemoryConfigTransport extends ConfigTransportPort {
  readonly committedStructuredDrafts: NinjaConfigResponse[];
  readonly committedPatches: Array<Record<string, unknown>>;
  readonly committedRawYamls: Array<{ yaml: string; restart: boolean }>;
  readonly restartCount: number;
  getServerConfig(): NinjaConfigResponse;
  getServerRawYaml(): string;
  getUnscrubbedYaml(): string;
  setServerState(config: NinjaConfigResponse, unscrubbedRawYaml?: string): void;
}

/**
 * Deterministic in-memory `ConfigTransportPort` adapter for fast behavioral unit tests.
 */
export function createInMemoryConfigTransport(
  options: InMemoryConfigTransportOptions = {}
): InMemoryConfigTransport {
  const defaultCfg: NinjaConfigResponse = options.initialConfig
    ? cloneSafe(options.initialConfig)
    : {
        version: "0.18-0",
        safe_mode: false,
        cameras: {},
      };

  let unscrubbedYaml =
    options.initialRawYaml !== undefined
      ? options.initialRawYaml
      : buildCleanYamlFromDraft(defaultCfg, "");
  let serverConfig = scrubConfigSecretsForClient(defaultCfg);

  const committedStructuredDrafts: NinjaConfigResponse[] = [];
  const committedPatches: Array<Record<string, unknown>> = [];
  const committedRawYamls: Array<{ yaml: string; restart: boolean }> = [];
  let restarts = 0;

  return {
    committedStructuredDrafts,
    committedPatches,
    committedRawYamls,
    get restartCount() {
      return restarts;
    },
    getServerConfig() {
      return cloneSafe(serverConfig);
    },
    getServerRawYaml() {
      return scrubYamlSecretsForClient(unscrubbedYaml);
    },
    getUnscrubbedYaml() {
      return unscrubbedYaml;
    },
    setServerState(config: NinjaConfigResponse, nextRawYaml?: string) {
      unscrubbedYaml =
        nextRawYaml !== undefined
          ? nextRawYaml
          : buildCleanYamlFromDraft(config, unscrubbedYaml);
      serverConfig = scrubConfigSecretsForClient(config);
    },

    async fetchStructuredConfig(): Promise<NinjaConfigResponse> {
      return cloneSafe(serverConfig);
    },

    async fetchRawYaml(): Promise<string> {
      return scrubYamlSecretsForClient(unscrubbedYaml);
    },

    async commitStructured(
      draft: NinjaConfigResponse
    ): Promise<ConfigTransportCommitResult> {
      if (options.beforeCommitStructured) {
        const custom = await options.beforeCommitStructured(cloneSafe(draft));
        if (custom) return custom;
      }
      if (options.structuredEndpointStatus === 404) {
        return { ok: false, status: 404, message: "Not Found" };
      }
      if (
        options.structuredEndpointStatus &&
        options.structuredEndpointStatus >= 400
      ) {
        return {
          ok: false,
          status: options.structuredEndpointStatus,
          message: `HTTP ${options.structuredEndpointStatus}`,
        };
      }

      if (
        Object.prototype.hasOwnProperty.call(draft, "cameras") &&
        draft.cameras !== undefined
      ) {
        if (!isPlainObject(draft.cameras)) {
          return {
            ok: false,
            status: 400,
            message: "invalid cameras config: expected object",
          };
        }
        for (const [camName, camEntry] of Object.entries(draft.cameras)) {
          if (UNSAFE_OBJECT_KEYS.has(camName)) continue;
          if (!isPlainObject(camEntry)) {
            return {
              ok: false,
              status: 400,
              message: `invalid camera config for '${camName}'`,
            };
          }
        }
      }

      committedStructuredDrafts.push(cloneSafe(draft));

      // Apply full structured commit: cameras map replaces keyset with exact keys in draft,
      // while preserving scrubbed secrets from previous serverConfig / unscrubbedYaml.
      const patchFromDraft: Record<string, unknown> = {
        ...(cloneSafe(draft) as unknown as Record<string, unknown>),
      };
      unscrubbedYaml = buildCleanYamlFromDraft(draft, unscrubbedYaml);
      serverConfig = mergeConfigStructuredPatch(serverConfig, patchFromDraft, {
        scrubSecretsForClient: true,
        replaceCamerasKeyset: true,
      });
      const scrubbedRawYaml = scrubYamlSecretsForClient(unscrubbedYaml);
      return {
        ok: true,
        status: 200,
        config: cloneSafe(serverConfig),
        rawYaml: scrubbedRawYaml,
        message: "Structured configuration saved",
      };
    },

    async commitStructuredPatch(
      patch: Record<string, unknown>
    ): Promise<ConfigTransportCommitResult> {
      if (options.beforeCommitStructuredPatch) {
        const custom = await options.beforeCommitStructuredPatch(
          cloneSafe(patch)
        );
        if (custom) return custom;
      }
      if (options.structuredEndpointStatus === 404) {
        return { ok: false, status: 404, message: "Not Found" };
      }
      if (
        options.structuredEndpointStatus &&
        options.structuredEndpointStatus >= 400
      ) {
        return {
          ok: false,
          status: options.structuredEndpointStatus,
          message: `HTTP ${options.structuredEndpointStatus}`,
        };
      }

      if (
        Object.prototype.hasOwnProperty.call(patch, "cameras") &&
        patch.cameras !== undefined
      ) {
        if (!isPlainObject(patch.cameras)) {
          return {
            ok: false,
            status: 400,
            message: "invalid cameras config: expected object",
          };
        }
        for (const [camName, camEntry] of Object.entries(patch.cameras)) {
          if (UNSAFE_OBJECT_KEYS.has(camName)) continue;
          if (!isPlainObject(camEntry)) {
            return {
              ok: false,
              status: 400,
              message: `invalid camera config for '${camName}'`,
            };
          }
        }
      }

      committedPatches.push(cloneSafe(patch));
      const mergedUnscrubbed = mergeConfigStructuredPatch(serverConfig, patch, {
        scrubSecretsForClient: false,
        replaceCamerasKeyset: true,
      });
      unscrubbedYaml = buildCleanYamlFromDraft(
        mergedUnscrubbed,
        unscrubbedYaml
      );
      serverConfig = mergeConfigStructuredPatch(serverConfig, patch, {
        scrubSecretsForClient: true,
        replaceCamerasKeyset: true,
      });
      const scrubbedRawYaml = scrubYamlSecretsForClient(unscrubbedYaml);
      return {
        ok: true,
        status: 200,
        config: cloneSafe(serverConfig),
        rawYaml: scrubbedRawYaml,
        message: "Structured configuration saved",
      };
    },

    async commitRawYaml(
      yaml: string,
      restart: boolean
    ): Promise<ConfigTransportCommitResult> {
      if (options.beforeCommitRawYaml) {
        const custom = await options.beforeCommitRawYaml(yaml, restart);
        if (custom) return custom;
      }
      committedRawYamls.push({ yaml, restart });
      unscrubbedYaml = yaml;
      const scrubbedRawYaml = scrubYamlSecretsForClient(unscrubbedYaml);
      if (options.returnConfigFromRawSave === false) {
        return {
          ok: true,
          status: 200,
          message: "Config saved successfully",
        };
      }
      return {
        ok: true,
        status: 200,
        config: cloneSafe(serverConfig),
        rawYaml: scrubbedRawYaml,
        message: "Config saved successfully",
      };
    },

    async triggerRestart(): Promise<boolean> {
      restarts += 1;
      return true;
    },
  };
}

export interface ConfigWorkbenchSaveOutcome {
  ok: boolean;
  message: string;
  at: number;
}

export interface ConfigWorkbenchSnapshot {
  baselineConfig: NinjaConfigResponse | null;
  draftConfig: NinjaConfigResponse | null;
  baselineYaml: string;
  rawYaml: string;
  isDirty: boolean;
  hasUnsavedRawEdits: boolean;
  isSaving: boolean;
  revision: number;
  yamlStale: boolean;
  saveOutcome: ConfigWorkbenchSaveOutcome | null;
}

export interface CreateConfigWorkbenchSessionOptions {
  initialConfig?: NinjaConfigResponse | null;
  initialRawYaml?: string;
  onServerCommitted?: (
    config: NinjaConfigResponse | null,
    rawYaml: string
  ) => void;
}

export interface MutateDraftOptions {
  syncYamlImmediately?: boolean;
}

export interface ApplyOutOfBandPatchOptions {
  /**
   * When false, merges an already-persisted server patch into `baselineConfig`
   * and `draftConfig` without issuing a second `commitStructuredPatch` HTTP call.
   * Defaults to true.
   */
  persistViaTransport?: boolean;
  syncYamlImmediately?: boolean;
}

export interface CommitStructuredOptions {
  restart?: boolean;
  discardRawEdits?: boolean;
  rawYamlConflictMessage?: string;
  savedMessage?: string;
  restartedMessage?: string;
  failedMessage?: string;
  networkErrorMessage?: string;
}

export interface CommitRawOptions {
  yaml?: string;
  restart?: boolean;
  savedMessage?: string;
  restartedMessage?: string;
  failedMessage?: string;
  networkErrorMessage?: string;
}

export interface WorkbenchCommitOutcome {
  ok: boolean;
  status: number;
  config: NinjaConfigResponse | null;
  rawYaml: string;
  message: string;
  rejectedGeometryCount: number;
}

export interface ConfigWorkbenchSession {
  readonly transport: ConfigTransportPort;
  getSnapshot(): ConfigWorkbenchSnapshot;
  subscribe(listener: () => void): () => void;
  hydrateFromServer(config: NinjaConfigResponse | null, rawYaml?: string): void;
  hydrateRawYamlFromServer(rawYaml: string): void;
  refreshFromServer(): Promise<{
    config: NinjaConfigResponse | null;
    rawYaml: string;
  }>;
  mutateDraft(
    updater: (next: NinjaConfigResponse) => void,
    options?: MutateDraftOptions
  ): StructuredDraftMutationResult | null;
  setRawYamlDraft(yaml: string): void;
  ensureDraftYamlSynced(): string;
  applyExternalPatch(
    patch: Record<string, unknown>,
    options?: {
      serverConfig?: NinjaConfigResponse;
      serverRawYaml?: string;
      syncYamlImmediately?: boolean;
    }
  ): void;
  applyOutOfBandPatch(
    patch: Record<string, unknown>,
    options?: ApplyOutOfBandPatchOptions
  ): Promise<WorkbenchCommitOutcome>;
  commitStructured(
    options?: CommitStructuredOptions
  ): Promise<WorkbenchCommitOutcome>;
  commitRaw(options?: CommitRawOptions): Promise<WorkbenchCommitOutcome>;
  discardRawEdits(): void;
  discardDraft(): void;
}

/**
 * Creates an isolated `ConfigWorkbenchSession` coordinating structured drafts,
 * raw YAML editing, out-of-band patch rebasing, and revision-fenced saves.
 */
export function createConfigWorkbenchSession(
  transport: ConfigTransportPort,
  options?: CreateConfigWorkbenchSessionOptions
): ConfigWorkbenchSession {
  const initialCfg = options?.initialConfig
    ? cloneSafe(options.initialConfig)
    : null;
  const initialYaml = options?.initialRawYaml ?? "";

  let baselineConfig: NinjaConfigResponse | null = initialCfg
    ? cloneSafe(initialCfg)
    : null;
  let draftConfig: NinjaConfigResponse | null = initialCfg
    ? cloneSafe(initialCfg)
    : null;
  let baselineYaml = initialYaml;
  let rawYaml = initialYaml;
  let isDirty = false;
  let isSaving = false;
  let revision = 0;
  let yamlStale = false;
  let saveOutcome: ConfigWorkbenchSaveOutcome | null = null;
  let hasUnsavedRawEdits = false;
  let sanitizedDraftCache: NinjaConfigResponse | null = null;

  const listeners = new Set<() => void>();

  let cachedSnapshot: ConfigWorkbenchSnapshot = {
    baselineConfig,
    draftConfig,
    baselineYaml,
    rawYaml,
    isDirty,
    hasUnsavedRawEdits,
    isSaving,
    revision,
    yamlStale,
    saveOutcome,
  };

  const publish = () => {
    cachedSnapshot = {
      baselineConfig,
      draftConfig,
      baselineYaml,
      rawYaml,
      isDirty,
      hasUnsavedRawEdits,
      isSaving,
      revision,
      yamlStale,
      saveOutcome,
    };
    for (const listener of listeners) {
      try {
        listener();
      } catch {
        // isolate subscriber errors
      }
    }
  };

  const notifyServerCommitted = () => {
    options?.onServerCommitted?.(
      baselineConfig ? cloneSafe(baselineConfig) : null,
      baselineYaml
    );
  };

  const hydrateFromServer = (
    config: NinjaConfigResponse | null,
    serverRawYaml?: string
  ): void => {
    if (config === null) {
      const hadState =
        baselineConfig !== null ||
        draftConfig !== null ||
        baselineYaml !== "" ||
        rawYaml !== "" ||
        isDirty ||
        isSaving;
      baselineConfig = null;
      draftConfig = null;
      baselineYaml = serverRawYaml ?? "";
      rawYaml = serverRawYaml ?? "";
      isDirty = false;
      isSaving = false;
      yamlStale = false;
      hasUnsavedRawEdits = false;
      saveOutcome = null;
      sanitizedDraftCache = null;
      if (hadState) publish();
      return;
    }

    const prevBaseline = baselineConfig;
    const nextBaseline = cloneSafe(config);
    const baselineChanged = !deepEqualConfigValue(prevBaseline, nextBaseline);
    const yamlProvided =
      typeof serverRawYaml === "string" && serverRawYaml.length > 0;
    const yamlChanged = yamlProvided && serverRawYaml !== baselineYaml;

    if (!baselineChanged && !yamlChanged && draftConfig !== null) {
      if (!isDirty && !isSaving && rawYaml === "" && baselineYaml !== "") {
        rawYaml = baselineYaml;
        publish();
      }
      return;
    }

    baselineConfig = nextBaseline;
    if (yamlProvided) {
      baselineYaml = serverRawYaml!;
    }

    if (!isDirty && !isSaving) {
      draftConfig = cloneSafe(nextBaseline);
      sanitizedDraftCache = null;
      hasUnsavedRawEdits = false;
      if (yamlProvided) {
        rawYaml = serverRawYaml!;
        yamlStale = false;
      } else if (rawYaml === "" && baselineYaml !== "") {
        rawYaml = baselineYaml;
        yamlStale = false;
      } else if (baselineChanged && baselineYaml !== "") {
        yamlStale = true;
      }
    } else if (baselineChanged) {
      draftConfig = rebaseDirtyConfigDraft(
        prevBaseline,
        draftConfig,
        nextBaseline
      );
      sanitizedDraftCache = null;
      if (!hasUnsavedRawEdits) {
        yamlStale = true;
      }
    }

    publish();
  };

  const hydrateRawYamlFromServer = (serverRawYaml: string): void => {
    if (!serverRawYaml) return;
    const baselineChanged = baselineYaml !== serverRawYaml;
    const shouldSyncRaw =
      !isDirty && !isSaving && (rawYaml !== serverRawYaml || yamlStale);
    if (!baselineChanged && !shouldSyncRaw) return;
    baselineYaml = serverRawYaml;
    if (!isDirty && !isSaving) {
      rawYaml = serverRawYaml;
      yamlStale = false;
    }
    publish();
  };

  const refreshFromServer = async (): Promise<{
    config: NinjaConfigResponse | null;
    rawYaml: string;
  }> => {
    const [freshCfg, freshYaml] = await Promise.all([
      transport.fetchStructuredConfig().catch(() => null),
      transport.fetchRawYaml().catch(() => ""),
    ]);
    if (freshCfg) {
      hydrateFromServer(freshCfg, freshYaml || undefined);
    } else if (freshYaml) {
      hydrateRawYamlFromServer(freshYaml);
    }
    return {
      config: baselineConfig ? cloneSafe(baselineConfig) : null,
      rawYaml: baselineYaml,
    };
  };

  const mutateDraft = (
    updater: (next: NinjaConfigResponse) => void,
    mutateOptions?: MutateDraftOptions
  ): StructuredDraftMutationResult | null => {
    if (!draftConfig) return null;
    const mutation = applyStructuredDraftMutation(draftConfig, updater, {
      syncYamlImmediately: Boolean(mutateOptions?.syncYamlImmediately),
      baselineYaml: baselineYaml || rawYaml,
    });
    draftConfig = mutation.draft;
    sanitizedDraftCache = mutation.draft;
    revision += 1;
    yamlStale = mutation.yamlStale;
    if (mutation.rawYaml !== null) {
      rawYaml = mutation.rawYaml;
      hasUnsavedRawEdits = false;
    }
    isDirty = true;
    publish();
    return mutation;
  };

  const setRawYamlDraft = (nextYaml: string): void => {
    revision += 1;
    rawYaml = nextYaml;
    yamlStale = false;
    hasUnsavedRawEdits = true;
    isDirty = true;
    publish();
  };

  const ensureDraftYamlSynced = (): string => {
    if (!draftConfig || !yamlStale) {
      return rawYaml;
    }
    const synced = synthesizeDraftYamlIfNeeded(
      draftConfig,
      baselineYaml,
      rawYaml,
      yamlStale
    );
    rawYaml = synced.rawYaml;
    yamlStale = synced.yamlStale;
    publish();
    return rawYaml;
  };

  const applyExternalPatch = (
    patch: Record<string, unknown>,
    extOptions?: {
      serverConfig?: NinjaConfigResponse;
      serverRawYaml?: string;
      syncYamlImmediately?: boolean;
    }
  ): void => {
    const prevBaseline = baselineConfig;
    const nextBaseline = extOptions?.serverConfig
      ? cloneSafe(extOptions.serverConfig)
      : mergeConfigStructuredPatch(prevBaseline, patch, {
          scrubSecretsForClient: true,
        });
    baselineConfig = nextBaseline;

    if (!isDirty && !isSaving) {
      draftConfig = cloneSafe(nextBaseline);
    } else {
      const rebased = rebaseDirtyConfigDraft(
        prevBaseline,
        draftConfig,
        nextBaseline
      );
      draftConfig = mergeConfigStructuredPatch(rebased, patch, {
        scrubSecretsForClient: true,
        onlyScrubPatchedSecrets: true,
      });
    }
    sanitizedDraftCache = null;

    if (
      typeof extOptions?.serverRawYaml === "string" &&
      extOptions.serverRawYaml.trim()
    ) {
      baselineYaml = extOptions.serverRawYaml;
      if (!isDirty && !isSaving) {
        rawYaml = extOptions.serverRawYaml;
        yamlStale = false;
      } else if (extOptions.syncYamlImmediately && draftConfig) {
        rawYaml = buildCleanYamlFromDraft(draftConfig, baselineYaml);
        yamlStale = false;
      } else if (!hasUnsavedRawEdits) {
        yamlStale = true;
      }
    } else if (extOptions?.syncYamlImmediately && draftConfig) {
      rawYaml = buildCleanYamlFromDraft(draftConfig, baselineYaml || rawYaml);
      yamlStale = false;
    } else if (!hasUnsavedRawEdits) {
      yamlStale = true;
    }

    if (
      isDirty &&
      !hasUnsavedRawEdits &&
      draftConfig &&
      deepEqualConfigValue(baselineConfig, draftConfig)
    ) {
      isDirty = false;
    }

    notifyServerCommitted();
    publish();
  };

  const applyOutOfBandPatch = async (
    patch: Record<string, unknown>,
    oobOptions?: ApplyOutOfBandPatchOptions
  ): Promise<WorkbenchCommitOutcome> => {
    const persistViaTransport = oobOptions?.persistViaTransport ?? true;
    if (!persistViaTransport) {
      applyExternalPatch(patch, {
        syncYamlImmediately: oobOptions?.syncYamlImmediately,
      });
      return {
        ok: true,
        status: 200,
        config: baselineConfig ? cloneSafe(baselineConfig) : null,
        rawYaml: baselineYaml,
        message: "Applied external configuration update",
        rejectedGeometryCount: 0,
      };
    }

    const startRevision = revision;
    const wasDirty = isDirty;
    const prePatchDraft = draftConfig ? cloneSafe(draftConfig) : null;
    const prevBaseline = baselineConfig ? cloneSafe(baselineConfig) : null;

    let transportPatch: Record<string, unknown> = patch;
    if (isPlainObject(patch.cameras)) {
      const patchCams = patch.cameras;
      const expandedCameras: Record<string, unknown> = {};
      if (isPlainObject(prevBaseline?.cameras)) {
        for (const camKey of Object.keys(prevBaseline.cameras)) {
          if (UNSAFE_OBJECT_KEYS.has(camKey)) continue;
          if (
            Object.prototype.hasOwnProperty.call(patchCams, camKey) &&
            patchCams[camKey] === null
          ) {
            continue;
          }
          expandedCameras[camKey] = {};
        }
      }
      for (const [camKey, camVal] of Object.entries(patchCams)) {
        if (UNSAFE_OBJECT_KEYS.has(camKey)) continue;
        if (camVal === null) {
          delete expandedCameras[camKey];
          continue;
        }
        if (camVal === undefined) continue;
        expandedCameras[camKey] = cloneSafe(camVal);
      }
      transportPatch = {
        ...cloneSafe(patch),
        cameras: expandedCameras,
      };
    }

    // Optimistic draft update so controlled toggles/inputs reflect immediately
    if (draftConfig) {
      draftConfig = mergeConfigStructuredPatch(draftConfig, patch, {
        scrubSecretsForClient: false,
      });
      sanitizedDraftCache = null;
      publish();
    }

    try {
      const res = await transport.commitStructuredPatch(transportPatch);
      if (res.ok) {
        const nextBaseline = res.config
          ? cloneSafe(res.config)
          : mergeConfigStructuredPatch(prevBaseline, transportPatch, {
              scrubSecretsForClient: true,
              replaceCamerasKeyset: true,
            });
        baselineConfig = nextBaseline;

        const concurrentOperatorEdit = revision !== startRevision;
        if (!wasDirty && !concurrentOperatorEdit) {
          draftConfig = cloneSafe(nextBaseline);
          isDirty = false;
          hasUnsavedRawEdits = false;
        } else {
          const currentActiveDraft = concurrentOperatorEdit
            ? draftConfig
            : prePatchDraft;
          const rebased = rebaseDirtyConfigDraft(
            prevBaseline,
            currentActiveDraft,
            nextBaseline
          );
          draftConfig = mergeConfigStructuredPatch(rebased, patch, {
            scrubSecretsForClient: true,
            onlyScrubPatchedSecrets: true,
          });
          isDirty =
            hasUnsavedRawEdits ||
            !deepEqualConfigValue(baselineConfig, draftConfig);
        }
        sanitizedDraftCache = null;

        if (typeof res.rawYaml === "string" && res.rawYaml.trim()) {
          baselineYaml = res.rawYaml;
          if (!isDirty) {
            rawYaml = res.rawYaml;
            yamlStale = false;
          } else if (oobOptions?.syncYamlImmediately && draftConfig) {
            rawYaml = buildCleanYamlFromDraft(draftConfig, baselineYaml);
            yamlStale = false;
          } else if (!hasUnsavedRawEdits) {
            yamlStale = true;
          }
        } else if (oobOptions?.syncYamlImmediately && draftConfig) {
          rawYaml = buildCleanYamlFromDraft(
            draftConfig,
            baselineYaml || rawYaml
          );
          yamlStale = false;
        } else if (!hasUnsavedRawEdits) {
          yamlStale = true;
        }

        notifyServerCommitted();
        publish();
        return {
          ok: true,
          status: res.status,
          config: cloneSafe(baselineConfig),
          rawYaml: baselineYaml,
          message: res.message || "Structured configuration saved",
          rejectedGeometryCount: 0,
        };
      }

      // Restore pre-optimistic draft if operator made no concurrent edits
      if (revision === startRevision) {
        draftConfig = prePatchDraft;
        isDirty = wasDirty;
      }
      const errMsg =
        res.message || `Failed to save configuration patch (HTTP ${res.status})`;
      saveOutcome = { ok: false, message: errMsg, at: Date.now() };
      publish();
      return {
        ok: false,
        status: res.status,
        config: baselineConfig ? cloneSafe(baselineConfig) : null,
        rawYaml: baselineYaml,
        message: errMsg,
        rejectedGeometryCount: 0,
      };
    } catch {
      if (revision === startRevision) {
        draftConfig = prePatchDraft;
        isDirty = wasDirty;
      }
      const errMsg = "Network error while saving configuration patch.";
      saveOutcome = { ok: false, message: errMsg, at: Date.now() };
      publish();
      return {
        ok: false,
        status: 0,
        config: baselineConfig ? cloneSafe(baselineConfig) : null,
        rawYaml: baselineYaml,
        message: errMsg,
        rejectedGeometryCount: 0,
      };
    }
  };

  const commitStructured = async (
    commitOptions?: CommitStructuredOptions
  ): Promise<WorkbenchCommitOutcome> => {
    const restartAfter = Boolean(commitOptions?.restart);
    if (!draftConfig || isSaving) {
      return {
        ok: false,
        status: 0,
        config: baselineConfig ? cloneSafe(baselineConfig) : null,
        rawYaml: baselineYaml,
        message: "No active configuration draft to save",
        rejectedGeometryCount: 0,
      };
    }

    if (hasUnsavedRawEdits && !commitOptions?.discardRawEdits) {
      const conflictMsg =
        commitOptions?.rawYamlConflictMessage ||
        "Unsaved Raw config.yml edits exist. Save or discard Raw YAML edits before saving visual settings.";
      saveOutcome = { ok: false, message: conflictMsg, at: Date.now() };
      publish();
      return {
        ok: false,
        status: 409,
        config: baselineConfig ? cloneSafe(baselineConfig) : null,
        rawYaml,
        message: conflictMsg,
        rejectedGeometryCount: 0,
      };
    }
    if (hasUnsavedRawEdits && commitOptions?.discardRawEdits) {
      hasUnsavedRawEdits = false;
    }

    const saveRev = revision;
    const { cleaned: payloadConfig, rejectedCount } =
      sanitizedDraftCache === draftConfig
        ? { cleaned: draftConfig, rejectedCount: 0 }
        : sanitizeConfigGeometryDraft(draftConfig);
    draftConfig = payloadConfig;
    sanitizedDraftCache = payloadConfig;
    saveOutcome = null;
    isSaving = true;
    publish();

    try {
      const res = await transport.commitStructured(payloadConfig);

      if (res.status === 404) {
        const generatedYaml = buildCleanYamlFromDraft(
          payloadConfig,
          baselineYaml || rawYaml
        );
        const saveRes = await transport.commitRawYaml(
          generatedYaml,
          restartAfter
        );
        if (saveRes.ok) {
          const nextSavedConfig = saveRes.config
            ? cloneSafe(saveRes.config)
            : scrubConfigSecretsForClient(payloadConfig);
          const nextSavedYaml =
            typeof saveRes.rawYaml === "string" && saveRes.rawYaml.trim()
              ? saveRes.rawYaml
              : generatedYaml;
          baselineConfig = cloneSafe(nextSavedConfig);
          baselineYaml = nextSavedYaml;

          if (revision === saveRev) {
            draftConfig = cloneSafe(nextSavedConfig);
            sanitizedDraftCache = draftConfig;
            rawYaml = nextSavedYaml;
            yamlStale = false;
            hasUnsavedRawEdits = false;
            isDirty = false;
          } else {
            draftConfig = rebaseDirtyConfigDraft(
              payloadConfig,
              draftConfig,
              nextSavedConfig
            );
            sanitizedDraftCache = null;
            isDirty = true;
          }

          if (restartAfter) {
            await transport.triggerRestart().catch(() => false);
          }

          const okMsg = restartAfter
            ? commitOptions?.restartedMessage ||
              "Configuration saved and NVR restart triggered"
            : commitOptions?.savedMessage || "Configuration saved to config.yml";
          saveOutcome = { ok: true, message: okMsg, at: Date.now() };
          notifyServerCommitted();
          return {
            ok: true,
            status: saveRes.status,
            config: cloneSafe(baselineConfig),
            rawYaml: baselineYaml,
            message: okMsg,
            rejectedGeometryCount: rejectedCount,
          };
        }

        const failMsg =
          saveRes.message ||
          commitOptions?.failedMessage ||
          "Failed to save configuration";
        saveOutcome = { ok: false, message: failMsg, at: Date.now() };
        return {
          ok: false,
          status: saveRes.status,
          config: baselineConfig ? cloneSafe(baselineConfig) : null,
          rawYaml: baselineYaml,
          message: failMsg,
          rejectedGeometryCount: rejectedCount,
        };
      }

      if (res.ok) {
        const nextSavedConfig = res.config
          ? cloneSafe(res.config)
          : scrubConfigSecretsForClient(payloadConfig);
        const nextSavedYaml =
          typeof res.rawYaml === "string" && res.rawYaml.trim()
            ? res.rawYaml
            : buildCleanYamlFromDraft(nextSavedConfig, baselineYaml || rawYaml);

        baselineConfig = cloneSafe(nextSavedConfig);
        baselineYaml = nextSavedYaml;

        if (revision === saveRev) {
          draftConfig = cloneSafe(nextSavedConfig);
          sanitizedDraftCache = draftConfig;
          rawYaml = nextSavedYaml;
          yamlStale = false;
          hasUnsavedRawEdits = false;
          isDirty = false;
        } else {
          draftConfig = rebaseDirtyConfigDraft(
            payloadConfig,
            draftConfig,
            nextSavedConfig
          );
          sanitizedDraftCache = null;
          isDirty = true;
        }

        if (restartAfter) {
          await transport.triggerRestart().catch(() => false);
        }

        const okMsg = restartAfter
          ? commitOptions?.restartedMessage ||
            "Structured configuration saved and pipeline restart triggered"
          : commitOptions?.savedMessage ||
            "Structured configuration saved to config.yml";
        saveOutcome = { ok: true, message: okMsg, at: Date.now() };
        notifyServerCommitted();
        return {
          ok: true,
          status: res.status,
          config: cloneSafe(baselineConfig),
          rawYaml: baselineYaml,
          message: okMsg,
          rejectedGeometryCount: rejectedCount,
        };
      }

      const failMsg =
        res.message ||
        commitOptions?.failedMessage ||
        `Failed to save structured configuration (HTTP ${res.status})`;
      saveOutcome = { ok: false, message: failMsg, at: Date.now() };
      return {
        ok: false,
        status: res.status,
        config: baselineConfig ? cloneSafe(baselineConfig) : null,
        rawYaml: baselineYaml,
        message: failMsg,
        rejectedGeometryCount: rejectedCount,
      };
    } catch {
      const netMsg =
        commitOptions?.networkErrorMessage ||
        "Network error while saving. Nothing was written.";
      saveOutcome = { ok: false, message: netMsg, at: Date.now() };
      return {
        ok: false,
        status: 0,
        config: baselineConfig ? cloneSafe(baselineConfig) : null,
        rawYaml: baselineYaml,
        message: netMsg,
        rejectedGeometryCount: rejectedCount,
      };
    } finally {
      isSaving = false;
      publish();
    }
  };

  const commitRaw = async (
    rawOptions?: CommitRawOptions
  ): Promise<WorkbenchCommitOutcome> => {
    if (isSaving) {
      return {
        ok: false,
        status: 0,
        config: baselineConfig ? cloneSafe(baselineConfig) : null,
        rawYaml: baselineYaml,
        message: "Configuration save already in progress",
        rejectedGeometryCount: 0,
      };
    }

    const restartAfter = Boolean(rawOptions?.restart);
    const saveRev = revision;
    let yamlToSave = rawOptions?.yaml ?? rawYaml;
    if (rawOptions?.yaml === undefined && yamlStale && draftConfig) {
      const synced = synthesizeDraftYamlIfNeeded(
        draftConfig,
        baselineYaml,
        rawYaml,
        yamlStale
      );
      yamlToSave = synced.rawYaml;
      rawYaml = synced.rawYaml;
      yamlStale = false;
    }

    saveOutcome = null;
    isSaving = true;
    publish();

    try {
      const res = await transport.commitRawYaml(yamlToSave, restartAfter);
      if (res.ok) {
        const fetchedCfg = res.config
          ? cloneSafe(res.config)
          : await transport.fetchStructuredConfig().catch(() => null);
        const nextSavedYaml =
          typeof res.rawYaml === "string" && res.rawYaml.trim()
            ? res.rawYaml
            : yamlToSave;

        if (fetchedCfg) {
          baselineConfig = cloneSafe(fetchedCfg);
        }
        baselineYaml = nextSavedYaml;

        if (revision === saveRev) {
          if (fetchedCfg) {
            draftConfig = cloneSafe(fetchedCfg);
            sanitizedDraftCache = null;
          }
          rawYaml = nextSavedYaml;
          yamlStale = false;
          hasUnsavedRawEdits = false;
          isDirty = false;
        } else {
          isDirty = true;
        }

        if (restartAfter) {
          await transport.triggerRestart().catch(() => false);
        }

        const okMsg = restartAfter
          ? rawOptions?.restartedMessage ||
            "Configuration saved and NinjaNVR restarted"
          : rawOptions?.savedMessage || "Configuration saved to disk";
        saveOutcome = { ok: true, message: okMsg, at: Date.now() };
        notifyServerCommitted();
        return {
          ok: true,
          status: res.status,
          config: baselineConfig ? cloneSafe(baselineConfig) : null,
          rawYaml: baselineYaml,
          message: okMsg,
          rejectedGeometryCount: 0,
        };
      }

      const failMsg =
        res.message ||
        rawOptions?.failedMessage ||
        "Failed to validate/save YAML configuration";
      saveOutcome = { ok: false, message: failMsg, at: Date.now() };
      return {
        ok: false,
        status: res.status,
        config: baselineConfig ? cloneSafe(baselineConfig) : null,
        rawYaml: baselineYaml,
        message: failMsg,
        rejectedGeometryCount: 0,
      };
    } catch {
      const netMsg =
        rawOptions?.networkErrorMessage ||
        "Network error while saving YAML configuration";
      saveOutcome = { ok: false, message: netMsg, at: Date.now() };
      return {
        ok: false,
        status: 0,
        config: baselineConfig ? cloneSafe(baselineConfig) : null,
        rawYaml: baselineYaml,
        message: netMsg,
        rejectedGeometryCount: 0,
      };
    } finally {
      isSaving = false;
      publish();
    }
  };

  const discardRawEdits = (): void => {
    if (!hasUnsavedRawEdits) return;
    revision += 1;
    hasUnsavedRawEdits = false;
    saveOutcome = null;
    const structuredDirty = Boolean(
      draftConfig && !deepEqualConfigValue(baselineConfig, draftConfig)
    );
    if (structuredDirty && draftConfig) {
      rawYaml = buildCleanYamlFromDraft(draftConfig, baselineYaml);
      yamlStale = false;
      isDirty = true;
    } else {
      rawYaml = baselineYaml;
      yamlStale = false;
      isDirty = false;
    }
    publish();
  };

  const discardDraft = (): void => {
    revision += 1;
    sanitizedDraftCache = null;
    yamlStale = false;
    hasUnsavedRawEdits = false;
    isDirty = false;
    saveOutcome = null;
    if (baselineConfig) {
      draftConfig = cloneSafe(baselineConfig);
    }
    rawYaml = baselineYaml;
    publish();
  };

  return {
    transport,
    getSnapshot: () => cachedSnapshot,
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    hydrateFromServer,
    hydrateRawYamlFromServer,
    refreshFromServer,
    mutateDraft,
    setRawYamlDraft,
    ensureDraftYamlSynced,
    applyExternalPatch,
    applyOutOfBandPatch,
    commitStructured,
    commitRaw,
    discardRawEdits,
    discardDraft,
  };
}

const ConfigWorkbenchContext = createContext<ConfigWorkbenchSession | null>(
  null
);

export const ConfigWorkbenchProvider: React.FC<{
  session: ConfigWorkbenchSession;
  children: React.ReactNode;
}> = ({ session, children }) =>
  React.createElement(
    ConfigWorkbenchContext.Provider,
    { value: session },
    children
  );

export function useConfigWorkbenchSession(): ConfigWorkbenchSession | null {
  return useContext(ConfigWorkbenchContext);
}

export function useConfigWorkbenchSnapshot(
  session: ConfigWorkbenchSession
): ConfigWorkbenchSnapshot {
  return useSyncExternalStore(
    session.subscribe,
    session.getSnapshot,
    session.getSnapshot
  );
}
