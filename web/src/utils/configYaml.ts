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

  if (trimmed.startsWith("#")) {
    return "";
  }
  return trimmed.replace(/\s+#.*$/, "").trim();
}

function isUnquotedYamlNullOrMask(rawField: string, parsedScalar: string): boolean {
  if (!parsedScalar || parsedScalar === "***" || parsedScalar === "*") {
    return true;
  }
  const trimmed = rawField.trim();
  const isQuoted = trimmed.startsWith('"') || trimmed.startsWith("'");
  if (!isQuoted && /^(?:null|~)$/i.test(parsedScalar)) {
    return true;
  }
  return false;
}

/**
 * Extracts all raw `rtsp://...`, `rtsps://...`, `http://...`, `https://...`, `rtmp://...` URLs
 * from an existing raw YAML string (excluding `#` comment lines) so scrubbed `*:*@` URLs in
 * `draftConfig` can be restored losslessly.
 */
function extractOriginalUrls(rawYaml: string): string[] {
  const nonCommentYaml = rawYaml
    .split(/\r?\n/)
    .filter((line) => !line.trim().startsWith("#"))
    .join("\n");
  const matches = nonCommentYaml.match(/(?:rtsps?|https?|rtmps?):\/\/[^\s"']+/gi);
  if (!matches) return [];
  const cleaned = matches.map((u) =>
    u
      .replace(/[,\]}]+$/, "")
      .replace(/\\"/g, '"')
      .replace(/\\\\/g, "\\")
      .replace(/''/g, "'")
  );
  return Array.from(new Set(cleaned));
}

const YAML_UNSAFE_OBJECT_KEYS = new Set([
  "__proto__",
  "constructor",
  "prototype",
]);

function parseYamlMapKey(rawKeySegment: string): string {
  const trimmed = rawKeySegment.trim();
  if (!trimmed) return "";
  if (trimmed.startsWith('"') || trimmed.startsWith("'")) {
    return parseYamlScalarValue(trimmed);
  }
  return trimmed;
}

/**
 * Extracts original camera ONVIF passwords from raw YAML so scrubbed `"***"`
 * passwords in `draft.cameras[cam].onvif.password` can be restored losslessly.
 */
function extractOriginalOnvifPasswords(rawYaml: string): Record<string, string> {
  const result: Record<string, string> = {};
  const lines = rawYaml.split(/\r?\n/);
  let inCameras = false;
  let camIndent: number | null = null;
  let currentCam = "";
  let inOnvif = false;
  let onvifIndent: number | null = null;

  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const indent = line.search(/\S/);
    if (indent === 0) {
      inCameras = /^cameras\s*:/.test(trimmed);
      camIndent = null;
      currentCam = "";
      inOnvif = false;
      onvifIndent = null;
      continue;
    }
    if (!inCameras) continue;

    if (camIndent === null || indent <= camIndent) {
      const m = trimmed.match(/^((?:"(?:\\.|[^"\\])*"|'(?:''|[^'])*'|[a-zA-Z0-9_.-]+))\s*:/);
      if (m) {
        camIndent = indent;
        const parsedKey = parseYamlMapKey(m[1]);
        currentCam = YAML_UNSAFE_OBJECT_KEYS.has(parsedKey) ? "" : parsedKey;
        inOnvif = false;
        onvifIndent = null;
      }
      continue;
    }

    if (currentCam && (onvifIndent === null || indent <= onvifIndent)) {
      if (/^onvif\s*:/.test(trimmed)) {
        inOnvif = true;
        onvifIndent = indent;
        continue;
      }
      if (onvifIndent !== null && indent <= onvifIndent) {
        inOnvif = false;
        onvifIndent = null;
      }
    }

    if (currentCam && inOnvif && onvifIndent !== null && indent > onvifIndent) {
      const pwMatch = trimmed.match(/^password\s*:\s*(.+)$/);
      if (pwMatch) {
        const unquoted = parseYamlScalarValue(pwMatch[1]);
        if (!isUnquotedYamlNullOrMask(pwMatch[1], unquoted)) {
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
      inGenAi = /^(?:"genai"|'genai'|genai)\s*:/.test(trimmed);
      continue;
    }
    if (inGenAi && indent >= 1) {
      const m = trimmed.match(/^api_key\s*:\s*(.+)$/);
      if (m) {
        const unquoted = parseYamlScalarValue(m[1]);
        if (!isUnquotedYamlNullOrMask(m[1], unquoted)) {
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
      inMqtt = /^(?:"mqtt"|'mqtt'|mqtt)\s*:/.test(trimmed);
      continue;
    }
    if (inMqtt && indent >= 1) {
      const m = trimmed.match(/^password\s*:\s*(.+)$/);
      if (m) {
        const unquoted = parseYamlScalarValue(m[1]);
        if (!isUnquotedYamlNullOrMask(m[1], unquoted)) {
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

const SENSITIVE_QUERY_PARAM_KEYS = new Set([
  "password",
  "pass",
  "passwd",
  "pwd",
  "token",
  "access_token",
  "auth_token",
  "id_token",
  "refresh_token",
  "session_token",
  "api_key",
  "apikey",
  "api-key",
  "x-api-key",
  "secret",
  "client_secret",
  "webhook_secret",
  "jwt_secret",
  "auth",
  "key",
  "sig",
  "signature",
]);

function findUserinfoAtIndex(afterScheme: string): number {
  const slashIdx = afterScheme.search(/[/\\]/);
  const preSlash = slashIdx === -1 ? afterScheme : afterScheme.slice(0, slashIdx);
  const qOrH = preSlash.search(/[?#]/);
  if (qOrH !== -1) {
    const beforeQh = preSlash.slice(0, qOrH);
    const atInAuth = beforeQh.lastIndexOf("@");
    if (atInAuth !== -1) {
      const afterAt = beforeQh.slice(atInAuth + 1);
      if (afterAt && !afterAt.includes("=") && !afterAt.includes(" ")) {
        return atInAuth;
      }
    }
    const colonIdx = beforeQh.indexOf(":");
    if (colonIdx === -1) return -1;
    const afterColon = beforeQh.slice(colonIdx + 1);
    if (afterColon.length > 0 && /^\d+$/.test(afterColon)) {
      return -1;
    }
  }
  const atIdx = preSlash.lastIndexOf("@");
  if (atIdx === -1) return -1;
  const afterAt = preSlash.slice(atIdx + 1);
  const hostEnd = afterAt.search(/[?# \t)\],"']/);
  const hostPortCandidate = hostEnd === -1 ? afterAt : afterAt.slice(0, hostEnd);
  if (!hostPortCandidate || hostPortCandidate.includes("=")) {
    return -1;
  }
  return atIdx;
}

function scrubUrlQueryParams(urlAfterAuthority: string): string {
  const qIdx = urlAfterAuthority.indexOf("?");
  if (qIdx === -1) return urlAfterAuthority;
  const prefix = urlAfterAuthority.slice(0, qIdx + 1);
  const afterQ = urlAfterAuthority.slice(qIdx + 1);
  const hIdx = afterQ.indexOf("#");
  const queryPart = hIdx === -1 ? afterQ : afterQ.slice(0, hIdx);
  const fragPart = hIdx === -1 ? "" : afterQ.slice(hIdx);
  const scrubbedPairs = queryPart.split("&").map((pair) => {
    const eqIdx = pair.indexOf("=");
    if (eqIdx === -1) return pair;
    const k = pair.slice(0, eqIdx);
    const v = pair.slice(eqIdx + 1);
    if (
      SENSITIVE_QUERY_PARAM_KEYS.has(k.trim().toLowerCase()) &&
      v.length > 0 &&
      v !== "***" &&
      v !== "*"
    ) {
      return `${k}=***`;
    }
    if (v.includes("://")) {
      return `${k}=${scrubUrl(v)}`;
    }
    return pair;
  });
  return `${prefix}${scrubbedPairs.join("&")}${fragPart}`;
}

function restoreUrlQueryParams(
  scrubbedRest: string,
  origAfterScheme: string
): string {
  if (!scrubbedRest.includes("=***")) return scrubbedRest;
  const qIdx = scrubbedRest.indexOf("?");
  const origQIdx = origAfterScheme.indexOf("?");
  if (qIdx === -1 || origQIdx === -1) return scrubbedRest;

  const origAfterQ = origAfterScheme.slice(origQIdx + 1);
  const origQuery = origAfterQ.split("#")[0];
  const origParams = new Map<string, string>();
  for (const pair of origQuery.split("&")) {
    const eqIdx = pair.indexOf("=");
    if (eqIdx !== -1) {
      origParams.set(
        pair.slice(0, eqIdx).trim().toLowerCase(),
        pair.slice(eqIdx + 1)
      );
    }
  }

  const prefix = scrubbedRest.slice(0, qIdx + 1);
  const afterQ = scrubbedRest.slice(qIdx + 1);
  const hIdx = afterQ.indexOf("#");
  const queryPart = hIdx === -1 ? afterQ : afterQ.slice(0, hIdx);
  const fragPart = hIdx === -1 ? "" : afterQ.slice(hIdx);
  const restoredPairs = queryPart.split("&").map((pair) => {
    const eqIdx = pair.indexOf("=");
    if (eqIdx === -1) return pair;
    const k = pair.slice(0, eqIdx);
    const v = pair.slice(eqIdx + 1);
    if (v === "***") {
      const origV = origParams.get(k.trim().toLowerCase());
      if (origV !== undefined) {
        return `${k}=${origV}`;
      }
    }
    return pair;
  });
  return `${prefix}${restoredPairs.join("&")}${fragPart}`;
}

function scrubUrl(input: string): string {
  const schemeIdx = input.indexOf("://");
  if (schemeIdx === -1) return input;
  const afterScheme = input.slice(schemeIdx + 3);
  const atIdx = findUserinfoAtIndex(afterScheme);
  if (atIdx !== -1) {
    const userinfo = afterScheme.slice(0, atIdx);
    const mask = userinfo.includes(":") ? "*:*" : "*";
    const restScrubbed = scrubUrlQueryParams(afterScheme.slice(atIdx + 1));
    return `${input.slice(0, schemeIdx + 3)}${mask}@${restScrubbed}`;
  }
  return `${input.slice(0, schemeIdx + 3)}${scrubUrlQueryParams(afterScheme)}`;
}

function extractHostPortFromAuthorityRest(rest: string): string {
  const slashIdx = rest.search(/[/?#\\]/);
  return slashIdx === -1 ? rest : rest.slice(0, slashIdx);
}

function extractHostFromAuthorityRest(rest: string): string {
  const hostPort = extractHostPortFromAuthorityRest(rest);
  if (hostPort.startsWith("[")) {
    const closeIdx = hostPort.indexOf("]");
    if (closeIdx !== -1) {
      return hostPort.slice(0, closeIdx + 1);
    }
  }
  return hostPort.split(":")[0];
}

function isUrlScrubbed(candidate: string): boolean {
  return (
    candidate.includes("://*:*@") ||
    candidate.includes("://*@") ||
    candidate.includes("=***")
  );
}

function restoreUrl(candidate: string, originals: string[]): string {
  if (!isUrlScrubbed(candidate)) {
    return candidate;
  }
  for (const orig of originals) {
    if (scrubUrl(orig) === candidate) {
      return orig;
    }
  }

  const schemeIdx = candidate.indexOf("://");
  if (schemeIdx === -1) return candidate;

  const rawSchemePrefix = candidate.slice(0, schemeIdx);
  const inputScheme = rawSchemePrefix.split(":").pop() || rawSchemePrefix;
  const afterScheme = candidate.slice(schemeIdx + 3);
  const atIdx = findUserinfoAtIndex(afterScheme);

  if (atIdx !== -1) {
    const inputUserinfo = afterScheme.slice(0, atIdx);
    const userinfoIsMasked = inputUserinfo === "*:*" || inputUserinfo === "*";
    const rest = afterScheme.slice(atIdx + 1);
    const targetHostPort = extractHostPortFromAuthorityRest(rest);
    const targetHost = extractHostFromAuthorityRest(rest);

    for (let tier = 0; tier < 4; tier++) {
      for (const orig of originals) {
        const oSchemeIdx = orig.indexOf("://");
        if (oSchemeIdx === -1) continue;
        const oRawScheme = orig.slice(0, oSchemeIdx);
        const oScheme = oRawScheme.split(":").pop() || oRawScheme;
        const oAfter = orig.slice(oSchemeIdx + 3);
        const oAt = findUserinfoAtIndex(oAfter);
        if (oAt === -1) continue;
        const creds = oAfter.slice(0, oAt);
        const oHostPart = oAfter.slice(oAt + 1);
        const oHostPort = extractHostPortFromAuthorityRest(oHostPart);
        const oHost = extractHostFromAuthorityRest(oHostPart);
        const schemeMatches =
          inputScheme.toLowerCase() === oScheme.toLowerCase();
        const matched =
          tier === 0
            ? schemeMatches && oHostPort === targetHostPort
            : tier === 1
            ? oHostPort === targetHostPort
            : tier === 2
            ? schemeMatches && oHost === targetHost
            : oHost === targetHost;
        if (matched) {
          const effectiveCreds = userinfoIsMasked ? creds : inputUserinfo;
          const restoredRest = restoreUrlQueryParams(rest, oAfter);
          return `${candidate.slice(0, schemeIdx + 3)}${effectiveCreds}@${restoredRest}`;
        }
      }
    }
  } else if (candidate.includes("=***")) {
    const targetHostPort = extractHostPortFromAuthorityRest(afterScheme);
    const targetHost = extractHostFromAuthorityRest(afterScheme);
    for (let tier = 0; tier < 4; tier++) {
      for (const orig of originals) {
        const oSchemeIdx = orig.indexOf("://");
        if (oSchemeIdx === -1) continue;
        const oRawScheme = orig.slice(0, oSchemeIdx);
        const oScheme = oRawScheme.split(":").pop() || oRawScheme;
        const oAfter = orig.slice(oSchemeIdx + 3);
        const oAt = findUserinfoAtIndex(oAfter);
        const oHostPart = oAt !== -1 ? oAfter.slice(oAt + 1) : oAfter;
        const oHostPort = extractHostPortFromAuthorityRest(oHostPart);
        const oHost = extractHostFromAuthorityRest(oHostPart);
        const schemeMatches =
          inputScheme.toLowerCase() === oScheme.toLowerCase();
        const matched =
          tier === 0
            ? schemeMatches && oHostPort === targetHostPort
            : tier === 1
            ? oHostPort === targetHostPort
            : tier === 2
            ? schemeMatches && oHost === targetHost
            : oHost === targetHost;
        if (matched) {
          const restoredAfter = restoreUrlQueryParams(afterScheme, oAfter);
          return `${candidate.slice(0, schemeIdx + 3)}${restoredAfter}`;
        }
      }
    }
  }
  return candidate;
}

const YAML_RESERVED_SCALAR_RE =
  /^(true|false|null|yes|no|on|off|~|\.nan|nan|\.inf|-\.inf|\+\.inf|inf|-inf|\+inf)$/i;
const YAML_NUMERIC_STRING_RE =
  /^(?:[+-]?(?:(?:0|[1-9](?:_?\d)*)(?:\.(?:\d(?:_?\d)*)?)?|\.\d(?:_?\d)*)(?:[eE][+-]?\d+)?|[+-]?0[xXoObB][0-9a-fA-F_]+|[+-]?0\d+)$/;

function escapeJsonQuotedForYaml(str: string): string {
  return JSON.stringify(str)
    .replace(/\u0085/g, "\\u0085")
    .replace(/\u2028/g, "\\u2028")
    .replace(/\u2029/g, "\\u2029")
    .replace(/\ufeff/g, "\\ufeff");
}

function yamlScalar(val: unknown): string {
  if (val === null || val === undefined) return "null";
  if (typeof val === "boolean") return String(val);
  if (typeof val === "number") {
    return Number.isFinite(val) ? String(val) : "0";
  }
  const s = String(val);
  if (
    s === "" ||
    /[\r\n\t\u0000-\u001f\u007f-\u009f\u2028\u2029\ufeff:#[\]{}&*!|>'"%@`,?]/.test(s) ||
    /^\s|\s$/.test(s) ||
    /^[-?.]/.test(s) ||
    YAML_RESERVED_SCALAR_RE.test(s) ||
    YAML_NUMERIC_STRING_RE.test(s)
  ) {
    return escapeJsonQuotedForYaml(s);
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
    return escapeJsonQuotedForYaml(key);
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
      if (Array.isArray(item)) {
        lines.push(`${pad}- [${item.map((x) => yamlScalar(x)).join(", ")}]`);
      } else if (item !== null && typeof item === "object") {
        const entries = Object.entries(item as Record<string, unknown>).filter(
          ([k, v]) => !YAML_UNSAFE_OBJECT_KEYS.has(k) && v !== undefined && v !== null
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
    ([k, v]) => !YAML_UNSAFE_OBJECT_KEYS.has(k) && v !== undefined && v !== null
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

function finiteInt(val: number | undefined | null, fallback: number): number {
  return typeof val === "number" && Number.isFinite(val)
    ? Math.round(val)
    : fallback;
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
    if (YAML_UNSAFE_OBJECT_KEYS.has(sName)) continue;
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
    if (YAML_UNSAFE_OBJECT_KEYS.has(camName) || !cam || typeof cam !== "object") {
      continue;
    }
    const inputs = (cam.ffmpeg?.inputs || []).map((inp) => ({
      path: restoreUrl(inp.path, originals),
      input_args: inp.input_args || "preset-rtsp-restream",
      roles: inp.roles && inp.roles.length > 0 ? inp.roles : ["detect"],
    }));

    camerasOut[camName] = {
      enabled: cam.enabled,
      ...(cam.indoor !== undefined ? { indoor: Boolean(cam.indoor) } : {}),
      ...(cam.webui_url && cam.webui_url.trim()
        ? { webui_url: restoreUrl(cam.webui_url.trim(), originals) }
        : {}),
      ...(cam.notes && cam.notes.trim() ? { notes: cam.notes.trim() } : {}),
      ffmpeg: {
        inputs,
        output_args: cam.ffmpeg?.output_args || {
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
        ...(cam.detect?.detector ? { detector: cam.detect.detector } : {}),
        width: finiteInt(cam.detect?.width, 1280),
        height: finiteInt(cam.detect?.height, 720),
        fps: finiteInt(cam.detect?.fps, 5),
        min_initialized: finiteInt(cam.detect?.min_initialized, 2),
        ...(cam.detect?.min_initialized_night !== undefined &&
        Number.isFinite(cam.detect.min_initialized_night)
          ? { min_initialized_night: finiteInt(cam.detect.min_initialized_night, 2) }
          : {}),
        max_disappeared: finiteInt(cam.detect?.max_disappeared, 25),
        stationary: {
          interval: finiteInt(cam.detect?.stationary?.interval, 50),
          threshold: finiteInt(cam.detect?.stationary?.threshold, 50),
          classifier: cam.detect?.stationary?.classifier ?? true,
        },
      },
      motion: {
        enabled: cam.motion?.enabled ?? true,
        threshold: finiteInt(cam.motion?.threshold, 30),
        lightning_threshold: finiteRound(
          cam.motion?.lightning_threshold,
          0.8,
          2
        ),
        improve_contrast: cam.motion?.improve_contrast ?? true,
        contour_area: finiteInt(cam.motion?.contour_area, 10),
        frame_height: finiteInt(cam.motion?.frame_height, 100),
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
      ...(cam.audio
        ? {
            audio: {
              enabled: Boolean(cam.audio.enabled),
              ...(cam.audio.min_volume_dbfs !== undefined &&
              Number.isFinite(cam.audio.min_volume_dbfs)
                ? { min_volume_dbfs: finiteRound(cam.audio.min_volume_dbfs, -40, 1) }
                : {}),
              ...(cam.audio.labels && cam.audio.labels.length > 0
                ? { labels: cam.audio.labels }
                : {}),
            },
          }
        : {}),
      record: {
        enabled: cam.record?.enabled ?? true,
        ...(cam.record?.continuous_substream !== undefined
          ? { continuous_substream: Boolean(cam.record.continuous_substream) }
          : {}),
        ...(cam.record?.mode_tiering !== undefined
          ? { mode_tiering: Boolean(cam.record.mode_tiering) }
          : {}),
        ...(cam.record?.max_size_gb !== undefined &&
        cam.record.max_size_gb !== null &&
        Number.isFinite(cam.record.max_size_gb)
          ? { max_size_gb: finiteRound(cam.record.max_size_gb, 0, 2) }
          : {}),
        ...(cam.record?.min_free_gb !== undefined &&
        cam.record.min_free_gb !== null &&
        Number.isFinite(cam.record.min_free_gb)
          ? { min_free_gb: finiteRound(cam.record.min_free_gb, 0, 2) }
          : {}),
        continuous: { days: finiteInt(cam.record?.continuous?.days, 14) },
        motion: { days: finiteInt(cam.record?.motion?.days, 0) },
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
        ...(cam.record?.storage_tiers
          ? { storage_tiers: cam.record.storage_tiers }
          : {}),
        ...(cam.record?.alert_backup
          ? { alert_backup: cam.record.alert_backup }
          : {}),
        ...(cam.record?.cold_compaction
          ? { cold_compaction: cam.record.cold_compaction }
          : {}),
      },
      snapshots: {
        enabled: cam.snapshots?.enabled ?? true,
        timestamp: cam.snapshots?.timestamp ?? false,
        bounding_box: cam.snapshots?.bounding_box ?? true,
        crop: cam.snapshots?.crop ?? false,
        quality: finiteInt(cam.snapshots?.quality, 70),
        ...(cam.snapshots?.high_res !== undefined
          ? { high_res: Boolean(cam.snapshots.high_res) }
          : {}),
        ...(cam.snapshots?.from_main_stream !== undefined
          ? { from_main_stream: Boolean(cam.snapshots.from_main_stream) }
          : {}),
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
                  interval_sec: finiteInt(roiCfg.interval_sec, 5),
                  hysteresis: finiteInt(roiCfg.hysteresis, 3),
                  ...(roiCfg.anomaly_state
                    ? { anomaly_state: roiCfg.anomaly_state }
                    : {}),
                  anomaly_timeout_sec: finiteInt(roiCfg.anomaly_timeout_sec, 300),
                },
              ])
            ),
          }
        : {}),
      privacy_mask: cam.privacy_mask || [],
      ...(cam.onvif &&
      ((cam.onvif.host && cam.onvif.host.trim().length > 0) ||
        (cam.onvif.user && cam.onvif.user.trim().length > 0) ||
        Boolean(cam.onvif.has_password) ||
        Boolean(cam.onvif.password && cam.onvif.password.trim().length > 0) ||
        Boolean(cam.onvif.autotracking) ||
        (cam.onvif.return_preset && cam.onvif.return_preset.trim().length > 0))
        ? (() => {
            const origCamOnvifPass = Object.prototype.hasOwnProperty.call(
              originalOnvifPasswords,
              camName
            )
              ? originalOnvifPasswords[camName]
              : undefined;
            const rawPass = cam.onvif.clear_password
              ? undefined
              : cam.onvif.password &&
                cam.onvif.password !== "***" &&
                cam.onvif.password !== "*"
              ? cam.onvif.password
              : origCamOnvifPass ||
                (cam.onvif.has_password ||
                cam.onvif.password === "***" ||
                cam.onvif.password === "*"
                  ? "***"
                  : undefined);
            return {
              onvif: {
                ...(cam.onvif.host && cam.onvif.host.trim()
                  ? { host: restoreUrl(cam.onvif.host.trim(), originals) }
                  : {}),
                port: finiteInt(cam.onvif.port, 80),
                ...(cam.onvif.user && cam.onvif.user.trim()
                  ? { user: cam.onvif.user.trim() }
                  : {}),
                ...(rawPass ? { password: rawPass } : {}),
                autotracking: Boolean(cam.onvif.autotracking),
                ...(cam.onvif.return_preset && cam.onvif.return_preset.trim()
                  ? { return_preset: cam.onvif.return_preset.trim() }
                  : {}),
                timeout_sec: finiteInt(cam.onvif.timeout_sec, 5),
              },
            };
          })()
        : {}),
      ...(cam.ui &&
      (cam.ui.order !== undefined ||
        cam.ui.dashboard !== undefined ||
        cam.ui.review !== undefined)
        ? {
            ui: {
              order: finiteInt(cam.ui.order, 0),
              dashboard: cam.ui.dashboard ?? true,
              ...(cam.ui.review !== undefined
                ? { review: Boolean(cam.ui.review) }
                : {}),
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
    : originalMqttPassword ||
      (draft.mqtt?.has_password ||
      draft.mqtt?.password === "***" ||
      draft.mqtt?.password === "*"
        ? "***"
        : undefined);

  const rootObj: Record<string, unknown> = {
    mqtt: {
      enabled: draft.mqtt?.enabled ?? true,
      host: draft.mqtt?.host || "127.0.0.1",
      port: finiteInt(draft.mqtt?.port, 1883),
      topic_prefix: draft.mqtt?.topic_prefix || "ninjanvr",
      client_id: draft.mqtt?.client_id || "ninjanvr",
      stats_interval: finiteInt(draft.mqtt?.stats_interval, 60),
      ...(draft.mqtt?.user ? { user: draft.mqtt.user } : {}),
      ...(rawMqttPassword ? { password: rawMqttPassword } : {}),
    },
    auth: {
      enabled: draft.auth?.enabled ?? true,
      ...(draft.auth?.local_login_enabled !== undefined
        ? { local_login_enabled: Boolean(draft.auth.local_login_enabled) }
        : {}),
      ...(draft.auth?.reset_admin_password
        ? { reset_admin_password: true }
        : {}),
      cookie_name: draft.auth?.cookie_name || "ninjanvr_token",
      cookie_secure: draft.auth?.cookie_secure ?? false,
      session_length: finiteInt(draft.auth?.session_length, 86400),
      refresh_time: finiteInt(draft.auth?.refresh_time, 1800),
      hash_iterations: finiteInt(draft.auth?.hash_iterations, 600000),
      ...(draft.auth?.trusted_proxies && draft.auth.trusted_proxies.length > 0
        ? { trusted_proxies: draft.auth.trusted_proxies }
        : {}),
      ...(draft.auth?.oidc
        ? {
            oidc: {
              enabled: Boolean(draft.auth.oidc.enabled),
              ...(draft.auth.oidc.provider_name
                ? { provider_name: draft.auth.oidc.provider_name }
                : {}),
              ...(draft.auth.oidc.issuer_url
                ? {
                    issuer_url: restoreUrl(
                      draft.auth.oidc.issuer_url,
                      originals
                    ),
                  }
                : {}),
              ...(draft.auth.oidc.client_id
                ? { client_id: draft.auth.oidc.client_id }
                : {}),
              ...(draft.auth.oidc.client_secret_env
                ? { client_secret_env: draft.auth.oidc.client_secret_env }
                : {}),
              ...(draft.auth.oidc.scopes && draft.auth.oidc.scopes.length > 0
                ? { scopes: draft.auth.oidc.scopes }
                : {}),
              ...(draft.auth.oidc.auto_redirect !== undefined
                ? { auto_redirect: Boolean(draft.auth.oidc.auto_redirect) }
                : {}),
              ...(draft.auth.oidc.username_claim
                ? { username_claim: draft.auth.oidc.username_claim }
                : {}),
              ...(draft.auth.oidc.groups_claim
                ? { groups_claim: draft.auth.oidc.groups_claim }
                : {}),
              ...(draft.auth.oidc.default_role
                ? { default_role: draft.auth.oidc.default_role }
                : {}),
            },
          }
        : {}),
      ...(draft.auth?.proxy
        ? {
            proxy: {
              enabled: Boolean(draft.auth.proxy.enabled),
              ...(draft.auth.proxy.trusted_proxies &&
              draft.auth.proxy.trusted_proxies.length > 0
                ? { trusted_proxies: draft.auth.proxy.trusted_proxies }
                : {}),
              ...(draft.auth.proxy.user_header
                ? { user_header: draft.auth.proxy.user_header }
                : {}),
              ...(draft.auth.proxy.groups_header
                ? { groups_header: draft.auth.proxy.groups_header }
                : {}),
              ...(draft.auth.proxy.logout_url
                ? {
                    logout_url: restoreUrl(
                      draft.auth.proxy.logout_url,
                      originals
                    ),
                  }
                : {}),
            },
          }
        : {}),
    },
    tls: {
      enabled: draft.tls?.enabled ?? true,
    },
    database: {
      path: draft.database?.path || "/config/ninjanvr.db",
    },
    detectors: detectorsOut,
    model: {
      width: finiteInt(draft.model?.width, 300),
      height: finiteInt(draft.model?.height, 300),
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
            : originalGenAiApiKey ||
              (draft.genai.has_api_key ||
              draft.genai.api_key?.trim() === "***" ||
              draft.genai.api_key?.trim() === "*"
                ? "***"
                : undefined);
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
              ...(draft.genai.timeout_ms && Number.isFinite(draft.genai.timeout_ms)
                ? { timeout_ms: finiteInt(draft.genai.timeout_ms, 5000) }
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
            min_face_area: finiteInt(draft.face_recognition.min_face_area, 750),
            unknown_alert: draft.face_recognition.unknown_alert ?? false,
          },
        }
      : {}),
    ...(draft.review ? { review: draft.review } : {}),
    detect: {
      enabled: draft.detect?.enabled ?? true,
      width: finiteInt(draft.detect?.width, 1280),
      height: finiteInt(draft.detect?.height, 720),
      fps: finiteInt(draft.detect?.fps, 5),
      min_initialized: finiteInt(draft.detect?.min_initialized, 2),
      max_disappeared: finiteInt(draft.detect?.max_disappeared, 25),
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
    ...(draft.audio
      ? {
          audio: {
            enabled: Boolean(draft.audio.enabled),
            ...(draft.audio.min_volume_dbfs !== undefined &&
            Number.isFinite(draft.audio.min_volume_dbfs)
              ? { min_volume_dbfs: finiteRound(draft.audio.min_volume_dbfs, -40, 1) }
              : {}),
            ...(draft.audio.labels && draft.audio.labels.length > 0
              ? { labels: draft.audio.labels }
              : {}),
          },
        }
      : {}),
    record: {
      enabled: draft.record?.enabled ?? true,
      expire_interval: finiteInt(draft.record?.expire_interval, 60),
      ...(draft.record?.continuous_substream !== undefined
        ? { continuous_substream: Boolean(draft.record.continuous_substream) }
        : {}),
      ...(draft.record?.mode_tiering !== undefined
        ? { mode_tiering: Boolean(draft.record.mode_tiering) }
        : {}),
      ...(draft.record?.max_size_gb !== undefined &&
      draft.record.max_size_gb !== null &&
      Number.isFinite(draft.record.max_size_gb)
        ? { max_size_gb: finiteRound(draft.record.max_size_gb, 0, 2) }
        : {}),
      ...(draft.record?.min_free_gb !== undefined &&
      draft.record.min_free_gb !== null &&
      Number.isFinite(draft.record.min_free_gb)
        ? { min_free_gb: finiteRound(draft.record.min_free_gb, 0, 2) }
        : {}),
      continuous: { days: finiteInt(draft.record?.continuous?.days, 14) },
      motion: { days: finiteInt(draft.record?.motion?.days, 0) },
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
      ...(draft.record?.storage_tiers
        ? { storage_tiers: draft.record.storage_tiers }
        : {}),
      ...(draft.record?.alert_backup
        ? { alert_backup: draft.record.alert_backup }
        : {}),
      ...(draft.record?.cold_compaction
        ? { cold_compaction: draft.record.cold_compaction }
        : {}),
    },
    snapshots: {
      enabled: draft.snapshots?.enabled ?? true,
      timestamp: draft.snapshots?.timestamp ?? false,
      bounding_box: draft.snapshots?.bounding_box ?? true,
      crop: draft.snapshots?.crop ?? false,
      quality: finiteInt(draft.snapshots?.quality, 70),
      ...(draft.snapshots?.high_res !== undefined
        ? { high_res: Boolean(draft.snapshots.high_res) }
        : {}),
      ...(draft.snapshots?.from_main_stream !== undefined
        ? { from_main_stream: Boolean(draft.snapshots.from_main_stream) }
        : {}),
      ...(draft.snapshots?.selection
        ? { selection: draft.snapshots.selection }
        : {}),
    },
    go2rtc: {
      streams: streamsOut,
    },
    ...(draft.peers && Object.keys(draft.peers).length > 0
      ? {
          peers: Object.fromEntries(
            Object.entries(draft.peers).map(([peerId, peerCfg]) => [
              peerId,
              {
                enabled: Boolean(peerCfg.enabled),
                url: restoreUrl(peerCfg.url || "", originals),
                ...(peerCfg.token_env ? { token_env: peerCfg.token_env } : {}),
              },
            ])
          ),
        }
      : {}),
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
  if (!norm || !knownPlates || typeof knownPlates !== "object") return null;
  if (
    Object.prototype.hasOwnProperty.call(knownPlates, norm) &&
    typeof knownPlates[norm] === "string" &&
    knownPlates[norm]
  ) {
    return knownPlates[norm];
  }
  for (const [k, v] of Object.entries(knownPlates)) {
    if (typeof v === "string" && normalizeLicensePlate(k) === norm) {
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
