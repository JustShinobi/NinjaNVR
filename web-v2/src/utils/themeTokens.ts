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

export type DarkThemeTokenName = keyof typeof DARK_THEME_TOKENS;

export const STRUCTURAL_SURFACE_TOKENS = [
  "--bg-canvas",
  "--bg-panel",
  "--bg-card-header",
  "--bg-elevated",
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
  const clampByte = (n: number) =>
    Math.max(0, Math.min(255, Math.round(n)))
      .toString(16)
      .padStart(2, "0");
  return `#${clampByte(rgb.r)}${clampByte(rgb.g)}${clampByte(rgb.b)}`;
}

function resolveRgb(color: string | RgbColor): RgbColor {
  return typeof color === "string" ? parseHexColor(color) : color;
}

export function srgbChannelToLinear(channel8Bit: number): number {
  const c = Math.max(0, Math.min(255, channel8Bit)) / 255;
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
  const y = Math.max(0, Math.min(1, relativeY));
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
  const clamp16To235 = (v: number) => Math.max(16, Math.min(235, Math.round(v)));
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
  const safeGamma = gamma > 0 ? gamma : 2.4;
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

export function extractRootCssVariables(cssContent: string): Record<string, string> {
  const rootMatch = cssContent.match(/:root\s*\{([\s\S]*?)\}/);
  if (!rootMatch) return {};
  const body = rootMatch[1];
  const vars: Record<string, string> = {};
  const declRegex = /(--[a-zA-Z0-9-]+)\s*:\s*([^;]+);/g;
  let match: RegExpExecArray | null;
  while ((match = declRegex.exec(body)) !== null) {
    vars[match[1].trim()] = match[2].trim();
  }
  return vars;
}
