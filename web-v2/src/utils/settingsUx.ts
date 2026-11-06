/**
 * Shared presentation helpers for the Settings workbench.
 *
 * Kept free of React so the scope grouping, unsaved-change counting and
 * sub-section ordering can be unit tested in isolation.
 */

export type SettingsScopeGroup = "global" | "camera";

/**
 * Label for the divider that separates global-only cards from the cards that
 * follow the scope selected in the Settings header.
 *
 * - `global`: cards that always write top-level config (shared by every camera).
 * - `camera`: cards that write `cameras.<cam>.*`, or the top-level defaults
 *   cameras inherit when the header scope is GLOBAL DEFAULTS.
 */
export function resolveScopeGroupLabel(
  group: SettingsScopeGroup,
  scope: "global" | string,
  inspectorCam: string,
  cameraCount: number,
  isPt = false
): string {
  if (group === "global") {
    const count = Number.isFinite(cameraCount) && cameraCount > 0 ? Math.floor(cameraCount) : 0;
    if (count === 0) {
      return isPt ? "GLOBAL · TODAS AS CÂMERAS" : "GLOBAL · ALL CAMERAS";
    }
    if (count === 1) {
      return isPt ? "GLOBAL · 1 CÂMERA" : "GLOBAL · 1 CAMERA";
    }
    return isPt
      ? `GLOBAL · TODAS AS ${count} CÂMERAS`
      : `GLOBAL · ALL ${count} CAMERAS`;
  }
  if (scope === "global") {
    return isPt
      ? "PADRÃO GLOBAL · HERDADO PELAS CÂMERAS"
      : "GLOBAL DEFAULTS · INHERITED BY CAMERAS";
  }
  const cam = (inspectorCam || "").trim().toUpperCase() || (isPt ? "CÂMERA" : "CAMERA");
  return isPt ? `${cam} · SOMENTE ESTA CÂMERA` : `${cam} · THIS CAMERA ONLY`;
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

const MAX_DIFF_DEPTH = 32;

function collectChangedPaths(
  base: unknown,
  draft: unknown,
  path: string[],
  out: string[][],
  depth: number
): void {
  if (depth > MAX_DIFF_DEPTH) {
    if (JSON.stringify(base) !== JSON.stringify(draft)) out.push(path);
    return;
  }
  if (isPlainObject(base) && isPlainObject(draft)) {
    const keys = new Set([...Object.keys(base), ...Object.keys(draft)]);
    for (const key of keys) {
      if (key === "__proto__" || key === "constructor" || key === "prototype") {
        continue;
      }
      collectChangedPaths(base[key], draft[key], [...path, key], out, depth + 1);
    }
    return;
  }
  if (isPlainObject(base) || isPlainObject(draft)) {
    const other = isPlainObject(base) ? draft : base;
    if (other !== undefined) {
      out.push(path); // type changed (object <-> scalar/array)
      return;
    }
    // A whole object appeared or disappeared: walk it so an empty object
    // materialised by a form is not reported as a change.
    collectChangedPaths(
      isPlainObject(base) ? base : {},
      isPlainObject(draft) ? draft : {},
      path,
      out,
      depth + 1
    );
    return;
  }
  if (base === undefined && draft === undefined) return;
  if (JSON.stringify(base) !== JSON.stringify(draft)) out.push(path);
}

/**
 * Settings group a changed leaf belongs to: `cameras.<cam>.<section>` for
 * per-camera config, otherwise the top-level section (`mqtt`, `genai`, ...).
 */
function settingsGroupOf(path: string[]): string {
  if (path[0] === "cameras") {
    return path.slice(0, 3).join(".");
  }
  return path[0] ?? "";
}

/**
 * Counts the settings sections that differ between the saved config and the
 * draft. Sections rather than leaves: forms fill in defaults when they write
 * a section, so a single toggle can materialise several leaves at once, and a
 * leaf count would overstate what the operator changed.
 */
export function countChangedSections(base: unknown, draft: unknown): number {
  const paths: string[][] = [];
  collectChangedPaths(base, draft, [], paths, 0);
  const groups = new Set(paths.map(settingsGroupOf));
  return groups.size;
}

export function formatUnsavedLabel(count: number, isPt = false): string {
  const n = Number.isFinite(count) && count > 0 ? Math.floor(count) : 0;
  if (n === 0) return "";
  if (isPt) {
    return n === 1 ? "1 SEÇÃO NÃO SALVA" : `${n} SEÇÕES NÃO SALVAS`;
  }
  return n === 1 ? "1 UNSAVED SECTION" : `${n} UNSAVED SECTIONS`;
}
