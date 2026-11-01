use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Represents a saved NinjaNVR server profile in the Tauri v2 desktop client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerProfile {
    pub id: String,
    pub name: String,
    pub url: String,
    pub default_layout: String,
    pub hardware_accel: bool,
    pub always_on_top_wall: bool,
}

/// Persistent desktop configuration for the Tauri v2 native shell (`ninjanvr-desktop`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesktopConfig {
    pub active_profile_id: String,
    pub profiles: Vec<ServerProfile>,
    pub tray_enabled: bool,
    pub disable_background_throttling: bool,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            active_profile_id: "local".to_string(),
            profiles: vec![ServerProfile {
                id: "local".to_string(),
                name: "Primary NinjaNVR".to_string(),
                url: "https://localhost:8971".to_string(),
                default_layout: "2x2".to_string(),
                hardware_accel: true,
                always_on_top_wall: false,
            }],
            tray_enabled: true,
            disable_background_throttling: true,
        }
    }
}

impl DesktopConfig {
    pub fn load_or_default(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let payload = serde_json::to_string_pretty(self)?;
        std::fs::write(path, payload)?;
        Ok(())
    }

    pub fn frontend_dist_dir(repo_root: &Path) -> PathBuf {
        repo_root.join("web-v2").join("dist")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_config_roundtrip_persists_profiles() {
        let tmp = std::env::temp_dir().join("ninjanvr-desktop-test.json");
        let cfg = DesktopConfig::default();
        cfg.save(&tmp).unwrap();
        let loaded = DesktopConfig::load_or_default(&tmp);
        assert_eq!(cfg, loaded);
        let _ = std::fs::remove_file(&tmp);
    }
}
