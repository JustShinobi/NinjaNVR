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
    zonesOverlay: "Vectors",
    hdStream: "1080p Main",
    subStream: "720p Sub",
    motionActive: "MOTION",
    idleState: "NOMINAL",
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
    zonesOverlay: string;
    hdStream: string;
    subStream: string;
    motionActive: string;
    idleState: string;
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
}
