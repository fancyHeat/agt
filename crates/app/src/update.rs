//! Auto-update via Velopack + GitHub Releases.
//!
//! With `update.enabled`, checks on startup and every `update.check_interval_minutes`, downloads
//! in the background, and applies the update when the user quits (or on the next launch, via
//! `VelopackApp`). On macOS that asks for an admin password when the bundle isn't the user's
//! (see `silent`). The tray's "Check for updates" ([`check_now`]) also installs the update and restarts
//! as soon as dictation is idle, and works even with updates disabled: a click is an explicit
//! request.
//!
//! The feed is read from `releases/latest/download/` (plain HTTPS, no GitHub API, so no token
//! and no API rate limit). That release's feed lists its full package plus the delta from the
//! previous version; installs further behind fall back to the full package.
//! Every request has a hard timeout, so a dead connection can't stall the updater thread: a
//! short one for the feed (a few KB), a long one for packages.
//!
//! Testing: `AGT_UPDATE_FEED` replaces the feed with another URL or a local directory (e.g. the
//! `Releases` folder `vpk pack` wrote), so an update can be tried without publishing.

use agt_core::config::UpdateConfig;
use std::{
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};
use velopack::{
    HttpOptions, UpdateCheck, UpdateManager, VelopackAsset,
    sources::{FileSource, HttpSource, UpdateSource},
};

const FEED_URL: &str = "https://github.com/fancyHeat/agt/releases/latest/download";
/// Overrides [`FEED_URL`]: a URL, or a local directory (for testing updates).
const FEED_ENV: &str = "AGT_UPDATE_FEED";
/// Whole-request timeouts (connect + transfer) for the feed and for each package download.
const FEED_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Held for a whole check + download, so the background thread and "Check for updates" never run
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
    let source: Box<dyn UpdateSource> = match std::env::var(FEED_ENV) {
        Ok(feed) if !feed.contains("://") => Box::new(FileSource::new(feed)),
        Ok(feed) => Box::new(HttpSource::new_with_options(feed, options)),
        Err(_) => Box::new(HttpSource::new_with_options(FEED_URL, options)),
    };
    UpdateManager::new_boxed(source, None, None)
}

/// Returns the updater (for "Check for updates" and applying on quit), or `None` if the app isn't a
/// Velopack install. Starts the background updater only if updates are enabled.
pub fn start(cfg: &UpdateConfig) -> Option<Updater> {
    let updater = match (manager(DOWNLOAD_TIMEOUT), manager(FEED_TIMEOUT)) {
        (Ok(um), Ok(feed)) => Updater { um, feed },
        (Err(e), _) | (_, Err(e)) => {
            tracing::info!("not running from a Velopack install ({e}); updates unavailable");
            return None;
        }
    };
    if let Ok(feed) = std::env::var(FEED_ENV) {
        tracing::warn!("{FEED_ENV} is set: updates come from {feed}");
    }
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

/// Records an update step in the telemetry database's `updates` table. `ok`: the result
/// to record on success ("none", "available" or "ok").
fn record<T, E: std::fmt::Display>(
    kind: &'static str,
    to_version: Option<&str>,
    started: Instant,
    result: &Result<T, E>,
    ok: &'static str,
) {
    agt_telemetry::global().update(agt_telemetry::Update {
        kind,
        from_version: env!("CARGO_PKG_VERSION").into(),
        to_version: to_version.map(str::to_owned),
        result: if result.is_ok() { ok } else { "error" },
        error: result.as_ref().err().map(ToString::to_string),
        duration_ms: started.elapsed().as_secs_f64() * 1e3,
    });
}

fn check_and_download(
    Updater { um, feed }: &Updater,
    downloading: impl Fn(&str),
) -> Result<(), velopack::Error> {
    let _guard = CHECK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Instant::now();
    let check = feed.check_for_updates();
    let available = match &check {
        Ok(UpdateCheck::UpdateAvailable(info)) => Some(info.TargetFullRelease.Version.as_str()),
        _ => None,
    };
    record(
        "check",
        available,
        t,
        &check,
        if available.is_some() {
            "available"
        } else {
            "none"
        },
    );
    let info = match check? {
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
    let t = Instant::now();
    let downloaded = um.download_updates(&info, None);
    record("download", Some(target), t, &downloaded, "ok");
    downloaded?;
    tracing::info!("update {target} downloaded");
    Ok(())
}

/// Hands `asset` to the Velopack updater, which applies it once this process exits and then
/// starts the new version. Call right before exiting.
pub fn apply_and_restart(um: &UpdateManager, asset: &VelopackAsset) -> Result<(), velopack::Error> {
    tracing::info!("applying update {} and restarting", asset.Version);
    let t = Instant::now();
    let result = um.wait_exit_then_apply_updates(asset, silent(), true, Vec::<String>::new());
    record("apply", Some(&asset.Version), t, &result, "ok");
    result
}

/// If an update has been downloaded, hand it to the Velopack updater, which applies it once
/// this process exits (asking for an admin password first if it must, see [`silent`]). Call
/// right before exiting.
pub fn apply_pending_on_exit(um: &UpdateManager) {
    let Some(asset) = um.get_update_pending_restart() else {
        return;
    };
    tracing::info!("applying update {} on exit", asset.Version);
    let t = Instant::now();
    let result = um.wait_exit_then_apply_updates(&asset, silent(), false, Vec::<String>::new());
    record("apply", Some(&asset.Version), t, &result, "ok");
    if let Err(e) = result {
        tracing::error!("failed to start updater: {e}");
    }
}

/// Whether the updater can apply without asking anything: everywhere but a macOS bundle this
/// user can't replace. The .pkg installer leaves `/Applications/AGT.app` owned by root, and a
/// silent updater can't ask for an admin password to replace it, so it gave up and relaunched
/// the old version (offering the same update again). Not silent, Velopack asks once; the
/// bundle it moves in was extracted by this user, so later updates are silent again (for an
/// admin: a standard user can't write `/Applications` either, so is asked every time).
fn silent() -> bool {
    #[cfg(target_os = "macos")]
    if let Some(bundle) = bundle_path()
        && let Some(dir) = [bundle.as_path(), bundle.parent().unwrap_or(&bundle)]
            .into_iter()
            .find(|d| needs_admin(d))
    {
        tracing::info!(
            "{} is not writable: the updater will ask for an admin password",
            dir.display()
        );
        return false;
    }
    true
}

/// The `.app` this runs from: the nearest ancestor of the executable named `*.app`.
#[cfg(target_os = "macos")]
fn bundle_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(|p| p.to_path_buf())
}

/// Whether writing `dir` takes more rights than this user has (replacing the bundle renames it
/// in its parent and rewrites its `..`). Not for a read-only volume (`EROFS`) or a protected one
/// (`EPERM`): no password fixes those.
#[cfg(target_os = "macos")]
fn needs_admin(dir: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
    if unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EACCES)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::needs_admin;
    use std::path::Path;

    #[test]
    fn needs_admin_only_for_dirs_a_password_unlocks() {
        assert!(!needs_admin(&std::env::temp_dir()));
        assert!(needs_admin(Path::new("/Library"))); // root-owned
        assert!(!needs_admin(Path::new("/System"))); // EPERM: System Integrity Protection
    }
}
