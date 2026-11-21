import { describe, expect, test } from "bun:test";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { NinjaStatsResponse, StorageBreakdownResponse } from "../types/nvr";
import {
  SystemWorkspace,
  buildCameraGranularRows,
  computeStorageStackSegments,
  formatRetentionSpan,
  formatStorageBytes,
  formatStorageShortBytes,
} from "../views/SystemWorkspace";

describe("Granular storage breakdown & cleanup helpers and SystemWorkspace UI", () => {
  test("formatStorageBytes, formatStorageShortBytes, and formatRetentionSpan handle scales and malformed values safely", () => {
    expect(formatStorageBytes(0)).toBe("0 B");
    expect(formatStorageBytes(-500)).toBe("0 B");
    expect(formatStorageBytes(Number.NaN)).toBe("0 B");
    expect(formatStorageBytes(Number.POSITIVE_INFINITY)).toBe("0 B");
    expect(formatStorageBytes(512)).toBe("512 B");
    expect(formatStorageBytes(15 * 1024)).toBe("15.0 KB");
    expect(formatStorageBytes(42.5 * 1024 * 1024)).toBe("42.5 MB");
    expect(formatStorageBytes(104.78 * 1024 * 1024 * 1024)).toBe("104.78 GB");
    expect(formatStorageBytes(2.5 * 1024 * 1024 * 1024 * 1024)).toBe("2.50 TB");

    expect(formatStorageShortBytes(0)).toBe("0 B");
    expect(formatStorageShortBytes(20 * 1024)).toBe("20K");
    expect(formatStorageShortBytes(420 * 1024 * 1024)).toBe("420M");
    expect(formatStorageShortBytes(55.2 * 1024 * 1024 * 1024)).toBe("55.2G");

    const nowSec = 1_750_000_000;
    expect(formatRetentionSpan(null, nowSec)).toBeNull();
    expect(formatRetentionSpan(Number.NaN, nowSec)).toBeNull();
    expect(formatRetentionSpan(nowSec + 60, nowSec)).toBeNull();
    expect(formatRetentionSpan(nowSec - 6 * 3600, nowSec)).toBe("6.0h");
    expect(formatRetentionSpan(nowSec - 7.7 * 86400, nowSec)).toBe("7.7d");
  });

  test("computeStorageStackSegments and buildCameraGranularRows produce normalized granular views", () => {
    const mockBreakdown: StorageBreakdownResponse = {
      generated_at: 1_750_000_000,
      total_tracked_bytes: 110 * 1024 * 1024 * 1024,
      volume_total_bytes: 500 * 1024 * 1024 * 1024,
      volume_used_bytes: 220 * 1024 * 1024 * 1024,
      volume_free_bytes: 280 * 1024 * 1024 * 1024,
      scoped: false,
      categories: {
        recordings_continuous: { bytes: 30 * 1024 * 1024 * 1024, count: 5000 },
        recordings_motion: { bytes: 25 * 1024 * 1024 * 1024, count: 0 },
        recordings_events: { bytes: 50 * 1024 * 1024 * 1024, count: 0 },
        recordings_retained_excess: { bytes: 75 * 1024 * 1024 * 1024, count: 3200 },
        recordings_compacted: { bytes: 0, count: 0 },
        events_media: { bytes: 512 * 1024 * 1024, count: 1420 },
        pinned_events_count: 3,
        reviews_media: { bytes: 128 * 1024 * 1024, count: 600 },
        previews_media: { bytes: 256 * 1024 * 1024, count: 95 },
        exports_media: { bytes: 1024 * 1024 * 1024, count: 8 },
        share_artifacts_count: 2,
        dataset_and_models: { bytes: 256 * 1024 * 1024, count: 40 },
        dataset_samples_count: 38,
        models_files_count: 2,
        database: { bytes: 64 * 1024 * 1024, count: 2 },
        database_main_bytes: 48 * 1024 * 1024,
        database_wal_bytes: 16 * 1024 * 1024,
        timeline_rows_count: 4200,
      },
      cameras: {
        escritorio: {
          camera: "escritorio",
          configured: true,
          total_bytes: 104.78 * 1024 * 1024 * 1024,
          recordings_bytes: 104 * 1024 * 1024 * 1024,
          continuous_bytes: 18 * 1024 * 1024 * 1024,
          motion_bytes: 31 * 1024 * 1024 * 1024,
          event_segments_bytes: 55 * 1024 * 1024 * 1024,
          compacted_bytes: 0,
          retained_past_continuous_bytes: 77 * 1024 * 1024 * 1024,
          retained_past_continuous_count: 3100,
          segments_count: 4800,
          oldest_segment_ts: 1_750_000_000 - 7.7 * 86400,
          newest_segment_ts: 1_750_000_000,
          events_count: 1200,
          pinned_events_count: 2,
          events_media_bytes: 450 * 1024 * 1024,
          reviews_count: 500,
          reviews_media_bytes: 100 * 1024 * 1024,
          previews_bytes: 200 * 1024 * 1024,
          previews_files_count: 80,
          exports_bytes: 0,
          exports_count: 0,
          bandwidth_mb_hr: 566.6,
          estimated_retention_days: 4.9,
          continuous_days: 2,
          event_retention_days: 14,
        },
      },
    };

    const segs = computeStorageStackSegments(mockBreakdown, true);
    expect(segs.length).toBe(6);
    const totalPct = segs.reduce((a, b) => a + b.pct, 0);
    expect(totalPct).toBeGreaterThan(99);
    expect(totalPct).toBeLessThanOrEqual(101);

    const rows = buildCameraGranularRows(null, mockBreakdown);
    expect(rows.length).toBe(1);
    expect(rows[0].camera).toBe("escritorio");
    expect(rows[0].events_count).toBe(1200);
  });

  test("SystemWorkspace renders full granular category breakdown, stacked composition bar, and per-camera Clear Events actions", () => {
    const mockStats: NinjaStatsResponse = {
      cameras: {},
      detectors: {},
      camera_fps: 30,
      process_fps: 10,
      skipped_fps: 0,
      detection_fps: 3.6,
      cpu_usages: {},
      processes: {},
      service: {
        uptime: 86400,
        version: "2.2.0",
        latest_version: "2.2.0",
        estimated_retention_days: 4.9,
        camera_storage: {
          cozinha: {
            usage: 26_644,
            bandwidth: 503.7,
            estimated_retention_days: 4.9,
            continuous_mb: 20_000,
            motion_mb: 4_000,
            event_segments_mb: 2_644,
            events_count: 85,
            pinned_events_count: 0,
          },
        },
        storage: {},
        last_updated: 1_750_000_000,
      },
    };
    const fallbackRows = buildCameraGranularRows(mockStats, null);
    expect(fallbackRows.length).toBe(1);
    expect(fallbackRows[0].camera).toBe("cozinha");
    expect(fallbackRows[0].events_count).toBe(85);
    expect(formatStorageBytes(fallbackRows[0].total_bytes)).toBe("26.02 GB");

    const sampleBreakdown: StorageBreakdownResponse = {
      generated_at: 1_750_000_000,
      total_tracked_bytes: 132 * 1024 * 1024 * 1024,
      volume_total_bytes: 500 * 1024 * 1024 * 1024,
      volume_used_bytes: 230 * 1024 * 1024 * 1024,
      volume_free_bytes: 270 * 1024 * 1024 * 1024,
      scoped: false,
      categories: {
        recordings_continuous: { bytes: 38 * 1024 * 1024 * 1024, count: 6200 },
        recordings_motion: { bytes: 35 * 1024 * 1024 * 1024, count: 0 },
        recordings_events: { bytes: 57 * 1024 * 1024 * 1024, count: 0 },
        recordings_retained_excess: { bytes: 77 * 1024 * 1024 * 1024, count: 3100 },
        recordings_compacted: { bytes: 0, count: 0 },
        events_media: { bytes: 420 * 1024 * 1024, count: 1842 },
        pinned_events_count: 3,
        reviews_media: { bytes: 110 * 1024 * 1024, count: 520 },
        previews_media: { bytes: 310 * 1024 * 1024, count: 96 },
        exports_media: { bytes: 640 * 1024 * 1024, count: 5 },
        share_artifacts_count: 1,
        dataset_and_models: { bytes: 180 * 1024 * 1024, count: 25 },
        dataset_samples_count: 23,
        models_files_count: 2,
        database: { bytes: 72 * 1024 * 1024, count: 2 },
        database_main_bytes: 56 * 1024 * 1024,
        database_wal_bytes: 16 * 1024 * 1024,
        timeline_rows_count: 9800,
      },
      cameras: {
        escritorio: {
          camera: "escritorio",
          configured: true,
          total_bytes: 104.78 * 1024 * 1024 * 1024,
          recordings_bytes: 104.1 * 1024 * 1024 * 1024,
          continuous_bytes: 18.4 * 1024 * 1024 * 1024,
          motion_bytes: 31.2 * 1024 * 1024 * 1024,
          event_segments_bytes: 54.5 * 1024 * 1024 * 1024,
          compacted_bytes: 0,
          retained_past_continuous_bytes: 77 * 1024 * 1024 * 1024,
          retained_past_continuous_count: 3100,
          segments_count: 4800,
          oldest_segment_ts: 1_750_000_000 - 7.7 * 86400,
          newest_segment_ts: 1_750_000_000,
          events_count: 1842,
          pinned_events_count: 3,
          events_media_bytes: 420 * 1024 * 1024,
          reviews_count: 520,
          reviews_media_bytes: 110 * 1024 * 1024,
          previews_bytes: 150 * 1024 * 1024,
          previews_files_count: 60,
          exports_bytes: 0,
          exports_count: 0,
          bandwidth_mb_hr: 566.6,
          estimated_retention_days: 4.9,
          continuous_days: 2,
          event_retention_days: 14,
        },
        legacy_cam: {
          camera: "legacy_cam",
          configured: false,
          total_bytes: 2 * 1024 * 1024 * 1024,
          recordings_bytes: 2 * 1024 * 1024 * 1024,
          continuous_bytes: 2 * 1024 * 1024 * 1024,
          motion_bytes: 0,
          event_segments_bytes: 0,
          compacted_bytes: 0,
          retained_past_continuous_bytes: 0,
          retained_past_continuous_count: 0,
          segments_count: 100,
          oldest_segment_ts: null,
          newest_segment_ts: null,
          events_count: 0,
          pinned_events_count: 0,
          events_media_bytes: 0,
          reviews_count: 0,
          reviews_media_bytes: 0,
          previews_bytes: 0,
          previews_files_count: 0,
          exports_bytes: 0,
          exports_count: 0,
          bandwidth_mb_hr: 0,
          estimated_retention_days: 14,
          continuous_days: 7,
          event_retention_days: 7,
        },
      },
    };

    const html = renderToStaticMarkup(
      React.createElement(SystemWorkspace, {
        cameraNames: ["escritorio"],
        logService: "ninjanvr",
        setLogService: () => {},
        logLines: [],
        logSearch: "",
        setLogSearch: () => {},
        logPaused: false,
        setLogPaused: () => {},
        notify: () => {},
        initialBreakdown: sampleBreakdown,
      })
    );
    expect(html).not.toContain("NaN");
    expect(html).toContain("escritorio");
    expect(html).toContain("104.78 GB");
    expect(html).toContain("1842");
    expect(html).toContain("3 ★");
    expect(html).toMatch(/ORPHAN|LEGADO/);
    expect(html).toMatch(/Clear Events|Limpar Eventos/);
    expect(html).toMatch(/Prune Excess|Podar Excedente/);
    expect(html).toMatch(/Clear Previews|Limpar Previews/);
    expect(html).toMatch(/Clear Exports|Limpar Exports/);
    expect(html).toMatch(/Checkpoint WAL|Compactar WAL/);
    expect(html).toMatch(/Protected \(Studio\)|Protegido \(Studio\)/);
  });
});
