import React from "react";
import { resolveScopeGroupLabel, SettingsScopeGroup } from "../utils/settingsUx";

interface SettingsScopeDividerProps {
  group: SettingsScopeGroup;
  scope: "global" | string;
  inspectorCam: string;
  cameraCount: number;
  isPt: boolean;
}

/**
 * Heading that groups Settings cards by where they apply. Replaces the
 * per-card "SCOPE:" pills: global-only cards sit under the global divider,
 * camera-or-default cards under the divider that follows the header scope.
 */
export const SettingsScopeDivider: React.FC<SettingsScopeDividerProps> = ({
  group,
  scope,
  inspectorCam,
  cameraCount,
  isPt,
}) => (
  <h2
    className={`nvr-scope-divider ${group === "camera" && scope !== "global" ? "is-camera" : ""}`}
  >
    {resolveScopeGroupLabel(group, scope, inspectorCam, cameraCount, isPt)}
  </h2>
);
