//! Model files: download on first use, SHA-256 verified, atomically renamed into place.
//!
//! Each model lives in `<models dir>/<model id>/`. A file counts as present when it exists
//! with the expected size: the hash is checked once, at download time, and the file only
//! appears under its final name after that check passed (`<name>.part` → rename), so a
//! crash or a dropped connection never leaves a truncated file that looks complete.
//! Checking sizes instead of hashes on every start keeps start-up fast (the fp32 Parakeet
//! encoder is 2.4 GB).

use agt_core::stt::Result;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// One downloadable file, pinned by hash. `url` should point at an immutable revision.
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

/// Download progress for UI/log purposes.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
}

/// Makes sure every file in `files` is in `dir`, downloading missing ones.
/// `progress` is called now and then (at most ~once a second) during downloads.
pub fn ensure(dir: &Path, files: &[ModelFile], progress: &mut dyn FnMut(Progress)) -> Result<()> {
    let missing: Vec<&ModelFile> = files.iter().filter(|f| !is_present(dir, f)).collect();
    if missing.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let total: u64 = missing.iter().map(|f| f.size).sum();
    tracing::info!(
        "downloading {} model file(s), {:.0} MB, to {}",
        missing.len(),
        total as f64 / 1e6,
        dir.display()
    );
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .build()
        .new_agent();
    let mut done = 0;
    for f in missing {
        download(&agent, dir, f, &mut |n| {
            progress(Progress {
                done: done + n,
                total,
            })
        })?;
        done += f.size;
    }
    tracing::info!("model download complete");
    Ok(())
}

fn is_present(dir: &Path, f: &ModelFile) -> bool {
    fs::metadata(dir.join(f.name)).is_ok_and(|m| m.len() == f.size)
}

fn download(
    agent: &ureq::Agent,
    dir: &Path,
    f: &ModelFile,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let start = Instant::now();
    let dest = dir.join(f.name);
    let part = part_path(&dest);
    tracing::info!("downloading {} ({:.1} MB)", f.url, f.size as f64 / 1e6);
    let fail = |what: &str, e: &dyn std::fmt::Display| format!("{what} {}: {e}", f.name);

    let resp = agent
        .get(f.url)
        // Generous whole-body budget (>= 200 kB/s) so a stalled connection can't hang forever.
        .config()
        .timeout_recv_body(Some(Duration::from_secs(120 + f.size / 200_000)))
        .build()
        .call()
        .map_err(|e| fail("cannot download", &e))?;
    let mut body = resp.into_body().into_reader();
    let mut out = fs::File::create(&part).map_err(|e| fail("cannot create", &e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let (mut n, mut last_report) = (0u64, Instant::now());
    loop {
        let read = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                drop(out);
                let _ = fs::remove_file(&part);
                return Err(fail("download failed for", &e).into());
            }
        };
        hasher.update(&buf[..read]);
        out.write_all(&buf[..read])
            .map_err(|e| fail("cannot write", &e))?;
        n += read as u64;
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            progress(n);
        }
    }
    out.sync_all().map_err(|e| fail("cannot write", &e))?;
    drop(out);
    progress(n);

    let hash = hex(&hasher.finalize());
    if n != f.size || hash != f.sha256 {
        let _ = fs::remove_file(&part);
        return Err(format!(
            "{} failed verification: got {n} bytes, sha256 {hash}; expected {} bytes, sha256 {}",
            f.name, f.size, f.sha256
        )
        .into());
    }
    fs::rename(&part, &dest).map_err(|e| fail("cannot move into place", &e))?;
    let secs = start.elapsed().as_secs_f64();
    tracing::info!(
        "{} verified ({:.1} MB in {secs:.1} s, {:.1} MB/s)",
        f.name,
        n as f64 / 1e6,
        n as f64 / 1e6 / secs.max(1e-3)
    );
    Ok(())
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_of_sha256() {
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn present_means_right_size() {
        let dir = std::env::temp_dir().join(format!("agt-models-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = ModelFile {
            name: "x.bin",
            url: "unused",
            size: 3,
            sha256: "unused",
        };
        assert!(!is_present(&dir, &f));
        fs::write(dir.join("x.bin"), b"ab").unwrap();
        assert!(!is_present(&dir, &f));
        fs::write(dir.join("x.bin"), b"abc").unwrap();
        assert!(is_present(&dir, &f));
        // Nothing missing: no network access is attempted.
        ensure(&dir, &[f], &mut |_| panic!("no download expected")).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
