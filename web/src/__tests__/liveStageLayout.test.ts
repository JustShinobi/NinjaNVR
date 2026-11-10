import { describe, expect, test } from "bun:test";
import {
  computeLiveStageDescriptors,
  DEFAULT_MAX_DOCK_LIVE_DECODERS,
  resolveEffectiveSpotlightHeroCam,
  resolvePersistentStageContainerStyle,
  resolvePersistentTileStyle,
  resolveTileClickAction,
  shouldRenderLiveAlertRibbon,
  shouldRenderSpotlightTimeline,
} from "../utils/liveStageLayout";

describe("liveStageLayout (#92 [UX-17])", () => {
  const fleet4 = ["escritorio", "sala", "garagem", "varanda"];

  test("compute_live_stage_descriptors_preserves_stable_camera_keys_across_2x2_focus_single_and_spotlight", () => {
    // 1. 2x2 Grid mode: all 4 cameras in canonical order with role="grid"
    const gridDesc = computeLiveStageDescriptors({
      liveLayout: "2x2",
      visibleMatrixCams: fleet4,
      groupCams: fleet4,
      selectedCamera: "sala",
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: null,
    });
    expect(gridDesc.map((d) => d.camera)).toEqual(fleet4);
    expect(gridDesc.every((d) => d.role === "grid")).toBe(true);
    expect(gridDesc.every((d) => d.useMainStream === false)).toBe(true);
    expect(gridDesc.find((d) => d.camera === "sala")?.isTimelineSelected).toBe(true);

    // 2. Transition 2x2 -> Focus Stage with selectedCamera="sala":
    // Exact same camera keys in the exact same array order!
    // "sala" becomes role="hero" (useMainStream=true), others become role="dock" (useMainStream=false)
    const focusDesc = computeLiveStageDescriptors({
      liveLayout: "focus",
      visibleMatrixCams: fleet4,
      groupCams: fleet4,
      selectedCamera: "sala",
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: null,
    });
    expect(focusDesc.map((d) => d.camera)).toEqual(fleet4);
    const focusHero = focusDesc.find((d) => d.camera === "sala")!;
    expect(focusHero.role).toBe("hero");
    expect(focusHero.useMainStream).toBe(true);
    expect(focusHero.dockTotal).toBe(3);
    const focusDocks = focusDesc.filter((d) => d.camera !== "sala");
    expect(focusDocks.every((d) => d.role === "dock" && !d.useMainStream)).toBe(true);
    expect(focusDocks.map((d) => d.dockIndex)).toEqual([0, 1, 2]);

    // 3. Transition Focus -> Spotlight with spotlightHeroCam="garagem":
    // Exact same camera keys in the exact same array order!
    const spotDesc = computeLiveStageDescriptors({
      liveLayout: "spotlight",
      visibleMatrixCams: fleet4,
      groupCams: fleet4,
      selectedCamera: "sala",
      spotlightHeroCam: "garagem",
      spotlightPromotedCams: ["garagem"],
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: null,
    });
    expect(spotDesc.map((d) => d.camera)).toEqual(fleet4);
    const spotHero = spotDesc.find((d) => d.camera === "garagem")!;
    expect(spotHero.role).toBe("hero");
    expect(spotHero.useMainStream).toBe(true);
    expect(spotHero.isSpotlightHeld).toBe(true);

    // 4. Transition -> Single Full mode:
    const singleDesc = computeLiveStageDescriptors({
      liveLayout: "single",
      visibleMatrixCams: fleet4,
      groupCams: fleet4,
      selectedCamera: "sala",
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: null,
    });
    expect(singleDesc.length).toBe(1);
    expect(singleDesc[0].camera).toBe("sala");
    expect(singleDesc[0].role).toBe("hero");
    expect(singleDesc[0].useMainStream).toBe(true);
  });

  test("focus_and_spotlight_dock_swap_promotes_dock_camera_to_hero_in_place", () => {
    const beforeSwap = computeLiveStageDescriptors({
      liveLayout: "focus",
      visibleMatrixCams: fleet4,
      groupCams: fleet4,
      selectedCamera: "escritorio",
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: null,
    });
    const afterSwap = computeLiveStageDescriptors({
      liveLayout: "focus",
      visibleMatrixCams: fleet4,
      groupCams: fleet4,
      selectedCamera: "varanda",
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: null,
    });

    // Canonical camera key order is identical so React never unmounts any child node
    expect(beforeSwap.map((d) => d.camera)).toEqual(afterSwap.map((d) => d.camera));

    // Before: escritorio is hero (gridColumn 1, gridRow "1 / span 3"), varanda is dockIndex 2 (gridColumn 2, gridRow 3)
    const escBefore = beforeSwap.find((d) => d.camera === "escritorio")!;
    const varBefore = beforeSwap.find((d) => d.camera === "varanda")!;
    expect(escBefore.role).toBe("hero");
    expect(varBefore.role).toBe("dock");
    expect(resolvePersistentTileStyle("focus", escBefore)).toEqual({
      gridColumn: 1,
      gridRow: "1 / span 3",
    });
    expect(resolvePersistentTileStyle("focus", varBefore)?.gridColumn).toBe(2);
    expect(resolvePersistentTileStyle("focus", varBefore)?.gridRow).toBe(3);

    // After: varanda is hero (gridColumn 1, gridRow "1 / span 3"), escritorio is dockIndex 0 (gridColumn 2, gridRow 1)
    const escAfter = afterSwap.find((d) => d.camera === "escritorio")!;
    const varAfter = afterSwap.find((d) => d.camera === "varanda")!;
    expect(varAfter.role).toBe("hero");
    expect(varAfter.useMainStream).toBe(true);
    expect(escAfter.role).toBe("dock");
    expect(escAfter.useMainStream).toBe(false);
    expect(resolvePersistentTileStyle("focus", varAfter)).toEqual({
      gridColumn: 1,
      gridRow: "1 / span 3",
    });
    expect(resolvePersistentTileStyle("focus", escAfter)?.gridColumn).toBe(2);
    expect(resolvePersistentTileStyle("focus", escAfter)?.gridRow).toBe(1);

    const containerStyle = resolvePersistentStageContainerStyle("focus", afterSwap, "2x2");
    expect(containerStyle?.gridTemplateColumns).toBe("minmax(0, 1fr) 310px");
  });

  test("focus_and_spotlight_enforce_max_dock_live_decoders_and_hover_override", () => {
    const fleet10 = Array.from({ length: 10 }, (_, i) => `cam_${i + 1}`);

    const descNoHover = computeLiveStageDescriptors({
      liveLayout: "focus",
      visibleMatrixCams: fleet10.slice(0, 4),
      groupCams: fleet10,
      selectedCamera: "cam_1",
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: null,
      maxDockLiveDecoders: DEFAULT_MAX_DOCK_LIVE_DECODERS,
    });
    const docksNoHover = descNoHover.filter((d) => d.role === "dock");
    expect(docksNoHover.length).toBe(9);
    // First 6 dock cameras use "mse", remaining 3 use "snapshot"
    expect(docksNoHover.slice(0, 6).every((d) => d.renderMode === "mse")).toBe(true);
    expect(docksNoHover.slice(6).every((d) => d.renderMode === "snapshot")).toBe(true);

    // Hovering over cam_9 (dockIndex 7) upgrades it from "snapshot" to "mse"
    const descHovered = computeLiveStageDescriptors({
      liveLayout: "focus",
      visibleMatrixCams: fleet10.slice(0, 4),
      groupCams: fleet10,
      selectedCamera: "cam_1",
      hoveredDockCam: "cam_9",
      useMainStream: false,
      timeShiftTs: null,
      maxDockLiveDecoders: DEFAULT_MAX_DOCK_LIVE_DECODERS,
    });
    expect(descHovered.find((d) => d.camera === "cam_9")?.renderMode).toBe("mse");
    expect(descHovered.find((d) => d.camera === "cam_10")?.renderMode).toBe("snapshot");
  });

  test("resolve_tile_click_action_handles_single_and_double_click_promotion", () => {
    // 1. Single-click on unselected grid tile selects timeline and pre-warms main stream without leaving 2x2
    const clickUnselected = resolveTileClickAction({
      liveLayout: "2x2",
      role: "grid",
      camera: "sala",
      selectedCamera: "escritorio",
      isDoubleClick: false,
    });
    expect(clickUnselected).toEqual({
      nextLayout: "2x2",
      nextSelectedCamera: "sala",
      promotedToFocus: false,
      shouldPrewarmMainStream: true,
    });

    // 2. Single-click on already-selected grid tile promotes directly to Focus Stage
    const clickSelected = resolveTileClickAction({
      liveLayout: "2x2",
      role: "grid",
      camera: "sala",
      selectedCamera: "sala",
      isDoubleClick: false,
    });
    expect(clickSelected).toEqual({
      nextLayout: "focus",
      nextSelectedCamera: "sala",
      promotedToFocus: true,
      shouldPrewarmMainStream: true,
    });

    // 3. Double-click on unselected grid tile promotes directly to Focus Stage in one gesture
    const dblClickUnselected = resolveTileClickAction({
      liveLayout: "2x2",
      role: "grid",
      camera: "garagem",
      selectedCamera: "escritorio",
      isDoubleClick: true,
    });
    expect(dblClickUnselected).toEqual({
      nextLayout: "focus",
      nextSelectedCamera: "garagem",
      promotedToFocus: true,
      shouldPrewarmMainStream: true,
    });

    // 4. Single-click on dock tile in Focus / Spotlight promotes to hero in-place
    const clickDock = resolveTileClickAction({
      liveLayout: "focus",
      role: "dock",
      camera: "varanda",
      selectedCamera: "escritorio",
      isDoubleClick: false,
    });
    expect(clickDock).toEqual({
      nextLayout: "focus",
      nextSelectedCamera: "varanda",
      promotedToFocus: true,
      shouldPrewarmMainStream: true,
    });

    // 5. Spotlight hero tile: single-click stays on spotlight, double-click promotes to Focus Stage
    const singleClickSpotlightHero = resolveTileClickAction({
      liveLayout: "spotlight",
      role: "hero",
      camera: "garagem",
      selectedCamera: "escritorio",
      isDoubleClick: false,
    });
    expect(singleClickSpotlightHero).toEqual({
      nextLayout: "spotlight",
      nextSelectedCamera: "garagem",
      promotedToFocus: false,
      shouldPrewarmMainStream: false,
    });

    const dblClickSpotlightHero = resolveTileClickAction({
      liveLayout: "spotlight",
      role: "hero",
      camera: "garagem",
      selectedCamera: "escritorio",
      isDoubleClick: true,
    });
    expect(dblClickSpotlightHero).toEqual({
      nextLayout: "focus",
      nextSelectedCamera: "garagem",
      promotedToFocus: true,
      shouldPrewarmMainStream: true,
    });
  });

  test("spotlight_hero_switches_to_vod_when_timeShiftTs_is_set_while_dock_tiles_remain_live_or_snapshot", () => {
    const fleet10 = Array.from({ length: 10 }, (_, i) => `cam_${i + 1}`);

    const liveSpot = computeLiveStageDescriptors({
      liveLayout: "spotlight",
      visibleMatrixCams: fleet10.slice(0, 4),
      groupCams: fleet10,
      selectedCamera: "cam_1",
      spotlightHeroCam: "cam_3",
      spotlightPromotedCams: ["cam_3"],
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: null,
      maxDockLiveDecoders: DEFAULT_MAX_DOCK_LIVE_DECODERS,
    });
    const liveHero = liveSpot.find((d) => d.camera === "cam_3")!;
    expect(liveHero.role).toBe("hero");
    expect(liveHero.renderMode).toBe("mse");
    expect(liveHero.isTimelineSelected).toBe(true);
    const liveDocks = liveSpot.filter((d) => d.camera !== "cam_3");
    expect(liveDocks.every((d) => d.isTimelineSelected === false)).toBe(true);

    const vodSpot = computeLiveStageDescriptors({
      liveLayout: "spotlight",
      visibleMatrixCams: fleet10.slice(0, 4),
      groupCams: fleet10,
      selectedCamera: "cam_1",
      spotlightHeroCam: "cam_3",
      spotlightPromotedCams: ["cam_3"],
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: 1700000000,
      maxDockLiveDecoders: DEFAULT_MAX_DOCK_LIVE_DECODERS,
    });
    expect(vodSpot.map((d) => d.camera)).toEqual(liveSpot.map((d) => d.camera));
    const vodHero = vodSpot.find((d) => d.camera === "cam_3")!;
    expect(vodHero.role).toBe("hero");
    expect(vodHero.renderMode).toBe("vod");
    expect(vodHero.isTimelineSelected).toBe(true);

    const vodDocks = vodSpot.filter((d) => d.camera !== "cam_3");
    expect(vodDocks.every((d) => d.isTimelineSelected === false)).toBe(true);
    expect(vodDocks.slice(0, 6).every((d) => d.renderMode === "mse")).toBe(true);
    expect(vodDocks.slice(6).every((d) => d.renderMode === "snapshot")).toBe(true);
  });

  test("simulated_100_camera_fleet_caps_focus_hero_span_and_4x4_grid_decoders", () => {
    const fleet100 = Array.from({ length: 100 }, (_, i) =>
      i < 4 ? `cam_${i + 1}` : `cam_${(i % 4) + 1}_sim_${String(i - 3).padStart(2, "0")}`
    );

    // 1. In Focus mode with 100 cameras, Hero gridRow span is capped at 6 (never 1 / span 99)
    const focus100 = computeLiveStageDescriptors({
      liveLayout: "focus",
      visibleMatrixCams: fleet100.slice(0, 16),
      groupCams: fleet100,
      selectedCamera: fleet100[0],
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: null,
    });
    const heroStyle = resolvePersistentTileStyle("focus", focus100[0]);
    expect(heroStyle?.gridRow).toBe("1 / span 6");
    const containerStyle = resolvePersistentStageContainerStyle("focus", focus100, "4x4");
    expect(containerStyle?.gridTemplateRows).toBe("repeat(6, minmax(0, 1fr))");
    expect(resolvePersistentTileStyle("focus", focus100[7])?.display).toBe("none");

    // 2. In 4x4 Grid (16 simulated tiles), MSE decoders are capped at 8 + hovered/selected
    const page2Cams = fleet100.slice(16, 32); // all *_sim_* clones
    const grid16 = computeLiveStageDescriptors({
      liveLayout: "2x2",
      visibleMatrixCams: page2Cams,
      groupCams: fleet100,
      selectedCamera: page2Cams[0],
      hoveredDockCam: page2Cams[12],
      useMainStream: false,
      timeShiftTs: null,
    });
    // First 8 tiles + hovered tile #12 use "mse", remaining 7 simulated tiles use "snapshot"
    expect(grid16.slice(0, 8).every((d) => d.renderMode === "mse")).toBe(true);
    expect(grid16[12].renderMode).toBe("mse");
    expect(grid16[10].renderMode).toBe("snapshot");
  });

  test("shouldRenderSpotlightTimeline_handles_reactive_and_wall_modes_across_layouts", () => {
    // Non-spotlight layouts always render timeline regardless of spotlightTimelineMode or timeShiftTs
    for (const layout of ["2x2", "focus", "single", "map"] as const) {
      expect(shouldRenderSpotlightTimeline(layout, "reactive", null)).toBe(true);
      expect(shouldRenderSpotlightTimeline(layout, "wall", null)).toBe(true);
      expect(shouldRenderSpotlightTimeline(layout, "wall", 1700000000)).toBe(true);
    }

    // Spotlight + reactive mode always renders timeline
    expect(shouldRenderSpotlightTimeline("spotlight", "reactive", null)).toBe(true);
    expect(shouldRenderSpotlightTimeline("spotlight", "reactive", 1700000000)).toBe(
      true
    );

    // Spotlight + wall mode hides timeline when live (timeShiftTs === null), shows when time-shifted
    expect(shouldRenderSpotlightTimeline("spotlight", "wall", null)).toBe(false);
    expect(shouldRenderSpotlightTimeline("spotlight", "wall", 1700000000)).toBe(true);
  });

  test("resolveEffectiveSpotlightHeroCam_enforces_auto_pause_lock_during_time_shift_and_releases_on_live", () => {
    // 1. Live mode (timeShiftTs === null) uses promotedCams[0] first, then latest trailCam, then selectedCamera, then fallbackCam
    expect(
      resolveEffectiveSpotlightHeroCam({
        promotedCams: ["garagem", "sala"],
        trailCams: [{ cam: "varanda", ts: 100 }],
        selectedCamera: "escritorio",
        fallbackCam: "hall",
        timeShiftTs: null,
        lockedHeroCam: "escritorio",
      })
    ).toBe("garagem");

    expect(
      resolveEffectiveSpotlightHeroCam({
        promotedCams: [],
        trailCams: [
          { cam: "sala", ts: 90 },
          { cam: "varanda", ts: 100 },
        ],
        selectedCamera: "escritorio",
        fallbackCam: "hall",
        timeShiftTs: null,
        lockedHeroCam: null,
      })
    ).toBe("varanda");

    expect(
      resolveEffectiveSpotlightHeroCam({
        promotedCams: [],
        trailCams: [],
        selectedCamera: "escritorio",
        fallbackCam: "hall",
        timeShiftTs: null,
        lockedHeroCam: null,
      })
    ).toBe("escritorio");

    expect(
      resolveEffectiveSpotlightHeroCam({
        promotedCams: [],
        trailCams: [],
        selectedCamera: "",
        fallbackCam: "hall",
        timeShiftTs: null,
        lockedHeroCam: null,
      })
    ).toBe("hall");

    // 2. Auto-Pause lock: while timeShiftTs !== null, background motion on "varanda" cannot hijack lockedHeroCam ("garagem")
    expect(
      resolveEffectiveSpotlightHeroCam({
        promotedCams: ["varanda", "garagem"],
        trailCams: [{ cam: "varanda", ts: 200 }],
        selectedCamera: "garagem",
        fallbackCam: "escritorio",
        timeShiftTs: 1700000000,
        lockedHeroCam: "garagem",
      })
    ).toBe("garagem");

    // 3. Returning to Live (timeShiftTs === null) immediately releases lock and promotes latest active camera ("varanda")
    expect(
      resolveEffectiveSpotlightHeroCam({
        promotedCams: ["varanda", "garagem"],
        trailCams: [{ cam: "varanda", ts: 200 }],
        selectedCamera: "garagem",
        fallbackCam: "escritorio",
        timeShiftTs: null,
        lockedHeroCam: "garagem",
      })
    ).toBe("varanda");
  });

  test("shouldRenderLiveAlertRibbon_hides_ribbon_in_spotlight_and_map_and_shows_in_grid_focus_single", () => {
    // Returns false for 'spotlight' and 'map' even when showAlertRibbon is true and unreviewedCount is 186
    expect(shouldRenderLiveAlertRibbon("spotlight", true, 186)).toBe(false);
    expect(shouldRenderLiveAlertRibbon("map", true, 186)).toBe(false);

    // Returns true for '2x2', 'focus', and 'single' when showAlertRibbon is true and unreviewedCount > 0
    for (const layout of ["2x2", "focus", "single"] as const) {
      expect(shouldRenderLiveAlertRibbon(layout, true, 186)).toBe(true);
      expect(shouldRenderLiveAlertRibbon(layout, true, 1)).toBe(true);
    }

    // Returns false when showAlertRibbon is false or unreviewedCount is 0
    for (const layout of ["2x2", "focus", "single", "spotlight", "map"] as const) {
      expect(shouldRenderLiveAlertRibbon(layout, false, 186)).toBe(false);
      expect(shouldRenderLiveAlertRibbon(layout, true, 0)).toBe(false);
      expect(shouldRenderLiveAlertRibbon(layout, false, 0)).toBe(false);
    }
  });
});
