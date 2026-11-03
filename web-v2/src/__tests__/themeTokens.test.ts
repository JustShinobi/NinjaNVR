import { describe, expect, test } from "bun:test";
import {
  ADJACENT_STRUCTURAL_PAIRS,
  CONTRAST_SURFACE_TOKENS,
  DARK_THEME_TOKENS,
  READABLE_TEXT_TOKENS,
  STRUCTURAL_SURFACE_TOKENS,
  cieLightnessFromLuminance,
  cieLightnessLStar,
  degradedDeltaLStar,
  deltaLStar,
  extractRootCssVariables,
  parseHexColor,
  quantizeHdmiLimitedRange,
  relativeLuminance,
  rgbToHex,
  simulateGamma24Transfer,
  srgbChannelToLinear,
  wcagContrastRatio,
} from "../utils/themeTokens";

async function readSrcFile(relativePathFromTests: string): Promise<string> {
  const fileUrl = new URL(relativePathFromTests, import.meta.url);
  const bunGlobal = (
    globalThis as unknown as {
      Bun: { file(url: URL): { text(): Promise<string> } };
    }
  ).Bun;
  return bunGlobal.file(fileUrl).text();
}

describe("themeTokens colorimetric utilities & cross-monitor invariants (#88)", () => {
  test("parseHexColor and rgbToHex round-trip 6-digit and 3-digit hex values", () => {
    expect(parseHexColor("#111722")).toEqual({ r: 17, g: 23, b: 34 });
    expect(parseHexColor("192231")).toEqual({ r: 25, g: 34, b: 49 });
    expect(parseHexColor("#0af")).toEqual({ r: 0, g: 170, b: 255 });
    expect(rgbToHex({ r: 17, g: 23, b: 34 })).toBe("#111722");
    expect(rgbToHex({ r: -5, g: 300, b: 128.4 })).toBe("#00ff80");

    let invalidMsg = "";
    try {
      parseHexColor("not-a-hex");
    } catch (err) {
      invalidMsg = err instanceof Error ? err.message : String(err);
    }
    expect(invalidMsg).toMatch(/Invalid hex color/);
  });

  test("srgbChannelToLinear, relativeLuminance, and CIE L* follow WCAG 2.1 / CIE 1976 formulas", () => {
    expect(srgbChannelToLinear(0)).toBe(0);
    expect(srgbChannelToLinear(10)).toBeCloseTo(10 / 255 / 12.92, 6);
    expect(srgbChannelToLinear(255)).toBeCloseTo(1, 6);

    expect(relativeLuminance("#000000")).toBe(0);
    expect(relativeLuminance("#ffffff")).toBeCloseTo(1, 6);
    expect(wcagContrastRatio("#ffffff", "#000000")).toBeCloseTo(21, 5);
    expect(wcagContrastRatio("#000000", "#ffffff")).toBeCloseTo(21, 5);

    expect(cieLightnessFromLuminance(0)).toBe(0);
    expect(cieLightnessFromLuminance(1)).toBeCloseTo(100, 5);
    expect(cieLightnessLStar("#000000")).toBe(0);
    expect(cieLightnessLStar("#ffffff")).toBeCloseTo(100, 5);
  });

  test("no UI structural surface (--bg-canvas, --bg-panel, --bg-card-header, --bg-elevated) falls into RGB <= 16 HDMI black-crush zone", () => {
    for (const token of STRUCTURAL_SURFACE_TOKENS) {
      const hex = DARK_THEME_TOKENS[token];
      const rgb = parseHexColor(hex);
      expect(rgb.r).toBeGreaterThan(16);
      expect(rgb.g).toBeGreaterThan(16);
      expect(rgb.b).toBeGreaterThan(16);
    }

    // Main surface hierarchy is strictly monotonic in perceptual lightness L*
    const lDeep = cieLightnessLStar(DARK_THEME_TOKENS["--bg-deep"]);
    const lInput = cieLightnessLStar(DARK_THEME_TOKENS["--bg-input"]);
    const lCanvas = cieLightnessLStar(DARK_THEME_TOKENS["--bg-canvas"]);
    const lSubcard = cieLightnessLStar(DARK_THEME_TOKENS["--bg-subcard"]);
    const lPanel = cieLightnessLStar(DARK_THEME_TOKENS["--bg-panel"]);
    const lHeader = cieLightnessLStar(DARK_THEME_TOKENS["--bg-card-header"]);
    const lElevated = cieLightnessLStar(DARK_THEME_TOKENS["--bg-elevated"]);
    const lHover = cieLightnessLStar(DARK_THEME_TOKENS["--bg-hover"]);
    const lActive = cieLightnessLStar(DARK_THEME_TOKENS["--bg-active"]);

    expect(lDeep).toBeLessThan(lInput);
    expect(lInput).toBeLessThan(lCanvas);
    expect(lCanvas).toBeLessThan(lSubcard);
    expect(lSubcard).toBeLessThan(lPanel);
    expect(lPanel).toBeLessThan(lHeader);
    expect(lHeader).toBeLessThan(lElevated);
    expect(lElevated).toBeLessThan(lHover);
    expect(lHover).toBeLessThan(lActive);

    // Primary surface steps maintain >= 4.5 CIE L* separation from canvas to panel to header
    expect(
      deltaLStar(DARK_THEME_TOKENS["--bg-canvas"], DARK_THEME_TOKENS["--bg-panel"])
    ).toBeGreaterThanOrEqual(4.5);
    expect(
      deltaLStar(
        DARK_THEME_TOKENS["--bg-panel"],
        DARK_THEME_TOKENS["--bg-card-header"]
      )
    ).toBeGreaterThanOrEqual(4.5);
  });

  test("all text and semantic signal-*-text tokens achieve >= 4.5:1 WCAG AA contrast across all dark surfaces", () => {
    for (const surfaceToken of CONTRAST_SURFACE_TOKENS) {
      const bgHex = DARK_THEME_TOKENS[surfaceToken];
      for (const textToken of READABLE_TEXT_TOKENS) {
        const fgHex = DARK_THEME_TOKENS[textToken];
        const ratio = wcagContrastRatio(fgHex, bgHex);
        expect(ratio).toBeGreaterThanOrEqual(4.5);
      }
    }

    // --text-muted maintains >= 4.8:1 across canvas, panel, and card-header
    for (const surface of [
      "--bg-canvas",
      "--bg-panel",
      "--bg-card-header",
    ] as const) {
      const ratio = wcagContrastRatio(
        DARK_THEME_TOKENS["--text-muted"],
        DARK_THEME_TOKENS[surface]
      );
      expect(ratio).toBeGreaterThanOrEqual(4.8);
    }
  });

  test("HDMI 16-235 limited-range quantization and gamma 2.4 transfer preserve positive delta-L* across all adjacent structural pairs", () => {
    // Legacy dark tokens (#040507 and #08090c) collapse to identical RGB 16,16,16 under HDMI 16-235 clipping
    expect(quantizeHdmiLimitedRange("#040507")).toEqual({ r: 16, g: 16, b: 16 });
    expect(quantizeHdmiLimitedRange("#08090c")).toEqual({ r: 16, g: 16, b: 16 });
    expect(degradedDeltaLStar("#040507", "#08090c", 2.4)).toBe(0);

    // Every new adjacent structural pair retains strictly positive delta-L* > 0 under 16-235 + gamma 2.4
    for (const [tokenA, tokenB] of ADJACENT_STRUCTURAL_PAIRS) {
      const hexA = DARK_THEME_TOKENS[tokenA];
      const hexB = DARK_THEME_TOKENS[tokenB];
      const dL = degradedDeltaLStar(hexA, hexB, 2.4);
      expect(dL).toBeGreaterThan(0);

      const simA = simulateGamma24Transfer(hexA, 2.4);
      const simB = simulateGamma24Transfer(hexB, 2.4);
      expect(simA.quantizedRgb).not.toEqual(simB.quantizedRgb);
      expect(simA.relativeLuminance).not.toBe(simB.relativeLuminance);
    }
  });

  test("web-v2/src/styles.css :root declarations match canonical DARK_THEME_TOKENS and structural CSS rules", async () => {
    const cssText = await readSrcFile("../styles.css");
    const rootVars = extractRootCssVariables(cssText);

    for (const [tokenName, expectedHex] of Object.entries(DARK_THEME_TOKENS)) {
      expect(rootVars[tokenName]?.toLowerCase()).toBe(expectedHex.toLowerCase());
    }
    expect(rootVars["--shadow-card"]).toBe("0 2px 8px rgba(0, 0, 0, 0.38)");

    // Video containers use --video-stage-bg
    expect(cssText).toMatch(
      /\.nvr-matrix-single\s*\{[^}]*background:\s*var\(--video-stage-bg\)/
    );
    expect(cssText).toMatch(
      /\.nvr-focus-stage\s*\{[^}]*background:\s*var\(--video-stage-bg\)/
    );
    expect(cssText).toMatch(
      /\.nvr-cam-tile\s*\{[^}]*background:\s*var\(--video-stage-bg\)/
    );

    // Cards, headers, subcards, dividers, and inputs use dedicated elevation & border tokens
    expect(cssText).toMatch(
      /\.nvr-cfg-card\s*\{[^}]*border:\s*1px solid var\(--border-card\)/
    );
    expect(cssText).toMatch(
      /\.nvr-cfg-card\s*\{[^}]*box-shadow:\s*var\(--shadow-card\)/
    );
    expect(cssText).toMatch(
      /\.nvr-cfg-card-header\s*\{[^}]*background:\s*var\(--bg-card-header\)/
    );
    expect(cssText).toMatch(
      /\.nvr-table th\s*\{[^}]*background:\s*var\(--bg-card-header\)/
    );
    expect(cssText).toMatch(
      /\.nvr-subcard\s*\{[^}]*background:\s*var\(--bg-subcard\)/
    );
    expect(cssText).toMatch(/\.nvr-subcard-header\s*\{/);
    expect(cssText).toMatch(
      /\.nvr-field-row\s*\{[^}]*border-bottom:\s*1px solid var\(--border-divider\)/
    );
    expect(cssText).toMatch(
      /\.nvr-input,\s*\.nvr-select,\s*textarea\s*\{[^}]*background:\s*var\(--bg-input\);\s*border:\s*1px solid var\(--border-input\)/
    );

    expect(extractRootCssVariables("body { color: red; }")).toEqual({});
  });

  test("settings panels and workspace views eliminate nested --bg-canvas and rgba(255,255,255,0.04) collisions", async () => {
    const targetFiles = [
      "../views/settings/ConsoleAndStreamsPanels.tsx",
      "../views/settings/VisionAndAiPanels.tsx",
      "../views/settings/RecordingAndSecurityPanels.tsx",
      "../App.tsx",
      "../components/RecordingCalendarPopover.tsx",
      "../views/ExploreWorkspace.tsx",
    ];

    for (const relPath of targetFiles) {
      const content = await readSrcFile(relPath);
      expect(content).not.toContain("var(--bg-canvas)");
      expect(content).not.toContain("var(--bg-deep)");
      expect(content).not.toMatch(/rgba\(\s*255\s*,\s*255\s*,\s*255\s*,\s*0\.04\s*\)/);
    }

    for (const settingsFile of [
      "../views/settings/ConsoleAndStreamsPanels.tsx",
      "../views/settings/VisionAndAiPanels.tsx",
      "../views/settings/RecordingAndSecurityPanels.tsx",
    ]) {
      const content = await readSrcFile(settingsFile);
      expect(content).toContain("nvr-subcard");
    }
  });
});
