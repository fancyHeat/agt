//! Auto-update via Velopack + GitHub Releases.
//!
//! With `update.enabled`, checks on startup and every `update.check_interval_minutes`, downloads
//! in the background, and applies the update when the user quits (or on the next launch, via
//! `VelopackApp`). The tray's "Update now" ([`check_now`]) works even with updates disabled: a
//! click is an explicit request.
//!
//! The feed is read from `releases/latest/download/` (plain HTTPS, no GitHub API, so no token
//! and no API rate limit). That release's feed lists its full package plus the delta from the
//! previous version; installs further behind fall back to the full package.
//! Every request has a hard timeout, so a dead connection can't stall the updater thread: a
//! short one for the feed (a few KB), a long one for packages.

use agt_core::config::UpdateConfig;
use std::{sync::Mutex, thread, time::Duration};
use velopack::{HttpOptions, UpdateCheck, UpdateManager, VelopackAsset, sources::HttpSource};

const FEED_URL: &str = "https://github.com/fancyHeat/agt/releases/latest/download";
/// Whole-request timeouts (connect + transfer) for the feed and for each package download.
const FEED_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Held for a whole check + download, so the background thread and "Update now" never run
/// at the same time (the second one waits, then finds the update already downloaded).
static CHECK: Mutex<()> = Mutex::new(());

/// Velopack handles for this install. Velopack sets timeouts per source, so the feed is checked
/// through a second manager; the `UpdateInfo` it returns is plain data that `um` downloads.
#[derive(Clone)]
pub struct Updater {
    /// Downloads, and finds/applies a downloaded update.
    pub um: UpdateManager,
    feed: UpdateManager,
}

fn manager(timeout: Duration) -> Result<UpdateManager, velopack::Error> {
    let options = HttpOptions {
        TimeoutMilliseconds: timeout.as_millis() as u64,
        ..Default::default()
    };
    UpdateManager::new(HttpSource::new_with_options(FEED_URL, options), None, None)
}

/// Returns the updater (for "Update now" and applying on quit), or `None` if the app isn't a
/// Velopack install. Starts the background updater only if updates are enabled.
pub fn start(cfg: &UpdateConfig) -> Option<Updater> {
    let updater = match (manager(DOWNLOAD_TIMEOUT), manager(FEED_TIMEOUT)) {
        (Ok(um), Ok(feed)) => Updater { um, feed },
        (Err(e), _) | (_, Err(e)) => {
            tracing::info!("not running from a Velopack install ({e}); updates unavailable");
            return None;
        }
    };
    let version = updater.um.get_current_version_as_string();
    if !cfg.enabled {
        tracing::info!("automatic updates disabled (current version {version})");
        return Some(updater);
    }
    let minutes = cfg.check_interval_minutes();
    tracing::info!("updater started: current version {version}, checking every {minutes} min");

    let worker = updater.clone();
    thread::Builder::new()
        .name("updater".into())
        .spawn(move || {
            loop {
                if let Err(e) = check_and_download(&worker, |_| {}) {
                    tracing::warn!("update check failed: {e}");
                }
                thread::sleep(Duration::from_secs(minutes * 60));
            }
        })
        .expect("spawn updater thread");
    Some(updater)
}

/// Checks and downloads now (blocking: run it off the UI thread). `downloading` is called
/// with the version before a download starts. Returns the update ready to apply, if any.
pub fn check_now(
    updater: &Updater,
    downloading: impl Fn(&str),
) -> Result<Option<VelopackAsset>, velopack::Error> {
    check_and_download(updater, downloading)?;
    Ok(updater.um.get_update_pending_restart())
}

fn check_and_download(
    Updater { um, feed }: &Updater,
    downloading: impl Fn(&str),
) -> Result<(), velopack::Error> {
    let _guard = CHECK.lock().unwrap_or_else(|e| e.into_inner());
    let info = match feed.check_for_updates()? {
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
    downloading(target);
    um.download_updates(&info, None)?;
    tracing::info!("update {target} downloaded; it will be applied on quit or next launch");
    Ok(())
}

/// Hands `asset` to the Velopack updater, which applies it once this process exits and then
/// starts the new version. Call right before exiting.
pub fn apply_and_restart(um: &UpdateManager, asset: &VelopackAsset) -> Result<(), velopack::Error> {
    tracing::info!("applying update {} and restarting", asset.Version);
    um.wait_exit_then_apply_updates(asset, true, true, Vec::<String>::new())
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
