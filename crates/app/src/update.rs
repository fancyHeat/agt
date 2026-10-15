//! Background auto-update via Velopack + GitHub Releases.
//!
//! Checks on startup and every `update.check_interval_minutes`, downloads in the background,
//! and applies the update when the user quits (or on the next launch, via `VelopackApp`).

use agt_core::config::{TOKEN_ENV, UpdateConfig};
use std::{thread, time::Duration};
use velopack::{UpdateCheck, UpdateManager, sources::GithubSource};

const REPO_URL: &str = "https://github.com/fancyHeat/agt";

/// Starts the background updater. Returns the manager (for applying on quit), or `None`
/// if updates are disabled, no token is configured, or the app isn't a Velopack install.
pub fn start(cfg: &UpdateConfig) -> Option<UpdateManager> {
    if !cfg.enabled {
        tracing::info!("updates disabled in config");
        return None;
    }
    let Some(token) = cfg.token() else {
        tracing::info!(
            "no GitHub token (update.github_token or {TOKEN_ENV}); skipping update checks"
        );
        return None;
    };
    let source = GithubSource::new(REPO_URL, Some(token), false);
    let um = match UpdateManager::new(source, None, None) {
        Ok(um) => um,
        Err(e) => {
            tracing::info!("not running from a Velopack install ({e}); updates disabled");
            return None;
        }
    };
    tracing::info!(
        "updater started: current version {}, checking every {} min",
        um.get_current_version_as_string(),
        cfg.check_interval_minutes
    );

    let interval = Duration::from_secs(cfg.check_interval_minutes.max(1) * 60);
    let worker = um.clone();
    thread::Builder::new()
        .name("updater".into())
        .spawn(move || {
            loop {
                if let Err(e) = check_and_download(&worker) {
                    tracing::warn!("update check failed: {e}");
                }
                thread::sleep(interval);
            }
        })
        .expect("spawn updater thread");
    Some(um)
}

fn check_and_download(um: &UpdateManager) -> Result<(), velopack::Error> {
    let info = match um.check_for_updates()? {
        UpdateCheck::UpdateAvailable(info) => info,
        UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty => {
            tracing::info!("no update available");
            return Ok(());
        }
    };
    let target = &info.TargetFullRelease.Version;
    if um
        .get_update_pending_restart()
        .is_some_and(|p| &p.Version == target)
    {
        tracing::debug!("update {target} already downloaded");
        return Ok(());
    }
    tracing::info!("update {target} available; downloading");
    um.download_updates(&info, None)?;
    tracing::info!("update {target} downloaded; it will be applied on quit or next launch");
    Ok(())
}

/// If an update has been downloaded, hand it to the Velopack updater, which applies it
/// silently once this process exits. Call right before exiting.
pub fn apply_pending_on_exit(um: &UpdateManager) {
    let Some(asset) = um.get_update_pending_restart() else {
        return;
    };
    tracing::info!("applying update {} on exit", asset.Version);
    if let Err(e) = um.wait_exit_then_apply_updates(&asset, true, false, Vec::<String>::new()) {
        tracing::error!("failed to start updater: {e}");
    }
}
