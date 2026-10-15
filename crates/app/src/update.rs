//! Background auto-update via Velopack + GitHub Releases.
//!
//! Checks on startup and every `update.check_interval_minutes`, downloads in the background,
//! and applies the update when the user quits (or on the next launch, via `VelopackApp`).
//!
//! The feed is read from `releases/latest/download/` (plain HTTPS, no GitHub API, so no token
//! and no API rate limit). That release's feed lists its full package plus the delta from the
//! previous version; installs further behind fall back to the full package.
//! Every request has a hard timeout, so a dead connection can't stall the updater thread.

use agt_core::config::UpdateConfig;
use std::{thread, time::Duration};
use velopack::{HttpOptions, UpdateCheck, UpdateManager, sources::HttpSource};

const FEED_URL: &str = "https://github.com/fancyHeat/agt/releases/latest/download";
/// Whole-request timeout (connect + transfer) for the feed and each package download.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Starts the background updater. Returns the manager (for applying on quit), or `None`
/// if updates are disabled or the app isn't a Velopack install.
pub fn start(cfg: &UpdateConfig) -> Option<UpdateManager> {
    if !cfg.enabled {
        tracing::info!("updates disabled");
        return None;
    }
    let options = HttpOptions {
        TimeoutMilliseconds: REQUEST_TIMEOUT.as_millis() as u64,
        ..Default::default()
    };
    let source = HttpSource::new_with_options(FEED_URL, options);
    let um = match UpdateManager::new(source, None, None) {
        Ok(um) => um,
        Err(e) => {
            tracing::info!("not running from a Velopack install ({e}); updates disabled");
            return None;
        }
    };
    let minutes = cfg.check_interval_minutes();
    tracing::info!(
        "updater started: current version {}, checking every {minutes} min",
        um.get_current_version_as_string()
    );

    let worker = um.clone();
    thread::Builder::new()
        .name("updater".into())
        .spawn(move || {
            loop {
                if let Err(e) = check_and_download(&worker) {
                    tracing::warn!("update check failed: {e}");
                }
                thread::sleep(Duration::from_secs(minutes * 60));
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
    let kind = if info.DeltasToTarget.is_empty() {
        "full"
    } else {
        "delta"
    };
    tracing::info!("update {target} available; downloading ({kind})");
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
