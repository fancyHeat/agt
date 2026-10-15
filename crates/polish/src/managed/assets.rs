//! What the managed S1-mini downloads, pinned by URL (immutable revision/tag), size and
//! SHA-256, and where it puts it:
//!
//! ```text
//! <models dir>/s1-mini/s1-mini-q4_k_m.gguf      485 MB, superwhisper/s1-mini-GGUF
//! <models dir>/llama.cpp/<build>/llama-server   official llama.cpp release, unpacked
//! ```
//!
//! llama.cpp builds: Windows x64 uses the Vulkan build (32 MB; any GPU with a Vulkan driver,
//! which NVIDIA, AMD and Intel drivers include, and the CPU otherwise; it needs no CUDA
//! runtime, which would be another 400+ MB). macOS arm64 uses the Metal build (12 MB).
//!
//! The archive is downloaded (verified, see `agt_fetch`), unpacked with the OS's own `tar`
//! (bsdtar; it reads zip too, and refuses absolute paths and `..`) into a temporary folder,
//! renamed into place once complete (marked by [`MARKER`]), and deleted. Older builds in
//! `llama.cpp/` are removed then, so a pin bump doesn't leave them behind.

use super::Status;
use agt_fetch::ModelFile;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Subfolder of the models dir holding the GGUF.
pub const MODEL_DIR: &str = "s1-mini";
/// Subfolder of the models dir holding llama.cpp builds.
pub const LLAMA_DIR: &str = "llama.cpp";
/// The pinned llama.cpp release.
pub const LLAMA_BUILD: &str = "b11669";

/// S1-mini Q4_K_M, the build its published accuracy was measured on. Revision `v1` of
/// `superwhisper/s1-mini-GGUF`.
const GGUF: ModelFile = ModelFile {
    name: "s1-mini-q4_k_m.gguf",
    url: "https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/34add00a48a2e5d24e5a4ee5405a99620a3a240c/s1-mini-q4_k_m.gguf",
    size: 484_219_808,
    sha256: "3b41ebe2502cbd03e811d5d16b022f5ab551eda58d62597d152f89535003c634",
};

#[cfg(all(windows, target_arch = "x86_64"))]
const SERVER: Option<ModelFile> = Some(ModelFile {
    name: "llama-b11669-bin-win-vulkan-x64.zip",
    url: "https://github.com/ggml-org/llama.cpp/releases/download/b11669/llama-b11669-bin-win-vulkan-x64.zip",
    size: 32_182_350,
    sha256: "2c07574e7bde71321cb785282b20cc821e951132149eeceeea32ddebdbf0598e",
});

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const SERVER: Option<ModelFile> = Some(ModelFile {
    name: "llama-b11669-bin-macos-arm64.tar.gz",
    url: "https://github.com/ggml-org/llama.cpp/releases/download/b11669/llama-b11669-bin-macos-arm64.tar.gz",
    size: 12_191_582,
    sha256: "dbed55493d72bc1ba2babc8770d669c2289b5bc4ee9fb2a4dfc158d45b7d13fd",
});

#[cfg(not(any(
    all(windows, target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
)))]
const SERVER: Option<ModelFile> = None;

/// Written into the unpacked build folder last; without it the folder is incomplete.
const MARKER: &str = ".agt-complete";

#[cfg(windows)]
const SERVER_EXE: &str = "llama-server.exe";
#[cfg(not(windows))]
const SERVER_EXE: &str = "llama-server";

pub(crate) struct Assets {
    /// The models dir.
    root: PathBuf,
    model: ModelFile,
    server: ModelFile,
    build: &'static str,
}

impl Assets {
    pub fn for_this_platform(models_dir: &Path) -> Result<Self, String> {
        let server = SERVER.ok_or_else(|| {
            format!(
                "polish.provider = \"s1-mini\" is not available on {}-{} (no llama.cpp build is pinned for it); use provider = \"openai\" with your own server",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        Ok(Self {
            root: models_dir.to_owned(),
            model: GGUF,
            server,
            build: LLAMA_BUILD,
        })
    }

    #[cfg(test)]
    pub fn test(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            model: GGUF,
            server: GGUF,
            build: "btest",
        }
    }

    pub fn model_path(&self) -> PathBuf {
        self.root.join(MODEL_DIR).join(self.model.name)
    }

    fn llama_dir(&self) -> PathBuf {
        self.root.join(LLAMA_DIR)
    }

    fn build_dir(&self) -> PathBuf {
        self.llama_dir().join(self.build)
    }

    pub fn server_exe(&self) -> PathBuf {
        self.build_dir().join(SERVER_EXE)
    }

    /// Downloads and unpacks whatever is missing. Cheap when everything is there.
    pub fn ensure(&self, report: &mut dyn FnMut(Status)) -> Result<(), String> {
        download(&self.root.join(MODEL_DIR), &self.model, "S1-mini", report)?;
        if self.build_dir().join(MARKER).is_file() && self.server_exe().is_file() {
            return Ok(());
        }
        let dir = self.llama_dir();
        download(&dir, &self.server, "llama.cpp", report)?;
        let archive = dir.join(self.server.name);
        let staging = dir.join(format!("{}.unpacking", self.build));
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_dir_all(self.build_dir());
        fs::create_dir_all(&staging)
            .map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
        untar(&archive, &staging)?;
        // The archive has the files at its root (Windows) or in one folder (macOS).
        let unpacked = find_exe_dir(&staging)
            .ok_or_else(|| format!("{} has no {SERVER_EXE}", self.server.name))?;
        fs::write(unpacked.join(MARKER), self.server.sha256)
            .map_err(|e| format!("cannot write to {}: {e}", unpacked.display()))?;
        fs::rename(&unpacked, self.build_dir())
            .map_err(|e| format!("cannot move llama.cpp into place: {e}"))?;
        // Leftovers: the archive, the staging folder (if the build was in a subfolder) and
        // builds of earlier pins.
        if let Ok(entries) = fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.file_name() == self.build {
                    continue;
                }
                let p = e.path();
                let _ = if p.is_dir() {
                    fs::remove_dir_all(&p)
                } else {
                    fs::remove_file(&p)
                };
            }
        }
        tracing::info!(
            "llama.cpp {} unpacked to {}",
            self.build,
            self.build_dir().display()
        );
        Ok(())
    }
}

fn download(
    dir: &Path,
    file: &ModelFile,
    what: &'static str,
    report: &mut dyn FnMut(Status),
) -> Result<(), String> {
    let mut last = None;
    agt_fetch::ensure(dir, std::slice::from_ref(file), &mut |p| {
        let percent = (p.done * 100 / p.total.max(1)).min(100) as u8;
        if last != Some(percent) {
            last = Some(percent);
            report(Status::Downloading { what, percent });
        }
    })
}

/// `dir` itself if it holds the server, else its one subfolder that does.
fn find_exe_dir(dir: &Path) -> Option<PathBuf> {
    if dir.join(SERVER_EXE).is_file() {
        return Some(dir.to_owned());
    }
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.join(SERVER_EXE).is_file())
}

/// The OS's bsdtar: `%SystemRoot%\System32\tar.exe` on Windows 10+, `/usr/bin/tar` on macOS.
fn tar_exe() -> PathBuf {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        PathBuf::from(root).join("System32").join("tar.exe")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/usr/bin/tar")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        PathBuf::from("tar")
    }
}

fn untar(archive: &Path, into: &Path) -> Result<(), String> {
    let mut cmd = Command::new(tar_exe());
    cmd.arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {}: {e}", tar_exe().display()))?;
    if !out.status.success() {
        return Err(format!(
            "cannot unpack {}: tar {}: {}",
            archive.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};

    /// Serves `body` over HTTP to each connection; returns the URL.
    fn serve(body: Vec<u8>) -> &'static str {
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
                let _ = s.write_all(&body);
            }
        });
        Box::leak(format!("http://{addr}/file").into_boxed_str())
    }

    fn hex(b: &[u8]) -> &'static str {
        Box::leak(
            Sha256::digest(b)
                .iter()
                .map(|x| format!("{x:02x}"))
                .collect::<String>()
                .into_boxed_str(),
        )
    }

    /// A llama.cpp-like archive (server in a subfolder, as on macOS), made with the same tar.
    fn archive(work: &Path) -> Vec<u8> {
        let src = work.join("src");
        fs::create_dir_all(src.join("llama-bX")).unwrap();
        fs::write(src.join("llama-bX").join(SERVER_EXE), b"exe").unwrap();
        fs::write(src.join("llama-bX").join("libggml.so"), b"lib").unwrap();
        let tar = work.join("a.tar");
        let ok = Command::new(tar_exe())
            .arg("-cf")
            .arg(&tar)
            .arg("-C")
            .arg(&src)
            .arg("llama-bX")
            .status()
            .unwrap()
            .success();
        assert!(ok);
        fs::read(&tar).unwrap()
    }

    #[test]
    fn downloads_verifies_unpacks_and_cleans_up() {
        let work = std::env::temp_dir().join(format!("agt-s1-assets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&work);
        let root = work.join("models");
        let gguf = b"gguf!".to_vec();
        let tar = archive(&work);
        let mut a = Assets {
            root: root.clone(),
            model: ModelFile {
                name: "m.gguf",
                url: serve(gguf.clone()),
                size: gguf.len() as u64,
                sha256: hex(&gguf),
            },
            server: ModelFile {
                name: "llama.tar",
                url: serve(tar.clone()),
                size: tar.len() as u64,
                sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            },
            build: "b2",
        };
        // A leftover from an earlier pin, to be removed.
        fs::create_dir_all(root.join(LLAMA_DIR).join("b1")).unwrap();

        // A bad hash: nothing is unpacked and nothing partial stays.
        let mut statuses = Vec::new();
        let err = a.ensure(&mut |s| statuses.push(s)).unwrap_err();
        assert!(err.contains("failed verification"), "{err}");
        assert!(!a.server_exe().exists());
        assert!(!root.join(LLAMA_DIR).join("llama.tar.part").exists());
        assert_eq!(fs::read(a.model_path()).unwrap(), gguf);
        assert!(statuses.contains(&Status::Downloading {
            what: "S1-mini",
            percent: 100
        }));

        a.server.sha256 = hex(&tar);
        a.ensure(&mut |_| {}).unwrap();
        assert_eq!(fs::read(a.server_exe()).unwrap(), b"exe");
        assert!(a.build_dir().join("libggml.so").is_file());
        let left: Vec<_> = fs::read_dir(root.join(LLAMA_DIR))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["b2"], "archive, staging and old builds are removed");

        // Everything present: no network access.
        a.model.url = "http://127.0.0.1:1/unreachable";
        a.server.url = "http://127.0.0.1:1/unreachable";
        a.ensure(&mut |s| panic!("unexpected {s:?}")).unwrap();

        // An interrupted unpack (no marker) is redone.
        fs::remove_file(a.build_dir().join(MARKER)).unwrap();
        a.server.url = serve(tar.clone());
        a.ensure(&mut |_| {}).unwrap();
        assert!(a.build_dir().join(MARKER).is_file());
        fs::remove_dir_all(&work).unwrap();
    }
}
