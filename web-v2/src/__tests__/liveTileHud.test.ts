import { describe, expect, test } from "bun:test";
import {
  buildCameraSnapshotUrl,
  formatBadgeCount,
  formatCameraTelemetryRows,
  isCompactViewport,
  resolveCameraRuntimeFlags,
  resolveMobileNavTabs,
  resolveTileException,
  resolveTileIdentityTone,
} from "../utils/liveTileHud";

describe("resolveTileException (HUD by exception)", () => {
  test("normal state renders no chip at all", () => {
    expect(resolveTileException({})).toBeNull();
    expect(
      resolveTileException({ recordEnabled: true, motion: false, objectLabels: [] })
    ).toBeNull();
  });

  test("tamper outranks everything and counts hidden exceptions", () => {
    const ex = resolveTileException({
      tamper: true,
      objectLabels: ["person"],
      recordEnabled: false,
      motion: true,
    });
    expect(ex?.kind).toBe("tamper");
    expect(ex?.tone).toBe("alert");
    expect(ex?.hiddenCount).toBe(2);
  });

  test("objects dedupe, uppercase and cap the label list", () => {
    const ex = resolveTileException({
      objectLabels: ["person", "Person", "car", "dog"],
      motion: true,
    });
    expect(ex).toEqual({ kind: "object", label: "PERSON, CAR +1", tone: "alert", hiddenCount: 0 });
  });

  test("anomalous ROI beats record-off, record-off beats motion", () => {
    expect(
      resolveTileException({ roiAnomalies: ["gate open"], recordEnabled: false })?.kind
    ).toBe("roi");
    const rec = resolveTileException({ recordEnabled: false, motion: true });
    expect(rec?.kind).toBe("record-off");
    expect(rec?.label).toBe("REC OFF");
    expect(rec?.tone).toBe("motion");
    expect(rec?.hiddenCount).toBe(1);
  });

  test("motion alone surfaces as an amber chip", () => {
    expect(resolveTileException({ motion: true })).toEqual({
      kind: "motion",
      label: "MOTION",
      tone: "motion",
      hiddenCount: 0,
    });
  });

  test("record undefined is not treated as off", () => {
    expect(resolveTileException({ recordEnabled: undefined })).toBeNull();
  });
});

describe("resolveTileIdentityTone", () => {
  test("time-shift wins over fps state", () => {
    expect(resolveTileIdentityTone({ timeShifted: true, cameraFps: 0, hasStats: true })).toBe(
      "rewind"
    );
  });
  test("zero fps with stats is offline, missing stats is optimistic", () => {
    expect(resolveTileIdentityTone({ timeShifted: false, cameraFps: 0, hasStats: true })).toBe(
      "offline"
    );
    expect(resolveTileIdentityTone({ timeShifted: false, hasStats: false })).toBe("live");
    expect(resolveTileIdentityTone({ timeShifted: false, cameraFps: 5, hasStats: true })).toBe(
      "live"
    );
  });
});

describe("snapshot + telemetry helpers", () => {
  test("snapshot url targets the unified endpoint and encodes the id", () => {
    expect(buildCameraSnapshotUrl("escritorio")).toBe("/api/escritorio/snapshot.jpg");
    expect(buildCameraSnapshotUrl("a b/c")).toBe("/api/a%20b%2Fc/snapshot.jpg");
  });

  test("telemetry rows format resolution, fps and image mode", () => {
    const rows = formatCameraTelemetryRows({
      detectWidth: 1280,
      detectHeight: 720,
      cameraFps: 5,
      detectionFps: 7.63,
      nightMode: true,
    });
    expect(rows.map((r) => r.value)).toEqual(["1280×720", "5.0 FPS", "7.6 FPS", "IR night"]);
    const pt = formatCameraTelemetryRows({ isPt: true });
    expect(pt.map((r) => r.value)).toEqual(["—", "—", "—", "Dia"]);
    expect(pt[0].label).toBe("Resolução");
  });

  test("runtime flags default to on and respect explicit config", () => {
    expect(resolveCameraRuntimeFlags()).toEqual({
      enabled: true,
      detect: true,
      record: true,
      alerts: true,
    });
    expect(resolveCameraRuntimeFlags({ record: false }, false)).toEqual({
      enabled: false,
      detect: true,
      record: false,
      alerts: true,
    });
  });
});

describe("mobile shell", () => {
  test("compact viewport boundary is 768px exclusive", () => {
    expect(isCompactViewport(390)).toBe(true);
    expect(isCompactViewport(767)).toBe(true);
    expect(isCompactViewport(768)).toBe(false);
    expect(isCompactViewport(0)).toBe(false);
    expect(isCompactViewport(Number.NaN)).toBe(false);
  });

  test("admin-only tabs never leak to viewers", () => {
    expect(resolveMobileNavTabs("viewer")).toEqual({
      primary: ["live", "sync", "review", "exports"],
      overflow: ["explore"],
    });
    expect(resolveMobileNavTabs("admin").overflow).toEqual(["explore", "zones", "system"]);
  });

  test("badge counts cap at 99+", () => {
    expect(formatBadgeCount(0)).toBe("");
    expect(formatBadgeCount(7)).toBe("7");
    expect(formatBadgeCount(200)).toBe("99+");
  });
});

import {
  filterCompactStageDescriptors,
  resolveCompactStageContainerStyle,
  sanitizeCompactGridColumns,
} from "../utils/liveTileHud";

describe("compact live stage", () => {
  test("matrix becomes a scrolling 1- or 2-column list", () => {
    expect(resolveCompactStageContainerStyle("2x2", 1).gridTemplateColumns).toBe("minmax(0, 1fr)");
    expect(resolveCompactStageContainerStyle("2x2", 2).gridTemplateColumns).toBe(
      "repeat(2, minmax(0, 1fr))"
    );
    expect(resolveCompactStageContainerStyle("2x2", 1).overflowY).toBe("auto");
    expect(resolveCompactStageContainerStyle("focus", 2).gridTemplateColumns).toBe(
      "minmax(0, 1fr)"
    );
  });

  test("focus and spotlight drop dock tiles so phones decode one stream", () => {
    const d = [{ role: "hero" }, { role: "dock" }, { role: "dock" }];
    expect(filterCompactStageDescriptors("focus", d)).toEqual([{ role: "hero" }]);
    expect(filterCompactStageDescriptors("spotlight", d)).toEqual([{ role: "hero" }]);
    const g = [{ role: "grid" }, { role: "grid" }];
    expect(filterCompactStageDescriptors("2x2", g)).toEqual(g);
  });

  test("column preference sanitizes persisted values", () => {
    expect(sanitizeCompactGridColumns("2")).toBe(2);
    expect(sanitizeCompactGridColumns(2)).toBe(2);
    expect(sanitizeCompactGridColumns("3")).toBe(1);
    expect(sanitizeCompactGridColumns(null)).toBe(1);
  });
});
