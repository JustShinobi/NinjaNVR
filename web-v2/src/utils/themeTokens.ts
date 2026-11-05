export {
  type EffectiveTheme,
  type ThemeMode,
  applyThemeToDocument,
  cycleThemeMode,
  resolveEffectiveTheme,
  resolveInitialThemeMode,
} from "./format";

export interface RgbColor {
  r: number;
  g: number;
  b: number;
}

export const DARK_THEME_TOKENS = {
  "--video-stage-bg": "#05070a",
  "--bg-deep": "#0b0f17",
  "--bg-canvas": "#111722",
  "--bg-panel": "#192231",
  "--bg-card-header": "#212c3f",
  "--bg-subcard": "#141c29",
  "--bg-input": "#0d131d",
  "--bg-elevated": "#243044",
  "--bg-hover": "#2d3b54",
  "--bg-active": "#354663",
  "--border-divider": "#263247",
  "--border-hairline": "#31405a",
  "--border-card": "#3a4b68",
  "--border-strong": "#4a5f82",
  "--border-input": "#4f658a",
  "--text-primary": "#f1f5f9",
  "--text-secondary": "#b0bfd4",
  "--text-muted": "#8ea2bd",
  "--signal-live": "#10b981",
  "--signal-live-text": "#34d399",
  "--signal-motion": "#f59e0b",
  "--signal-motion-text": "#fcd34d",
  "--signal-alert": "#ef4444",
  "--signal-alert-text": "#fca5a5",
  "--signal-accent": "#0ea5e9",
  "--signal-accent-text": "#38bdf8",
} as const;

export const LIGHT_THEME_TOKENS = {
  "--video-stage-bg": "#070a0f",
  "--bg-deep": "#cdd6e4",
  "--bg-canvas": "#dce3ee",
  "--bg-panel": "#f5f7fb",
  "--bg-card-header": "#e4eaf4",
  "--bg-subcard": "#eaeff7",
  "--bg-input": "#ffffff",
  "--bg-elevated": "#e2e8f2",
  "--bg-hover": "#d2dceb",
  "--bg-active": "#c2d0e5",
  "--border-divider": "#c5d0e0",
  "--border-hairline": "#a8b7cc",
  "--border-card": "#8fa0ba",
  "--border-strong": "#64748b",
  "--border-input": "#5b6b82",
  "--text-primary": "#0f172a",
  "--text-secondary": "#2d3c52",
  "--text-muted": "#475569",
  "--signal-live": "#059669",
  "--signal-live-text": "#065f46",
  "--signal-motion": "#d97706",
  "--signal-motion-text": "#92400e",
  "--signal-alert": "#dc2626",
  "--signal-alert-text": "#991b1b",
  "--signal-accent": "#0284c7",
  "--signal-accent-text": "#075985",
} as const;

export const DARK_HIGH_CONTRAST_TOKENS = {
  "--border-divider": "#3b4d6b",
  "--border-hairline": "#4a6084",
  "--border-card": "#58729c",
  "--border-strong": "#6e8cbe",
  "--border-input": "#7595ca",
  "--text-primary": "#ffffff",
  "--text-secondary": "#d0ddef",
  "--text-muted": "#b0bfd4",
} as const;

export const LIGHT_HIGH_CONTRAST_TOKENS = {
  "--border-divider": "#94a3b8",
  "--border-hairline": "#7588a3",
  "--border-card": "#5b6e8a",
  "--border-strong": "#3e4c63",
  "--border-input": "#334155",
  "--text-primary": "#020617",
  "--text-secondary": "#1e293b",
  "--text-muted": "#2d3c52",
} as const;

export type DarkThemeTokenName = keyof typeof DARK_THEME_TOKENS;
export type LightThemeTokenName = keyof typeof LIGHT_THEME_TOKENS;

export const STRUCTURAL_SURFACE_TOKENS = [
  "--bg-canvas",
  "--bg-panel",
  "--bg-card-header",
  "--bg-elevated",
] as const;

export const LIGHT_ANTI_WHITE_CRUSH_SURFACE_TOKENS = [
  "--bg-deep",
  "--bg-canvas",
  "--bg-card-header",
  "--bg-subcard",
  "--bg-elevated",
  "--bg-hover",
  "--bg-active",
] as const;

export const CONTRAST_SURFACE_TOKENS = [
  "--bg-canvas",
  "--bg-panel",
  "--bg-subcard",
  "--bg-card-header",
  "--bg-elevated",
] as const;

export const READABLE_TEXT_TOKENS = [
  "--text-primary",
  "--text-secondary",
  "--text-muted",
  "--signal-live-text",
  "--signal-motion-text",
  "--signal-alert-text",
  "--signal-accent-text",
] as const;

export const ADJACENT_STRUCTURAL_PAIRS: ReadonlyArray<
  readonly [DarkThemeTokenName, DarkThemeTokenName]
> = [
  ["--bg-canvas", "--bg-panel"],
  ["--bg-panel", "--bg-card-header"],
  ["--bg-panel", "--bg-subcard"],
  ["--bg-subcard", "--bg-input"],
  ["--bg-panel", "--border-card"],
] as const;

export const LIGHT_ADJACENT_STRUCTURAL_PAIRS: ReadonlyArray<
  readonly [LightThemeTokenName, LightThemeTokenName]
> = [
  ["--bg-deep", "--bg-canvas"],
  ["--bg-canvas", "--bg-card-header"],
  ["--bg-card-header", "--bg-subcard"],
  ["--bg-subcard", "--bg-input"],
  ["--bg-panel", "--border-card"],
  ["--bg-panel", "--border-input"],
  ["--bg-subcard", "--border-input"],
] as const;

export function parseHexColor(hex: string): RgbColor {
  const clean = hex.trim().replace(/^#/, "");
  if (/^[0-9a-fA-F]{3}$/.test(clean)) {
    const r = parseInt(clean[0] + clean[0], 16);
    const g = parseInt(clean[1] + clean[1], 16);
    const b = parseInt(clean[2] + clean[2], 16);
    return { r, g, b };
  }
  if (/^[0-9a-fA-F]{6}$/.test(clean)) {
    const r = parseInt(clean.slice(0, 2), 16);
    const g = parseInt(clean.slice(2, 4), 16);
    const b = parseInt(clean.slice(4, 6), 16);
    return { r, g, b };
  }
  throw new Error(`Invalid hex color: ${hex}`);
}

export function rgbToHex(rgb: RgbColor): string {
  const clampByte = (n: number) => {
    const safe = typeof n === "number" && Number.isFinite(n) ? n : 0;
    return Math.max(0, Math.min(255, Math.round(safe)))
      .toString(16)
      .padStart(2, "0");
  };
  return `#${clampByte(rgb.r)}${clampByte(rgb.g)}${clampByte(rgb.b)}`;
}

function resolveRgb(color: string | RgbColor): RgbColor {
  return typeof color === "string" ? parseHexColor(color) : color;
}

export function srgbChannelToLinear(channel8Bit: number): number {
  const safe =
    typeof channel8Bit === "number" && Number.isFinite(channel8Bit)
      ? channel8Bit
      : 0;
  const c = Math.max(0, Math.min(255, safe)) / 255;
  return c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
}

export function relativeLuminance(color: string | RgbColor): number {
  const { r, g, b } = resolveRgb(color);
  const rLin = srgbChannelToLinear(r);
  const gLin = srgbChannelToLinear(g);
  const bLin = srgbChannelToLinear(b);
  return 0.2126 * rLin + 0.7152 * gLin + 0.0722 * bLin;
}

export function wcagContrastRatio(
  foreground: string | RgbColor,
  background: string | RgbColor
): number {
  const lumA = relativeLuminance(foreground);
  const lumB = relativeLuminance(background);
  const lighter = Math.max(lumA, lumB);
  const darker = Math.min(lumA, lumB);
  return (lighter + 0.05) / (darker + 0.05);
}

export function cieLightnessLStar(color: string | RgbColor): number {
  const y = relativeLuminance(color);
  return cieLightnessFromLuminance(y);
}

export function cieLightnessFromLuminance(relativeY: number): number {
  const safeY =
    typeof relativeY === "number" && Number.isFinite(relativeY) ? relativeY : 0;
  const y = Math.max(0, Math.min(1, safeY));
  const epsilon = 216 / 24389;
  const kappa = 24389 / 27;
  return y <= epsilon ? y * kappa : 116 * Math.cbrt(y) - 16;
}

export function deltaLStar(
  colorA: string | RgbColor,
  colorB: string | RgbColor
): number {
  return Math.abs(cieLightnessLStar(colorA) - cieLightnessLStar(colorB));
}

export function quantizeHdmiLimitedRange(color: string | RgbColor): RgbColor {
  const { r, g, b } = resolveRgb(color);
  const clamp16To235 = (v: number) => {
    const safe = typeof v === "number" && Number.isFinite(v) ? v : 16;
    return Math.max(16, Math.min(235, Math.round(safe)));
  };
  return {
    r: clamp16To235(r),
    g: clamp16To235(g),
    b: clamp16To235(b),
  };
}

export function simulateGamma24Transfer(
  color: string | RgbColor,
  gamma = 2.4
): {
  quantizedRgb: RgbColor;
  displayRgb: RgbColor;
  relativeLuminance: number;
  lStar: number;
} {
  const quantizedRgb = quantizeHdmiLimitedRange(color);
  const safeGamma =
    typeof gamma === "number" && Number.isFinite(gamma) && gamma > 0
      ? gamma
      : 2.4;
  const transferChannel = (v: number) => {
    const normLimited = (v - 16) / (235 - 16);
    return Math.pow(Math.max(0, Math.min(1, normLimited)), safeGamma);
  };
  const rLin = transferChannel(quantizedRgb.r);
  const gLin = transferChannel(quantizedRgb.g);
  const bLin = transferChannel(quantizedRgb.b);
  const lum = 0.2126 * rLin + 0.7152 * gLin + 0.0722 * bLin;
  return {
    quantizedRgb,
    displayRgb: {
      r: Math.round(rLin * 255),
      g: Math.round(gLin * 255),
      b: Math.round(bLin * 255),
    },
    relativeLuminance: lum,
    lStar: cieLightnessFromLuminance(lum),
  };
}

export function degradedDeltaLStar(
  colorA: string | RgbColor,
  colorB: string | RgbColor,
  gamma = 2.4
): number {
  const simA = simulateGamma24Transfer(colorA, gamma);
  const simB = simulateGamma24Transfer(colorB, gamma);
  return Math.abs(simA.lStar - simB.lStar);
}

export function extractSelectorCssVariables(
  cssContent: string,
  selector: string
): Record<string, string> {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const blockRegex = new RegExp(`${escaped}\\s*\\{([\\s\\S]*?)\\}`);
  const blockMatch = cssContent.match(blockRegex);
  if (!blockMatch) return {};
  const body = blockMatch[1];
  const vars: Record<string, string> = {};
  const declRegex = /(--[a-zA-Z0-9-]+)\s*:\s*([^;]+);/g;
  let match: RegExpExecArray | null;
  while ((match = declRegex.exec(body)) !== null) {
    vars[match[1].trim()] = match[2].trim();
  }
  return vars;
}

export function extractRootCssVariables(cssContent: string): Record<string, string> {
  return extractSelectorCssVariables(cssContent, ":root");
}

export function extractLightThemeCssVariables(cssContent: string): Record<string, string> {
  return extractSelectorCssVariables(cssContent, '[data-theme="light"]');
}

export interface CanvasThemePalette {
  theme: "dark" | "light";
  gridLineStroke: string;
  sparklineGridStroke: string;
  continuousBarFill: string;
  audioWaveformFill: string;
  objectBarFill: string;
  motionBarFill: string;
  gapFill: string;
  gapHatchStroke: string;
  alertPinFill: string;
  motionPinFill: string;
  floorplanStageBg: string;
  floorplanGridStroke: string;
  floorplanBorderStroke: string;
  floorplanRoomFill: string;
  floorplanRoomSelectedFill: string;
  floorplanRoomLabelFill: string;
  floorplanWallStroke: string;
  floorplanWallNodeFill: string;
  floorplanCamNodeStroke: string;
  floorplanCamLabelFill: string;
  floorplanObjectLabelFill: string;
}

export function resolveCanvasThemePalette(
  themeHint?: "dark" | "light"
): CanvasThemePalette {
  const domTheme =
    typeof document !== "undefined"
      ? document.documentElement?.dataset?.theme
      : undefined;
  const isLight =
    themeHint === "light" || (themeHint === undefined && domTheme === "light");
  const baseTokens = isLight ? LIGHT_THEME_TOKENS : DARK_THEME_TOKENS;

  const computedStyle =
    typeof window !== "undefined" &&
    typeof window.getComputedStyle === "function" &&
    typeof document !== "undefined" &&
    document.documentElement
      ? window.getComputedStyle(document.documentElement)
      : null;

  const readToken = (name: keyof typeof DARK_THEME_TOKENS): string => {
    const fromCss = computedStyle?.getPropertyValue(name)?.trim();
    return fromCss || baseTokens[name];
  };

  if (isLight) {
    return {
      theme: "light",
      gridLineStroke: "rgba(15, 23, 42, 0.14)",
      sparklineGridStroke: "rgba(15, 23, 42, 0.12)",
      continuousBarFill: "rgba(5, 150, 105, 0.52)",
      audioWaveformFill: "rgba(2, 132, 199, 0.82)",
      objectBarFill: "rgba(220, 38, 38, 0.78)",
      motionBarFill: "rgba(217, 119, 6, 0.58)",
      gapFill: "rgba(220, 38, 38, 0.20)",
      gapHatchStroke: "rgba(180, 83, 9, 0.72)",
      alertPinFill: readToken("--signal-alert"),
      motionPinFill: readToken("--signal-motion"),
      floorplanStageBg:
        "radial-gradient(circle at 50% 50%, rgba(2, 132, 199, 0.08), rgba(220, 227, 238, 0.98))",
      floorplanGridStroke: "rgba(71, 85, 105, 0.22)",
      floorplanBorderStroke: "rgba(2, 132, 199, 0.42)",
      floorplanRoomFill: "rgba(245, 247, 251, 0.84)",
      floorplanRoomSelectedFill: "rgba(2, 132, 199, 0.18)",
      floorplanRoomLabelFill: readToken("--text-primary"),
      floorplanWallStroke: "#475569",
      floorplanWallNodeFill: "#334155",
      floorplanCamNodeStroke: readToken("--bg-panel"),
      floorplanCamLabelFill: readToken("--text-primary"),
      floorplanObjectLabelFill: readToken("--signal-alert-text"),
    };
  }

  return {
    theme: "dark",
    gridLineStroke: "rgba(255, 255, 255, 0.06)",
    sparklineGridStroke: "rgba(255, 255, 255, 0.06)",
    continuousBarFill: "rgba(16, 185, 129, 0.42)",
    audioWaveformFill: "rgba(6, 182, 212, 0.78)",
    objectBarFill: "rgba(239, 68, 68, 0.72)",
    motionBarFill: "rgba(245, 158, 11, 0.48)",
    gapFill: "rgba(239, 68, 68, 0.22)",
    gapHatchStroke: "rgba(245, 158, 11, 0.65)",
    alertPinFill: readToken("--signal-alert"),
    motionPinFill: readToken("--signal-motion"),
    floorplanStageBg:
      "radial-gradient(circle at 50% 50%, rgba(14, 165, 233, 0.06), rgba(7, 10, 15, 0.98))",
    floorplanGridStroke: "rgba(148, 163, 184, 0.14)",
    floorplanBorderStroke: "rgba(56, 189, 248, 0.25)",
    floorplanRoomFill: "rgba(15, 23, 42, 0.55)",
    floorplanRoomSelectedFill: "rgba(56, 189, 248, 0.18)",
    floorplanRoomLabelFill: readToken("--text-primary"),
    floorplanWallStroke: "#94a3b8",
    floorplanWallNodeFill: "#cbd5e1",
    floorplanCamNodeStroke: "#070a0f",
    floorplanCamLabelFill: readToken("--text-primary"),
    floorplanObjectLabelFill: readToken("--signal-alert-text"),
  };
}
