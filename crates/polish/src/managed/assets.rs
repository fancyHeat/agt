//! What the managed S1-mini downloads, pinned by URL (immutable revision/tag), size and
//! SHA-256, and where it puts it:
//!
//! ```text
//! <models dir>/s1-mini/s1-mini-q4_k_m.gguf                485 MB, superwhisper/s1-mini-GGUF
//! <models dir>/agt-llama-server/<build>/llama-server      from an official llama.cpp release
//! ```
//!
//! llama.cpp builds: Windows x64 uses the Vulkan build (32 MB; any GPU with a Vulkan driver,
//! which NVIDIA, AMD and Intel drivers include, and the CPU otherwise; it needs no CUDA
//! runtime, which would be another 400+ MB). macOS arm64 uses the Metal build (12 MB).
//!
//! The archive is downloaded (verified, see `agt_fetch`) and unpacked with the OS's own
//! `tar` (bsdtar; it reads zip too, and refuses absolute paths and `..`) into a temporary
//! folder. Only the server and its libraries (and license files) are kept: they're moved
//! into a new folder with a manifest ([`MARKER`]: SHA-256, size and mtime of every file),
//! which is then renamed into place. Before every start the folder is checked against the
//! manifest ([`Assets::verified_server_exe`]): exactly those files, unchanged (a file whose
//! size or mtime differs is hashed again), so nothing put there later is ever run.
//!
//! The folder name is AGT's own (`stt.models_dir` may be a shared folder that already has
//! a `llama.cpp`), and AGT only ever deletes what it made there: builds with a manifest,
//! its staging folders and its archives.

use super::Status;
use agt_fetch::ModelFile;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::UNIX_EPOCH,
};

/// Subfolder of the models dir holding the GGUF.
pub const MODEL_DIR: &str = "s1-mini";
/// Subfolder of the models dir holding the llama.cpp builds AGT installed.
pub const LLAMA_DIR: &str = "agt-llama-server";
/// The pinned llama.cpp release. When bumping it, check every flag in
/// `LlamaLauncher::args` (process.rs) against the new `llama-server --help`: options get
/// renamed (the `--spec-*` ones were recently), and an unknown flag makes the server exit
/// at start, i.e. no polish at all.
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

/// The manifest, written into an installed build folder last.
const MARKER: &str = ".agt-manifest";

#[cfg(windows)]
const SERVER_EXE: &str = "llama-server.exe";
#[cfg(not(windows))]
const SERVER_EXE: &str = "llama-server";

/// The files of a build that are kept: the server, its libraries, license texts.
fn wanted(name: &str) -> bool {
    // Each tool's code lives in a `<tool>-impl` library; only the server's is needed.
    if name.contains("-impl.") {
        return name.contains("llama-server-impl.");
    }
    name == SERVER_EXE
        || name.ends_with(".dll")
        || name.ends_with(".dylib")
        || name.ends_with(".so")
        || name.starts_with("LICENSE")
}

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

    #[cfg(test)]
    pub fn server_exe(&self) -> PathBuf {
        self.build_dir().join(SERVER_EXE)
    }

    /// Downloads and installs whatever is missing. Cheap when everything is there.
    pub fn ensure(&self, report: &mut dyn FnMut(Status)) -> Result<(), String> {
        download(&self.root.join(MODEL_DIR), &self.model, "S1-mini", report)?;
        let build_dir = self.build_dir();
        if build_dir.exists() {
            if build_dir.join(MARKER).is_file() {
                return Ok(());
            }
            return Err(format!(
                "{} exists but wasn't installed by AGT (no {MARKER}); move it away",
                build_dir.display()
            ));
        }
        let dir = self.llama_dir();
        download(&dir, &self.server, "llama.cpp", report)?;
        let archive = dir.join(self.server.name);
        let staging = dir.join(format!("{}.unpacking", self.build));
        let ready = dir.join(format!("{}.installing", self.build));
        for d in [&staging, &ready] {
            let _ = fs::remove_dir_all(d);
            fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
        }
        untar(&archive, &staging)?;
        // The archive has the files at its root (Windows) or in one folder (macOS).
        let unpacked = find_exe_dir(&staging)
            .ok_or_else(|| format!("{} has no {SERVER_EXE}", self.server.name))?;
        let entries = fs::read_dir(&unpacked).map_err(|e| e.to_string())?;
        for e in entries.flatten() {
            let name = e.file_name();
            if wanted(&name.to_string_lossy()) {
                fs::rename(e.path(), ready.join(&name))
                    .map_err(|e| format!("cannot install {}: {e}", name.to_string_lossy()))?;
            }
        }
        write_manifest(&ready)?;
        fs::rename(&ready, &build_dir)
            .map_err(|e| format!("cannot move llama.cpp into place: {e}"))?;
        self.clean_up();
        tracing::info!(
            "llama.cpp {} installed in {}",
            self.build,
            build_dir.display()
        );
        Ok(())
    }

    /// Removes what AGT left in its folder: staging folders, archives, and builds it
    /// installed (they have a manifest) other than the current one. Nothing else.
    fn clean_up(&self) {
        let Ok(entries) = fs::read_dir(self.llama_dir()) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            let ours_dir = p.is_dir()
                && name != self.build
                && (name.ends_with(".unpacking")
                    || name.ends_with(".installing")
                    || p.join(MARKER).is_file());
            let ours_file = p.is_file()
                && name.starts_with("llama-b")
                && [".zip", ".tar.gz", ".zip.part", ".tar.gz.part"]
                    .iter()
                    .any(|ext| name.ends_with(ext));
            if ours_dir {
                let _ = fs::remove_dir_all(&p);
            } else if ours_file {
                let _ = fs::remove_file(&p);
            }
        }
    }

    /// The server executable, after checking its folder against the manifest written at
    /// install time: the same files and nothing else, each unchanged.
    pub fn verified_server_exe(&self) -> Result<PathBuf, String> {
        let dir = self.build_dir();
        let bad = |why: String| {
            format!(
                "{} doesn't match what AGT installed ({why}); delete that folder to reinstall",
                dir.display()
            )
        };
        let text = fs::read_to_string(dir.join(MARKER)).map_err(|e| bad(e.to_string()))?;
        let manifest = parse_manifest(&text).ok_or_else(|| bad("unreadable manifest".into()))?;
        let entries = fs::read_dir(&dir).map_err(|e| bad(e.to_string()))?;
        let present: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != MARKER)
            .collect();
        if let Some(extra) = present.iter().find(|n| !manifest.contains_key(*n)) {
            return Err(bad(format!("unexpected file {extra}")));
        }
        for (name, want) in &manifest {
            let path = dir.join(name);
            let have = stat(&path).map_err(|e| bad(format!("{name}: {e}")))?;
            if (have.size, have.mtime) != (want.size, want.mtime) {
                tracing::info!("{name} changed since it was installed; checking its hash");
                if sha256(&path).map_err(|e| bad(format!("{name}: {e}")))? != want.sha256 {
                    return Err(bad(format!("{name} was modified")));
                }
            }
        }
        if !manifest.contains_key(SERVER_EXE) {
            return Err(bad(format!("no {SERVER_EXE}")));
        }
        Ok(dir.join(SERVER_EXE))
    }
}

#[derive(Debug, PartialEq)]
struct Entry {
    sha256: String,
    size: u64,
    mtime: u128,
}

fn stat(path: &Path) -> std::io::Result<Entry> {
    let m = fs::metadata(path)?;
    let mtime = m
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(Entry {
        sha256: String::new(),
        size: m.len(),
        mtime,
    })
}

fn sha256(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match f.read(&mut buf)? {
            0 => break,
            n => h.update(&buf[..n]),
        }
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// One line per file: `sha256 size mtime name` (names have no tabs; separated by tabs).
fn write_manifest(dir: &Path) -> Result<(), String> {
    let mut out = String::new();
    let entries = fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let st = stat(&path).map_err(|e| format!("{name}: {e}"))?;
        let hash = sha256(&path).map_err(|e| format!("{name}: {e}"))?;
        out.push_str(&format!("{hash}\t{}\t{}\t{name}\n", st.size, st.mtime));
    }
    fs::write(dir.join(MARKER), out).map_err(|e| format!("cannot write the manifest: {e}"))
}

fn parse_manifest(text: &str) -> Option<BTreeMap<String, Entry>> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut f = l.splitn(4, '\t');
            let sha256 = f.next()?.to_owned();
            let size = f.next()?.parse().ok()?;
            let mtime = f.next()?.parse().ok()?;
            let name = f.next()?.to_owned();
            Some((
                name,
                Entry {
                    sha256,
                    size,
                    mtime,
                },
            ))
        })
        .collect()
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
    use std::io::Write;

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

    /// A llama.cpp-like archive (server in a subfolder, as on macOS), made with the same tar:
    /// the server, a library, a license, and a tool that isn't needed.
    fn archive(work: &Path) -> Vec<u8> {
        let src = work.join("src").join("llama-bX");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join(SERVER_EXE), b"exe").unwrap();
        fs::write(src.join("libggml.dylib"), b"lib").unwrap();
        fs::write(src.join("LICENSE"), b"MIT").unwrap();
        fs::write(src.join("llama-quantize"), b"tool").unwrap();
        fs::write(src.join("libllama-quantize-impl.dylib"), b"tool").unwrap();
        fs::write(src.join("libllama-server-impl.dylib"), b"srv").unwrap();
        let tar = work.join("a.tar");
        let ok = Command::new(tar_exe())
            .arg("-cf")
            .arg(&tar)
            .arg("-C")
            .arg(work.join("src"))
            .arg("llama-bX")
            .status()
            .unwrap()
            .success();
        assert!(ok);
        fs::read(&tar).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn downloads_verifies_installs_and_only_removes_its_own_files() {
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
                name: "llama-b2-bin-test.tar.gz",
                url: serve(tar.clone()),
                size: tar.len() as u64,
                sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            },
            build: "b2",
        };
        let dir = root.join(LLAMA_DIR);
        // An earlier build AGT installed (has a manifest), and things that aren't AGT's.
        fs::create_dir_all(dir.join("b1")).unwrap();
        fs::write(dir.join("b1").join(MARKER), "").unwrap();
        fs::create_dir_all(dir.join("b0-user")).unwrap();
        fs::write(dir.join("notes.txt"), b"mine").unwrap();
        fs::create_dir_all(root.join("llama.cpp")).unwrap();

        // A bad hash: nothing is installed and nothing partial stays.
        let mut statuses = Vec::new();
        let err = a.ensure(&mut |s| statuses.push(s)).unwrap_err();
        assert!(err.contains("failed verification"), "{err}");
        assert!(!a.server_exe().exists());
        assert!(!dir.join("llama-b2-bin-test.tar.gz.part").exists());
        assert_eq!(fs::read(a.model_path()).unwrap(), gguf);
        assert!(statuses.contains(&Status::Downloading {
            what: "S1-mini",
            percent: 100
        }));

        a.server.sha256 = hex(&tar);
        a.ensure(&mut |_| {}).unwrap();
        assert_eq!(
            names(&a.build_dir()),
            [
                MARKER,
                "LICENSE",
                "libggml.dylib",
                "libllama-server-impl.dylib",
                SERVER_EXE
            ],
            "only the server, its libraries and licenses are kept"
        );
        assert_eq!(a.verified_server_exe().unwrap(), a.server_exe());
        assert_eq!(
            names(&dir),
            ["b0-user", "b2", "notes.txt"],
            "archive, staging and AGT's old build removed; nothing else"
        );
        assert!(root.join("llama.cpp").is_dir());

        // Everything present: no network access.
        a.model.url = "http://127.0.0.1:1/unreachable";
        a.server.url = "http://127.0.0.1:1/unreachable";
        a.ensure(&mut |s| panic!("unexpected {s:?}")).unwrap();

        // Touched but identical: still fine. Modified, or an extra file: refused.
        let lib = a.build_dir().join("libggml.dylib");
        fs::write(&lib, b"lib").unwrap();
        a.verified_server_exe().unwrap();
        fs::write(&lib, b"evil").unwrap();
        assert!(a.verified_server_exe().unwrap_err().contains("modified"));
        fs::write(&lib, b"lib").unwrap();
        a.verified_server_exe().unwrap();
        fs::write(a.build_dir().join("version.dll"), b"x").unwrap();
        let err = a.verified_server_exe().unwrap_err();
        assert!(err.contains("unexpected file"), "{err}");

        // A build folder AGT didn't make is never replaced.
        fs::remove_dir_all(a.build_dir()).unwrap();
        fs::create_dir_all(a.build_dir()).unwrap();
        let err = a.ensure(&mut |_| {}).unwrap_err();
        assert!(err.contains("wasn't installed by AGT"), "{err}");
        fs::remove_dir_all(&work).unwrap();
    }
}
