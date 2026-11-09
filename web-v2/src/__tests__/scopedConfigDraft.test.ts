import { describe, expect, test } from "bun:test";
import type { CameraConfig, NinjaConfigResponse } from "../types/nvr";
import {
  applyStructuredDraftMutation,
  createScopedConfigLens,
  DEFAULT_ATTRIBUTE_LABELS,
  mutateScopedConfig,
  synthesizeDraftYamlIfNeeded,
} from "../utils/scopedConfigDraft";

function makeMinimalConfig(): NinjaConfigResponse {
  const bareCamera = {
    enabled: true,
    motion: { enabled: true, threshold: 30, contour_area: 10, mask: [] },
    zones: {},
  } as unknown as CameraConfig;

  return {
    version: "0.9.0",
    safe_mode: false,
    cameras: {
      front_door: JSON.parse(JSON.stringify(bareCamera)) as CameraConfig,
      backyard: JSON.parse(JSON.stringify(bareCamera)) as CameraConfig,
    },
  };
}

describe("ScopedConfigLens — Global vs Per-Camera Scoped Reads & Mutations", () => {
  test("reads and mutates detect in global scope vs per-camera scope with automatic default hydration", () => {
    const cfg = makeMinimalConfig();
    expect(cfg.detect).toBeUndefined();
    expect(cfg.cameras.front_door.detect).toBeUndefined();

    // 1. Global scope read hydrates defaults without mutating input
    const globalLens = createScopedConfigLens(cfg, "global", "front_door");
    expect(globalLens.isGlobal).toBe(true);
    expect(globalLens.readDetect()).toEqual({
      enabled: true,
      width: 1280,
      height: 720,
      fps: 5,
      min_initialized: 2,
      max_disappeared: 25,
      stationary: { interval: 50, threshold: 50, classifier: true },
    });

    // Mutate global detect
    globalLens.mutateDetect(cfg, { fps: 8, min_initialized: 1 });
    expect(cfg.detect?.fps).toBe(8);
    expect(cfg.detect?.min_initialized).toBe(1);
    expect(cfg.cameras.front_door.detect).toBeUndefined();

    // 2. Per-camera scope inherits global detect when cam.detect is undefined
    const camLens = createScopedConfigLens(cfg, "front_door", "front_door");
    expect(camLens.isGlobal).toBe(false);
    expect(camLens.activeCamera).toBe("front_door");
    expect(camLens.readDetect().fps).toBe(8);
    expect(camLens.readDetect().min_initialized).toBe(1);

    // Mutating camera scope automatically hydrates cam.detect without touching global
    camLens.mutateDetect(cfg, (det) => {
      det.width = 1920;
      det.height = 1080;
      det.fps = 10;
    });
    expect(cfg.cameras.front_door.detect).toBeDefined();
    expect(cfg.cameras.front_door.detect.width).toBe(1920);
    expect(cfg.cameras.front_door.detect.height).toBe(1080);
    expect(cfg.cameras.front_door.detect.fps).toBe(10);
    expect(cfg.detect?.fps).toBe(8);
    expect(cfg.cameras.backyard.detect).toBeUndefined();
  });

  test("reads and mutates objects (tracked labels, attributes, per-label filters, excluded sub-labels) across global and camera scopes", () => {
    const cfg = makeMinimalConfig();
    const globalLens = createScopedConfigLens(cfg, "global", "front_door");

    // Default tracked labels & attributes
    expect(globalLens.readTrackedLabels()).toEqual(["person"]);
    expect(globalLens.readAttributeLabels()).toEqual(DEFAULT_ATTRIBUTE_LABELS);

    // Prevent removing the last remaining tracked label by default
    expect(globalLens.toggleTrackedLabel(cfg, "person")).toEqual(["person"]);
    // Add and remove additional labels in global scope
    expect(globalLens.toggleTrackedLabel(cfg, "car")).toEqual(["person", "car"]);
    expect(cfg.objects?.track).toEqual(["person", "car"]);

    // Toggle attribute label in global scope
    const afterAttrToggle = globalLens.toggleAttributeLabel(cfg, "face");
    expect(afterAttrToggle.includes("face")).toBe(false);
    expect(cfg.objects?.attribute_labels?.includes("face")).toBe(false);

    // Update object filter by key/value and enforce min_score <= threshold invariant
    globalLens.updateObjectFilter(cfg, "person", "min_score", 0.8);
    expect(cfg.objects?.filters?.person?.min_score).toBe(0.8);
    expect(cfg.objects?.filters?.person?.threshold).toBe(0.8);

    // Camera scope inherits global objects until overridden
    const camLens = createScopedConfigLens(cfg, "front_door", "front_door");
    expect(camLens.readTrackedLabels()).toEqual(["person", "car"]);
    expect(camLens.readAttributeLabels().includes("face")).toBe(false);
    expect(camLens.readObjectFilter("person").threshold).toBe(0.8);
    expect(cfg.cameras.front_door.objects).toBeUndefined();

    // Mutating camera objects hydrates cam.objects without mutating global
    camLens.toggleTrackedLabel(cfg, "dog");
    expect(cfg.cameras.front_door.objects?.track).toEqual([
      "person",
      "car",
      "dog",
    ]);
    expect(cfg.objects?.track).toEqual(["person", "car"]);

    // Re-enable "face" attribute only on front_door
    camLens.toggleAttributeLabel(cfg, "face");
    expect(cfg.cameras.front_door.objects?.attribute_labels?.includes("face")).toBe(
      true
    );
    expect(cfg.objects?.attribute_labels?.includes("face")).toBe(false);

    // Update object filter on front_door with threshold lower than min_score
    camLens.updateObjectFilter(cfg, "person", { threshold: 0.45 });
    expect(cfg.cameras.front_door.objects?.filters?.person?.threshold).toBe(0.45);
    expect(cfg.cameras.front_door.objects?.filters?.person?.min_score).toBe(0.45);
    expect(cfg.objects?.filters?.person?.threshold).toBe(0.8);

    // Excluded sub-labels (both 2-arg and 3-arg forms)
    expect(globalLens.readExcludedSubLabels()).toEqual([]);
    globalLens.toggleExcludedSubLabel(cfg, "person", "Bruno");
    expect(cfg.review?.alerts.exclude_sub_labels).toEqual(["Bruno"]);
    expect(camLens.readExcludedSubLabels()).toEqual(["Bruno"]);

    // Camera override of excluded sub-labels hydrates cam.review
    expect(cfg.cameras.front_door.review).toBeUndefined();
    camLens.toggleExcludedSubLabel(cfg, "Alice");
    expect(cfg.cameras.front_door.review?.alerts.exclude_sub_labels).toEqual([
      "Bruno",
      "Alice",
    ]);
    expect(cfg.review?.alerts.exclude_sub_labels).toEqual(["Bruno"]);

    // Toggling case-insensitively removes "bruno" on camera scope only
    camLens.toggleExcludedSubLabel(cfg, "person", "bruno");
    expect(cfg.cameras.front_door.review?.alerts.exclude_sub_labels).toEqual([
      "Alice",
    ]);
    expect(cfg.review?.alerts.exclude_sub_labels).toEqual(["Bruno"]);
  });

  test("reads and mutates record, snapshots, snapshot selection, review, and audio with automatic camera hydration", () => {
    const cfg = makeMinimalConfig();
    expect(cfg.cameras.front_door.record).toBeUndefined();
    expect(cfg.cameras.front_door.snapshots).toBeUndefined();
    expect(cfg.cameras.front_door.review).toBeUndefined();
    expect(cfg.cameras.front_door.audio).toBeUndefined();

    const globalLens = createScopedConfigLens(cfg, "global", "front_door");
    const camLens = createScopedConfigLens(cfg, "front_door", "front_door");

    // 1. Record
    expect(globalLens.readRecord().continuous?.days).toBe(14);
    globalLens.mutateRecord(cfg, (r) => {
      r.continuous = { days: 7 };
      r.alerts!.retain.days = 30;
    });
    expect(cfg.record?.continuous?.days).toBe(7);
    expect(camLens.readRecord().continuous?.days).toBe(7);
    expect(camLens.readRecord().alerts?.retain.days).toBe(30);

    camLens.mutateRecord(cfg, (r) => {
      r.continuous = { days: 3 };
      r.continuous_substream = true;
    });
    expect(cfg.cameras.front_door.record.continuous?.days).toBe(3);
    expect(cfg.cameras.front_door.record.continuous_substream).toBe(true);
    expect(cfg.cameras.front_door.record.alerts?.retain.days).toBe(30);
    expect(cfg.record?.continuous?.days).toBe(7);

    // 2. Snapshots & Snapshot Selection
    expect(globalLens.readSnapshotSelection()).toBe("best_in_zone");
    globalLens.mutateSnapshotSelection(cfg, "best_attribute");
    expect(cfg.snapshots?.selection).toBe("best_attribute");
    expect(camLens.readSnapshotSelection()).toBe("best_attribute");

    camLens.mutateSnapshots(cfg, (s) => {
      s.quality = 90;
      s.crop = true;
    });
    expect(cfg.cameras.front_door.snapshots.quality).toBe(90);
    expect(cfg.cameras.front_door.snapshots.crop).toBe(true);
    expect(cfg.cameras.front_door.snapshots.selection).toBe("best_attribute");
    expect(cfg.snapshots?.quality).toBeUndefined();

    // 3. Review
    globalLens.mutateReview(cfg, (rv) => {
      rv.alerts.cutoff_time = 60;
    });
    expect(camLens.readReview().alerts.cutoff_time).toBe(60);
    camLens.mutateReview(cfg, (rv) => {
      rv.alerts.required_zones = ["porch"];
      rv.detections.cutoff_time = 45;
    });
    expect(cfg.cameras.front_door.review?.alerts.required_zones).toEqual(["porch"]);
    expect(cfg.cameras.front_door.review?.alerts.cutoff_time).toBe(60);
    expect(cfg.cameras.front_door.review?.detections.cutoff_time).toBe(45);
    expect(cfg.review?.alerts.required_zones).toEqual([]);

    // 4. Audio
    expect(globalLens.readAudio().enabled).toBe(false);
    expect(globalLens.readAudio().min_volume_dbfs).toBe(-60);
    globalLens.mutateAudio(cfg, (au) => {
      au.enabled = true;
      au.min_volume_dbfs = -58;
    });
    expect(camLens.readAudio().enabled).toBe(true);
    expect(camLens.readAudio().min_volume_dbfs).toBe(-58);

    camLens.mutateAudio(cfg, (au) => {
      au.min_volume_dbfs = -38;
    });
    expect(cfg.cameras.front_door.audio?.enabled).toBe(true);
    expect(cfg.cameras.front_door.audio?.min_volume_dbfs).toBe(-38);
    expect(cfg.audio?.min_volume_dbfs).toBe(-58);
  });

  test("applySectionToAllCameras copies the active scope configuration to all cameras with independent clones", () => {
    const cfg = makeMinimalConfig();

    mutateScopedConfig(cfg, "front_door", "front_door", (lens, draft) => {
      lens.mutateDetect(draft, { fps: 12, width: 1920, height: 1080 });
      lens.mutateRecord(draft, (r) => {
        r.continuous = { days: 21 };
      });
      lens.toggleTrackedLabel(draft, "truck");
      lens.mutateSnapshots(draft, { quality: 85 });
      lens.mutateReview(draft, (rv) => {
        rv.alerts.cutoff_time = 90;
      });
      lens.mutateAudio(draft, { enabled: true, min_volume_dbfs: -50 });

      lens.applySectionToAllCameras(draft, "detect");
      lens.applySectionToAllCameras(draft, "record");
      lens.applySectionToAllCameras(draft, "objects");
      lens.applySectionToAllCameras(draft, "snapshots");
      lens.applySectionToAllCameras(draft, "review");
      lens.applySectionToAllCameras(draft, "audio");
    });

    expect(cfg.cameras.backyard.detect.fps).toBe(12);
    expect(cfg.cameras.backyard.record.continuous?.days).toBe(21);
    expect(cfg.cameras.backyard.objects?.track).toEqual(["person", "truck"]);
    expect(cfg.cameras.backyard.snapshots.quality).toBe(85);
    expect(cfg.cameras.backyard.review?.alerts.cutoff_time).toBe(90);
    expect(cfg.cameras.backyard.audio?.min_volume_dbfs).toBe(-50);

    // Verify deep clone independence between cameras
    cfg.cameras.front_door.record.continuous!.days = 99;
    cfg.cameras.front_door.objects!.track.push("cat");
    expect(cfg.cameras.backyard.record.continuous?.days).toBe(21);
    expect(cfg.cameras.backyard.objects?.track).toEqual(["person", "truck"]);
  });

  test("applyStructuredDraftMutation defers YAML synthesis on visual tabs and synthesizes lazily on YAML tab or save", () => {
    const initialCfg = makeMinimalConfig();
    initialCfg.cameras.front_door.privacy_mask = [
      "0.2,0.2,0.2,0.2,0.2,0.2", // degenerate polygon should be sanitized
    ];

    // 1. Visual tab mutation (syncYamlImmediately: false) skips buildCleanYamlFromDraft
    const visualStep = applyStructuredDraftMutation(
      initialCfg,
      (next) => {
        const lens = createScopedConfigLens(next, "front_door", "front_door");
        lens.mutateDetect(next, { fps: 9 });
      },
      {
        syncYamlImmediately: false,
        baselineYaml: "version: '0.9.0'\n",
      }
    );

    expect(visualStep.yamlStale).toBe(true);
    expect(visualStep.rawYaml).toBeNull();
    expect(visualStep.rejectedGeometryCount).toBe(1);
    expect(visualStep.draft.cameras.front_door.detect.fps).toBe(9);
    expect(visualStep.draft.cameras.front_door.privacy_mask).toEqual([]);

    // 2. Switching to YAML tab lazily synthesizes the YAML string once
    const lazySynced = synthesizeDraftYamlIfNeeded(
      visualStep.draft,
      "version: '0.9.0'\n",
      "version: '0.9.0'\n",
      visualStep.yamlStale
    );
    expect(lazySynced.yamlStale).toBe(false);
    expect(lazySynced.rawYaml).toContain("fps: 9");

    // Repeated call when yamlStale is false is a no-op
    const noOpSynced = synthesizeDraftYamlIfNeeded(
      visualStep.draft,
      "version: '0.9.0'\n",
      lazySynced.rawYaml,
      lazySynced.yamlStale
    );
    expect(noOpSynced.rawYaml).toBe(lazySynced.rawYaml);
    expect(noOpSynced.yamlStale).toBe(false);

    // 3. Direct mutation while on YAML tab (syncYamlImmediately: true) synthesizes immediately
    const yamlTabStep = applyStructuredDraftMutation(
      visualStep.draft,
      (next) => {
        const lens = createScopedConfigLens(next, "global", "front_door");
        lens.mutateAudio(next, { enabled: true, min_volume_dbfs: -42 });
      },
      {
        syncYamlImmediately: true,
        baselineYaml: lazySynced.rawYaml,
      }
    );
    expect(yamlTabStep.yamlStale).toBe(false);
    expect(yamlTabStep.rawYaml).not.toBeNull();
    expect(yamlTabStep.rawYaml).toContain("min_volume_dbfs: -42");
  });
});
