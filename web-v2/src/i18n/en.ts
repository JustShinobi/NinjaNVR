export const en = {
  brand: {
    name: "NinjaNVR",
    release: "CONSOLE",
    engine: "RUST + OPENVINO",
  },
  nav: {
    live: "Live Matrix",
    sync: "Sync Playback",
    review: "Review & Triage",
    explore: "Object Explore",
    exports: "Exports",
    zones: "Settings & Studio",
    system: "System & Logs",
  },
  navMobile: {
    live: "Live",
    sync: "Playback",
    review: "Review",
    exports: "Exports",
    more: "More",
  },
  auth: {
    title: "OPERATOR AUTHENTICATION",
    subtitle: "Enter credentials to access NinjaNVR Security Console",
    username: "Username",
    password: "Password",
    signIn: "Authenticate",
    signingIn: "Verifying...",
    signOut: "Sign Out",
    invalidCredentials: "Authentication failed. Check username and password.",
  },
  live: {
    grid2x2: "2x2 Matrix",
    focusStage: "Focus + Timeline",
    singleFull: "Single Monitor",
    allCameras: "All Cameras",
    unreviewedAlerts: "Unreviewed Alerts",
    hoverToPreview: "Hover card for instant MP4 preview · Click to open in Focus Stage",
    markReviewed: "Mark Reviewed",
    markAllReviewed: "Acknowledge All",
    noPendingAlerts: "Zero unreviewed alerts in the last 24 hours",
    liveBadge: "LIVE",
    timeShiftBadge: "TIME-SHIFT",
    returnToLive: "Return to Live",
    rewind10s: "-10s",
    rewind30s: "-30s",
    rewind2m: "-2m",
    export60s: "Clip 60s",
    snapshot: "Snapshot",
    snapshotHint: "Save a full-resolution snapshot (main stream, privacy masks applied)",
    cameraDetails: "Camera details",
    gridColumns: "Columns",
    zonesOverlay: "Vectors",
    hdStream: "1080p Main",
    subStream: "720p Sub",
    motionActive: "MOTION",
    idleState: "NOMINAL",
    spotlightTimelineReactive: "Timeline: Auto-Pause",
    spotlightTimelineWall: "Clean Wall",
    spotlightPausedBadge: "SPOTLIGHT PAUSED // TIME-SHIFT",
    resumeSpotlight: "Resume Spotlight",
    focusCameraBtn: "Focus",
    matrixMode: "Matrix",
    spotlightMode: "Spotlight",
    tacticalMap: "Tactical Map",
    viewMode: "Live view mode",
    density: "DENSITY",
    densityNeedsMore: "Needs more than {n} cameras",
    densityAuto: "Showing every camera at this size",
    unreviewedShort: "UNREVIEWED",
    openReview: "Review",
    recentEvents: "Recent events",
    eventTimeShift: "Click to jump to this moment",
    exportClip: "Export clip",
    exportThisEvent: "Export this event as a clip",
    clipLast30s: "Clip 30s",
    clipLast30sHint: "Export the last 30 seconds of this camera",
  },
  sync: {
    title: "MULTI-CAMERA SYNCHRONIZED TIME MACHINE",
    subtitle: "Simultaneous 4-channel playback locked to a shared master clock for cross-room incident tracking",
    masterClock: "Master Timestamp",
    jumpBack30: "-30s All",
    jumpBack10: "-10s All",
    jumpFwd10: "+10s All",
    jumpFwd30: "+30s All",
    recentIncidents: "Recent Incident Jump Points",
    syncLocked: "4 CHANNELS LOCKED",
  },
  review: {
    title: "REVIEW & TRIAGE WORKBENCH",
    alerts: "Alerts",
    detections: "Detections",
    allSeverities: "All Severities",
    unreviewedOnly: "Unreviewed",
    reviewedOnly: "Reviewed",
    allStatuses: "All Items",
    markSelectedReviewed: "Mark Viewed",
    deleteSelected: "Delete",
    exportClip: "Export MP4",
    keyboardHint: "[J/K] Nav · [R] Ack · [E] Clip",
    emptyState: "No review segments match the active filter criteria.",
    shareLinkBtn: "Share Link",
    shareCopiedBtn: "Copied!",
    shareError: "Failed to generate share link",
    shareManualCopyHint: "Copy URL:",
  },
  explore: {
    title: "TRACKED OBJECT EXPLORER",
    searchPlaceholder: "Filter by ID, camera, label, or zone...",
    allLabels: "All Classes",
    favoritesOnly: "Retained Only",
    topScore: "Peak Confidence",
    duration: "Duration",
    inProgress: "Active Track",
    retainToggle: "Retain",
    falsePositive: "False Positive",
    viewSnapshot: "Snapshot",
    playClip: "Play Clip",
  },
  exports: {
    title: "MP4 EXPORT STUDIO & ARCHIVE",
    createTitle: "Generate New MP4 Clip",
    camera: "Camera Channel",
    rangePreset: "Time Window",
    last1m: "Last 1 Minute",
    last5m: "Last 5 Minutes",
    last15m: "Last 15 Minutes",
    customName: "Export Label",
    generateBtn: "Remux MP4 Clip",
    archiveTitle: "Exported Clips Archive",
    downloadMp4: "Download MP4",
    renameBtn: "Rename",
    deleteBtn: "Delete",
    noExports: "No exported MP4 clips on disk. Use the generator or 1-click export from Live/Review.",
    activeShareLinks: "Active Share Links",
    noShareLinks: "No active share links.",
    showInactiveLinks: "Show revoked/expired",
    maxViewsUnlimited: "Views: Unlimited",
    maxViews1: "1 view",
    maxViews5: "5 views",
    maxViews25: "25 views",
    copyLinkBtn: "Copy Link",
    revokeBtn: "Revoke",
    statusActive: "ACTIVE",
    statusRevoked: "REVOKED",
    statusExpired: "EXPIRED",
    statusExhausted: "EXHAUSTED",
    generatedUrlLabel: "Share URL",
  },
  zones: {
    title: "TACTICAL SETTINGS & CONFIGURATION STUDIO",
    visualTab: "Vector Mask & Zone Inspector",
    yamlTab: "Raw config.yml Studio",
    motionMasks: "Motion Mask Polygons",
    detectionZones: "Detection Zones",
    streamProbe: "RTSP Stream Probe",
    saveOnly: "Save Config",
    saveAndRestart: "Save & Restart NVR",
    copyYaml: "Copy YAML",
  },
  system: {
    title: "SYSTEM TELEMETRY & LIVE LOG STREAM",
    detectorLatency: "OpenVINO Inference Latency (ms)",
    cpuLoad: "Process CPU Utilization (%)",
    memoryRss: "Container Memory Footprint (%)",
    storageVolumes: "Storage Mounts & Retention Capacity",
    cameraHealth: "Per-Camera Pipeline Telemetry",
    liveLogs: "Service Log Stream",
    pauseStream: "Pause",
    resumeStream: "Resume",
    copyLogs: "Copy Buffer",
  },
  theme: {
    label: "Console Theme Mode",
    dark: "Tactical Dark",
    light: "Daylight / Light",
    system: "System Auto",
    highContrast: "High-Contrast Monitor Boost",
    highContrastDesc:
      "Strengthens structural borders (+15%), promotes muted text to >= 7:1 AAA contrast, and applies 1.5px border weight.",
    cycleTooltip: "Cycle Console Theme (Dark / Light / System)",
  },
};

export interface TranslationDict {
  brand: {
    name: string;
    release: string;
    engine: string;
  };
  nav: {
    live: string;
    sync: string;
    review: string;
    explore: string;
    exports: string;
    zones: string;
    system: string;
  };
  navMobile: {
    live: string;
    sync: string;
    review: string;
    exports: string;
    more: string;
  };
  auth: {
    title: string;
    subtitle: string;
    username: string;
    password: string;
    signIn: string;
    signingIn: string;
    signOut: string;
    invalidCredentials: string;
  };
  live: {
    grid2x2: string;
    focusStage: string;
    singleFull: string;
    allCameras: string;
    unreviewedAlerts: string;
    hoverToPreview: string;
    markReviewed: string;
    markAllReviewed: string;
    noPendingAlerts: string;
    liveBadge: string;
    timeShiftBadge: string;
    returnToLive: string;
    rewind10s: string;
    rewind30s: string;
    rewind2m: string;
    export60s: string;
    snapshot: string;
    snapshotHint: string;
    cameraDetails: string;
    gridColumns: string;
    zonesOverlay: string;
    hdStream: string;
    subStream: string;
    motionActive: string;
    idleState: string;
    spotlightTimelineReactive: string;
    spotlightTimelineWall: string;
    spotlightPausedBadge: string;
    resumeSpotlight: string;
    focusCameraBtn: string;
    matrixMode: string;
    spotlightMode: string;
    tacticalMap: string;
    viewMode: string;
    density: string;
    densityNeedsMore: string;
    densityAuto: string;
    unreviewedShort: string;
    openReview: string;
    recentEvents: string;
    eventTimeShift: string;
    exportClip: string;
    exportThisEvent: string;
    clipLast30s: string;
    clipLast30sHint: string;
  };
  sync: {
    title: string;
    subtitle: string;
    masterClock: string;
    jumpBack30: string;
    jumpBack10: string;
    jumpFwd10: string;
    jumpFwd30: string;
    recentIncidents: string;
    syncLocked: string;
  };
  review: {
    title: string;
    alerts: string;
    detections: string;
    allSeverities: string;
    unreviewedOnly: string;
    reviewedOnly: string;
    allStatuses: string;
    markSelectedReviewed: string;
    deleteSelected: string;
    exportClip: string;
    keyboardHint: string;
    emptyState: string;
    shareLinkBtn: string;
    shareCopiedBtn: string;
    shareError: string;
    shareManualCopyHint: string;
  };
  explore: {
    title: string;
    searchPlaceholder: string;
    allLabels: string;
    favoritesOnly: string;
    topScore: string;
    duration: string;
    inProgress: string;
    retainToggle: string;
    falsePositive: string;
    viewSnapshot: string;
    playClip: string;
  };
  exports: {
    title: string;
    createTitle: string;
    camera: string;
    rangePreset: string;
    last1m: string;
    last5m: string;
    last15m: string;
    customName: string;
    generateBtn: string;
    archiveTitle: string;
    downloadMp4: string;
    renameBtn: string;
    deleteBtn: string;
    noExports: string;
    activeShareLinks: string;
    noShareLinks: string;
    showInactiveLinks: string;
    maxViewsUnlimited: string;
    maxViews1: string;
    maxViews5: string;
    maxViews25: string;
    copyLinkBtn: string;
    revokeBtn: string;
    statusActive: string;
    statusRevoked: string;
    statusExpired: string;
    statusExhausted: string;
    generatedUrlLabel: string;
  };
  zones: {
    title: string;
    visualTab: string;
    yamlTab: string;
    motionMasks: string;
    detectionZones: string;
    streamProbe: string;
    saveOnly: string;
    saveAndRestart: string;
    copyYaml: string;
  };
  system: {
    title: string;
    detectorLatency: string;
    cpuLoad: string;
    memoryRss: string;
    storageVolumes: string;
    cameraHealth: string;
    liveLogs: string;
    pauseStream: string;
    resumeStream: string;
    copyLogs: string;
  };
  theme: {
    label: string;
    dark: string;
    light: string;
    system: string;
    highContrast: string;
    highContrastDesc: string;
    cycleTooltip: string;
  };
}
