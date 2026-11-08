import { describe, expect, test } from "bun:test";
import {
  MASK_META_KEYS,
  coordStringToNormalizedPoints,
  coordStringToPoints,
  extractMaskList,
  extractMaskMeta,
  extractMaskPolygons,
  formatCoordPoints,
  parseCoordPoints,
  parsePolygonPoints,
  readMaskList,
  sanitizeMaskPolygonsValue,
  sanitizePolygonCoordString,
  writeMaskList,
} from "../utils/visionGeometry";

describe("VisionGeometry — canonical mask union codec & metadata round-trips", () => {
  test("exports unified MASK_META_KEYS covering all mask option keys", () => {
    expect(MASK_META_KEYS).toEqual([
      "mode",
      "style",
      "pixelate",
      "block_size",
      "enabled",
      "color",
      "intensity",
      "blur",
    ]);
  });

  test("extractMaskList and extractMaskMeta handle string, array, and object-with-metadata mask unions", () => {
    expect(extractMaskList(null)).toEqual([]);
    expect(extractMaskList(undefined)).toEqual([]);
    expect(extractMaskList("   ")).toEqual([]);
    expect(extractMaskMeta(null)).toBeNull();
    expect(extractMaskMeta("0.1,0.1,0.5,0.1,0.5,0.5")).toBeNull();
    expect(extractMaskMeta(["0.1,0.1,0.5,0.1,0.5,0.5"])).toBeNull();
    expect(extractMaskMeta({})).toBeNull();

    // 1. String mask union
    const strMask = " 0.100,0.100,0.500,0.100,0.500,0.500 ";
    expect(extractMaskList(strMask)).toEqual(["0.100,0.100,0.500,0.100,0.500,0.500"]);
    expect(extractMaskPolygons(strMask)).toEqual(["0.100,0.100,0.500,0.100,0.500,0.500"]);
    expect(readMaskList(strMask)).toEqual(["0.100,0.100,0.500,0.100,0.500,0.500"]);

    // 2. Array mask union (strings + coordinate records)
    const arrMask = [
      "0.1,0.1,0.4,0.1,0.4,0.4",
      { coordinates: " 0.5,0.5,0.9,0.5,0.9,0.9 ", enabled: true },
      { invalid: true },
      "",
    ];
    expect(extractMaskList(arrMask)).toEqual([
      "0.1,0.1,0.4,0.1,0.4,0.4",
      "0.5,0.5,0.9,0.5,0.9,0.9",
    ]);

    // 3. Object-with-metadata mask union (skips MASK_META_KEYS and boolean/numeric option fields)
    const objMask = {
      mode: "pixelate",
      style: "blur",
      pixelate: true,
      block_size: 16,
      enabled: true,
      color: "#111722",
      intensity: 0.85,
      blur: 8,
      custom_flag: false,
      custom_threshold: 42,
      window_a: "0.1,0.1,0.4,0.1,0.4,0.4,0.1,0.4",
      window_b: { coordinates: "0.6,0.2,0.9,0.2,0.9,0.7" },
    };
    expect(extractMaskMeta(objMask)).toEqual({
      mode: "pixelate",
      style: "blur",
      pixelate: true,
      block_size: 16,
      enabled: true,
      color: "#111722",
      intensity: 0.85,
      blur: 8,
    });
    expect(extractMaskList(objMask)).toEqual([
      "0.1,0.1,0.4,0.1,0.4,0.4,0.1,0.4",
      "0.6,0.2,0.9,0.2,0.9,0.7",
    ]);
  });

  test("round-trips string, array, and object-with-metadata masks through sanitizeMaskPolygonsValue and writeMaskList", () => {
    // String round-trip (pixel -> normalized)
    const strSanitized = sanitizeMaskPolygonsValue(
      "0,0,640,0,640,360,0,360",
      1280,
      720
    );
    expect(strSanitized).toEqual({
      sanitized: "0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500",
      rejectedCount: 0,
    });
    expect(extractMaskList(strSanitized.sanitized)).toEqual([
      "0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500",
    ]);

    // Array round-trip
    const arrSanitized = sanitizeMaskPolygonsValue(
      [
        "0,0,640,0,640,360,0,360",
        { coordinates: "640,360,1280,360,1280,720,640,720" },
        "0.2,0.2,0.2,0.2,0.2,0.2", // degenerate -> rejected
      ],
      1280,
      720
    );
    expect(arrSanitized.rejectedCount).toBe(1);
    expect(extractMaskList(arrSanitized.sanitized)).toEqual([
      "0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500",
      "0.500,0.500,1.000,0.500,1.000,1.000,0.500,1.000",
    ]);

    // Object-with-metadata round-trip
    const rawMetaMask = {
      mode: "pixelate",
      style: "mosaic",
      pixelate: true,
      block_size: 12,
      enabled: true,
      color: "#000000",
      intensity: 0.75,
      blur: 4,
      poly_px: "0,0,640,0,640,360,0,360",
      poly_norm: "0.5,0.5,0.9,0.5,0.9,0.9",
      degenerate: "0.1,0.1,0.1,0.1,0.1,0.1",
    };
    const objSanitized = sanitizeMaskPolygonsValue(rawMetaMask, 1280, 720);
    expect(objSanitized.rejectedCount).toBe(1);
    expect(extractMaskMeta(objSanitized.sanitized)).toEqual({
      mode: "pixelate",
      style: "mosaic",
      pixelate: true,
      block_size: 12,
      enabled: true,
      color: "#000000",
      intensity: 0.75,
      blur: 4,
    });
    const list = extractMaskList(objSanitized.sanitized);
    expect(list).toEqual([
      "0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500",
      "0.500,0.500,0.900,0.500,0.900,0.900",
    ]);

    const rewritten = writeMaskList(list, objSanitized.sanitized);
    expect(extractMaskMeta(rewritten)).toEqual(extractMaskMeta(objSanitized.sanitized));
    expect(extractMaskList(rewritten)).toEqual(list);
  });
});

describe("VisionGeometry — pixel-space (> 1.5) vs normalized (0..1) coordinate codecs", () => {
  test("parseCoordPoints and coordStringToNormalizedPoints normalize pixel-space (> 1.5) and clamp normalized (0..1) coordinates", () => {
    expect(parseCoordPoints(undefined)).toEqual([]);
    expect(parseCoordPoints("")).toEqual([]);
    expect(coordStringToNormalizedPoints("")).toEqual([]);

    // Normalized (0..1) coordinates
    const normStr = "0.1, 0.2, 0.75, 0.2, 0.75, 0.85";
    expect(parseCoordPoints(normStr, 1280, 720)).toEqual([
      [0.1, 0.2],
      [0.75, 0.2],
      [0.75, 0.85],
    ]);
    expect(coordStringToNormalizedPoints(normStr, 1280, 720)).toEqual([
      { x: 0.1, y: 0.2 },
      { x: 0.75, y: 0.2 },
      { x: 0.75, y: 0.85 },
    ]);

    // Slight canvas overshoot (1.0 < v <= 1.5) is clamped as normalized, not divided as pixel-space
    expect(parseCoordPoints("0.1,0.2,1.25,0.8", 1280, 720)).toEqual([
      [0.1, 0.2],
      [1, 0.8],
    ]);
    expect(coordStringToNormalizedPoints("0.1,0.2,1.25,0.8", 1280, 720)).toEqual([
      { x: 0.1, y: 0.2 },
      { x: 1, y: 0.8 },
    ]);

    // Pixel-space (> 1.5) coordinates against 1280x720
    const pxStr = "0,0,640,0,640,360,1280,720";
    expect(parseCoordPoints(pxStr, 1280, 720)).toEqual([
      [0, 0],
      [0.5, 0],
      [0.5, 0.5],
      [1, 1],
    ]);
    expect(coordStringToNormalizedPoints(pxStr, 1280, 720)).toEqual([
      { x: 0, y: 0 },
      { x: 0.5, y: 0 },
      { x: 0.5, y: 0.5 },
      { x: 1, y: 1 },
    ]);
    expect(coordStringToPoints(pxStr, 1280, 720)).toEqual([
      [0, 0],
      [0.5, 0],
      [0.5, 0.5],
      [1, 1],
    ]);
    expect(parsePolygonPoints(pxStr, 1280, 720)).toBe("0,0 500,0 500,500 1000,1000");

    // Round-trip formatCoordPoints -> sanitizePolygonCoordString -> parseCoordPoints
    const formatted = formatCoordPoints(
      [
        [0.125, 0.25],
        [0.625, 0.25],
        [0.625, 0.75],
      ],
      3
    );
    expect(formatted).toBe("0.125,0.250,0.625,0.250,0.625,0.750");
    expect(sanitizePolygonCoordString(formatted, 1280, 720)).toBe(formatted);
    expect(parseCoordPoints(formatted, 1280, 720)).toEqual([
      [0.125, 0.25],
      [0.625, 0.25],
      [0.625, 0.75],
    ]);
  });
});
