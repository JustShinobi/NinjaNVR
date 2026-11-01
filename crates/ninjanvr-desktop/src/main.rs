use std::path::PathBuf;

use anyhow::Result;
use ninjanvr_desktop::DesktopConfig;
use tracing::info;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg_path = std::env::var("NINJANVR_DESKTOP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("ninjanvr-desktop.json"));
    let cfg = DesktopConfig::load_or_default(&cfg_path);
    info!(
        "NinjaNVR Tauri v2 Desktop Shell ready (active_profile={}, profiles={}, tray={})",
        cfg.active_profile_id,
        cfg.profiles.len(),
        cfg.tray_enabled
    );
    Ok(())
}
