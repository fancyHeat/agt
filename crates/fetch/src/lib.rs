//! Downloaded files (STT models, the S1-mini polish model and its llama.cpp server):
//! fetched on first use, SHA-256 verified, atomically renamed into place.
//!
//! Each model lives in `<models dir>/<model id>/`. A file counts as present when it exists
//! with the expected size: the hash is checked once, at download time, and the file only
//! appears under its final name after that check passed (`<name>.part` → rename), so a
//! crash or a dropped connection never leaves a truncated file that looks complete.
//! Checking sizes instead of hashes on every start keeps start-up fast (the fp32 Parakeet
//! encoder is 2.4 GB).

use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Errors are reported to the user as text and logged.
pub type Result<T> = std::result::Result<T, String>;

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

pub fn is_present(dir: &Path, f: &ModelFile) -> bool {
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
    // Any failure (network, full disk, bad hash) removes the partial file: it can be GBs.
    let n = match fetch(agent, f, &part, progress) {
        Ok(n) => n,
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Err(e);
        }
    };
    fs::rename(&part, &dest).map_err(|e| format!("cannot move {} into place: {e}", f.name))?;
    let secs = start.elapsed().as_secs_f64();
    tracing::info!(
        "{} verified ({:.1} MB in {secs:.1} s, {:.1} MB/s)",
        f.name,
        n as f64 / 1e6,
        n as f64 / 1e6 / secs.max(1e-3)
    );
    Ok(())
}

/// Streams `f` into `part`, hashing as it goes; returns the size once verified.
fn fetch(
    agent: &ureq::Agent,
    f: &ModelFile,
    part: &Path,
    progress: &mut dyn FnMut(u64),
) -> Result<u64> {
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
    let mut out = fs::File::create(part).map_err(|e| fail("cannot create", &e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let (mut n, mut last_report) = (0u64, Instant::now());
    loop {
        let read = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(fail("download failed for", &e)),
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
        return Err(format!(
            "{} failed verification: got {n} bytes, sha256 {hash}; expected {} bytes, sha256 {}",
            f.name, f.size, f.sha256
        ));
    }
    Ok(n)
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

    /// Serves `body` over HTTP to each connection, on 127.0.0.1; returns the base URL.
    fn serve(body: &'static [u8]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut req = [0u8; 4096];
                let _ = s.read(&mut req);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body);
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn download_verifies_and_never_leaves_partial_files() {
        let base: &'static str = Box::leak(serve(b"abc").into_boxed_str());
        let url: &'static str = Box::leak(format!("{base}/x.bin").into_boxed_str());
        let dir = std::env::temp_dir().join(format!("agt-dl-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let good = ModelFile {
            name: "x.bin",
            url,
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let bad = ModelFile {
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            ..good
        };
        let err = ensure(&dir, &[bad], &mut |_| {}).unwrap_err().to_string();
        assert!(err.contains("failed verification"), "{err}");
        assert!(!dir.join("x.bin").exists());
        assert!(!dir.join("x.bin.part").exists());

        ensure(&dir, &[good], &mut |_| {}).unwrap();
        assert_eq!(fs::read(dir.join("x.bin")).unwrap(), b"abc");
        assert!(!dir.join("x.bin.part").exists());
        fs::remove_dir_all(&dir).unwrap();
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
