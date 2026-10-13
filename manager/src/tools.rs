//! Outside programs for video downloads, kept in the data folder's `tools` and fetched from
//! their official GitHub releases. Every file is checked against the release's SHA-256
//! list, and yt-dlp's list must also carry its maintainers' signature.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use engine::{CancellationToken, Options, Progress};
use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::task::spawn_blocking;

use crate::{Error, Result};

/// yt-dlp's release signing key (public.key in its repository), fingerprint
/// AC0C BBE6 848D 6A87 3464 AF4E 57CF 6593 3B5A 7581.
const YT_DLP_KEY: &str = include_str!("tools/yt-dlp-signing-key.asc");
const RECORD: &str = "tools.json";
/// How often a tool kept up to date by itself is checked.
pub const CHECK_EVERY_SECS: i64 = 24 * 60 * 60;

/// Whether a tool last checked at `checked` (unix seconds) is due for a check at `now`.
pub fn due(checked: i64, now: i64) -> bool {
    now.saturating_sub(checked) >= CHECK_EVERY_SECS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Finds a page's video formats and their stream addresses.
    YtDlp,
    /// JavaScript runtime yt-dlp uses for YouTube's player challenges.
    Deno,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::YtDlp, Tool::Deno];

    pub fn name(self) -> &'static str {
        match self {
            Tool::YtDlp => "yt-dlp",
            Tool::Deno => "deno",
        }
    }

    /// The program, relative to the tools folder. yt-dlp's is its folder build, which starts
    /// in a third of the time of the one-file build (that one unpacks itself on every run).
    fn file(self) -> PathBuf {
        let exe = format!("{}{}", self.name(), std::env::consts::EXE_SUFFIX);
        match self {
            Tool::YtDlp if cfg!(target_os = "macos") => Path::new("yt-dlp").join("yt-dlp_macos"),
            Tool::YtDlp => Path::new("yt-dlp").join(exe),
            Tool::Deno => PathBuf::from(exe),
        }
    }

    /// Where earlier versions kept the program (yt-dlp's one-file build): still used until
    /// the next update installs the folder build, which then deletes it. Only Windows had
    /// such versions (and elsewhere a program named `yt-dlp` would be the folder itself).
    fn old_file(self) -> Option<PathBuf> {
        (cfg!(windows) && self == Tool::YtDlp).then(|| PathBuf::from("yt-dlp.exe"))
    }
}

/// Where a tool comes from on this platform.
struct Source {
    repo: &'static str,
    asset: &'static str,
    sums: Sums,
    unpack: Unpack,
}

enum Sums {
    /// One signed list for all assets (`SHA2-256SUMS` + `.sig`).
    Signed(&'static str),
    /// A `<asset>.sha256sum` file next to each asset.
    PerFile,
}

enum Unpack {
    /// The program is this file in the zip.
    One(&'static str),
    /// The whole zip is the program's folder.
    Folder,
}

fn source(tool: Tool, nightly: bool) -> Option<Source> {
    use std::env::consts::{ARCH, OS};
    Some(match tool {
        Tool::YtDlp => Source {
            repo: if nightly { "yt-dlp/yt-dlp-nightly-builds" } else { "yt-dlp/yt-dlp" },
            asset: match OS {
                "windows" => "yt-dlp_win.zip",
                "macos" => "yt-dlp_macos.zip",
                _ => return None,
            },
            sums: Sums::Signed("SHA2-256SUMS"),
            unpack: Unpack::Folder,
        },
        Tool::Deno => Source {
            repo: "denoland/deno",
            asset: match (OS, ARCH) {
                ("windows", "x86_64") => "deno-x86_64-pc-windows-msvc.zip",
                ("macos", "aarch64") => "deno-aarch64-apple-darwin.zip",
                ("macos", "x86_64") => "deno-x86_64-apple-darwin.zip",
                _ => return None,
            },
            sums: Sums::PerFile,
            unpack: Unpack::One(if OS == "windows" { "deno.exe" } else { "deno" }),
        },
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub nightly: bool,
    /// When the newest release was last looked up (unix seconds), whether or not that
    /// installed anything.
    #[serde(default)]
    pub checked: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    Installed(String),
    UpToDate(String),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Release {
    fn url(&self, name: &str) -> Result<&str> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.as_str())
            .ok_or_else(|| Error::Tool(format!("release {} has no {name}", self.tag_name)))
    }
}

pub struct Tools {
    dir: PathBuf,
    http: reqwest::Client,
}

impl Tools {
    /// Tools kept in `dir` (the data folder's `tools`).
    pub fn new(dir: PathBuf) -> Result<Self> {
        let http = reqwest::Client::builder().user_agent(concat!("rdm/", env!("CARGO_PKG_VERSION"))).build()?;
        Ok(Tools { dir, http })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The program, if installed (in an earlier version's place, until the next update).
    pub fn path(&self, tool: Tool) -> Option<PathBuf> {
        std::iter::once(tool.file()).chain(tool.old_file()).map(|f| self.dir.join(f)).find(|p| p.is_file())
    }

    /// What the last update installed, if it is in its current place.
    pub fn installed(&self, tool: Tool) -> Option<Installed> {
        self.dir.join(tool.file()).is_file().then(|| self.records().remove(tool.name()))?
    }

    fn records(&self) -> BTreeMap<String, Installed> {
        std::fs::read(self.dir.join(RECORD)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Installs the newest release, unless it is already there.
    pub async fn update(&self, tool: Tool, nightly: bool) -> Result<Update> {
        let src = source(tool, nightly).ok_or_else(|| Error::Tool(format!("{} has no build for this system", tool.name())))?;
        let release: Release = self
            .http
            .get(format!("https://api.github.com/repos/{}/releases/latest", src.repo))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let asset = src.asset;
        let wanted = Installed { version: release.tag_name.clone(), nightly, checked: unix_now() };
        if self.installed(tool).is_some_and(|i| i.version == wanted.version && i.nightly == nightly) {
            self.record(tool, &wanted)?;
            return Ok(Update::UpToDate(wanted.version));
        }

        let expected = match src.sums {
            Sums::Signed(list) => {
                let sums = self.text(release.url(list)?).await?;
                let sig = self.bytes(release.url(&format!("{list}.sig"))?).await?;
                verify_signature(sums.as_bytes(), &sig)?;
                hash_in_list(&sums, asset)
            }
            Sums::PerFile => first_hash(&self.text(release.url(&format!("{asset}.sha256sum"))?).await?),
        }
        .ok_or_else(|| Error::Tool(format!("no checksum for {asset} in release {}", release.tag_name)))?;

        let work = self.dir.join(".download");
        std::fs::create_dir_all(&work)?;
        let opts = Options { connections: 4, dir: work.clone(), file_name: Some(asset.into()), ..Options::default() };
        let (progress, _) = watch::channel(Progress::default());
        let fetched = engine::download(release.url(asset)?, &opts, &progress, &CancellationToken::new()).await?.path;

        let result = install(&fetched, &expected, src.unpack, &self.dir.join(tool.file())).await;
        let _ = std::fs::remove_dir_all(&work);
        result?;
        if let Some(old) = tool.old_file() {
            let _ = std::fs::remove_file(self.dir.join(old));
        }
        self.record(tool, &wanted)?;
        Ok(Update::Installed(wanted.version))
    }

    fn record(&self, tool: Tool, installed: &Installed) -> Result<()> {
        let mut records = self.records();
        records.insert(tool.name().into(), installed.clone());
        std::fs::write(self.dir.join(RECORD), serde_json::to_vec_pretty(&records)?)?;
        Ok(())
    }

    async fn bytes(&self, url: &str) -> Result<Vec<u8>> {
        Ok(self.http.get(url).send().await?.error_for_status()?.bytes().await?.to_vec())
    }

    async fn text(&self, url: &str) -> Result<String> {
        String::from_utf8(self.bytes(url).await?).map_err(|_| Error::Tool(format!("{url} is not text")))
    }
}

/// Checks the file, takes the program out of it when it is a zip, and moves it to `dest`.
async fn install(fetched: &Path, expected: &str, unpack: Unpack, dest: &Path) -> Result<()> {
    let (fetched, expected, dest) = (fetched.to_owned(), expected.to_owned(), dest.to_owned());
    spawn_blocking(move || {
        let got = sha256_file(&fetched)?;
        if !got.eq_ignore_ascii_case(&expected) {
            return Err(Error::Tool(format!("checksum mismatch for {}: expected {expected}, got {got}", fetched.display())));
        }
        let open = || zip::ZipArchive::new(BufReader::new(File::open(&fetched)?)).map_err(|e| Error::Tool(format!("{}: {e}", fetched.display())));
        match unpack {
            Unpack::One(name) => {
                let out = fetched.with_extension("program");
                let mut archive = open()?;
                let mut entry = archive.by_name(name).map_err(|e| Error::Tool(format!("{name} in {}: {e}", fetched.display())))?;
                io::copy(&mut entry, &mut File::create(&out)?)?;
                place(&out, &dest)
            }
            Unpack::Folder => {
                let folder = dest.parent().expect("the program has a folder").to_owned();
                let staged = folder.with_extension("new");
                let _ = std::fs::remove_dir_all(&staged);
                std::fs::create_dir_all(&staged)?;
                let mut archive = open()?;
                for i in 0..archive.len() {
                    let mut entry = archive.by_index(i).map_err(|e| Error::Tool(e.to_string()))?;
                    let Some(path) = entry.enclosed_name() else { continue };
                    let out = staged.join(path);
                    if entry.is_dir() {
                        std::fs::create_dir_all(&out)?;
                        continue;
                    }
                    if let Some(parent) = out.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    io::copy(&mut entry, &mut File::create(&out)?)?;
                    // The program and its helpers keep their execute bits.
                    #[cfg(unix)]
                    if let Some(mode) = entry.unix_mode() {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode & 0o777))?;
                    }
                }
                if !staged.join(dest.file_name().expect("the program has a name")).is_file() {
                    return Err(Error::Tool(format!("no {} in {}", dest.display(), fetched.display())));
                }
                swap_folder(&staged, &folder)
            }
        }
    })
    .await
    .map_err(io::Error::other)?
}

/// Puts `staged` in the place of `folder` as a whole, so the program never runs from a
/// half-replaced folder: the old folder is first moved aside in one step, which fails
/// without touching anything while the program runs (Windows keeps a running program's
/// folder busy), and only then deleted.
fn swap_folder(staged: &Path, folder: &Path) -> Result<()> {
    let aside = folder.with_extension("old");
    let _ = std::fs::remove_dir_all(&aside);
    if folder.exists() {
        std::fs::rename(folder, &aside)
            .map_err(|e| Error::Tool(format!("could not replace {} (is it running?): {e}", folder.display())))?;
    }
    if let Err(e) = std::fs::rename(staged, folder) {
        // Put the old one back rather than leave nothing.
        let _ = std::fs::rename(&aside, folder);
        return Err(e.into());
    }
    let _ = std::fs::remove_dir_all(&aside);
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn place(program: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(program, dest)?;
    Ok(())
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fails unless `sig` is yt-dlp's maintainers' signature over `data`.
pub(crate) fn verify_signature(data: &[u8], sig: &[u8]) -> Result<()> {
    let (key, _) = SignedPublicKey::from_string(YT_DLP_KEY).map_err(|e| Error::Tool(format!("signing key: {e}")))?;
    let sig = DetachedSignature::from_bytes(sig).map_err(|e| Error::Tool(format!("signature: {e}")))?;
    sig.verify(&key, data).map_err(|_| Error::Tool("the checksum list's signature is not yt-dlp's".into()))
}

/// Hash for `name` in a `<hash>  <name>` list.
pub(crate) fn hash_in_list(list: &str, name: &str) -> Option<String> {
    list.lines().find_map(|line| {
        let (hash, file) = line.split_once(char::is_whitespace)?;
        (file.trim_start().trim_start_matches('*') == name && is_sha256(hash)).then(|| hash.to_ascii_lowercase())
    })
}

/// The first SHA-256 value in a checksum file, whatever its layout (Deno's Windows files
/// are PowerShell `Get-FileHash` output, the others `<hash>  <name>`).
pub(crate) fn first_hash(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit()).find(|w| is_sha256(w)).map(str::to_ascii_lowercase)
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMS: &[u8] = include_bytes!("../tests/fixtures/yt-dlp-2026.08.19-SHA2-256SUMS");
    const SIG: &[u8] = include_bytes!("../tests/fixtures/yt-dlp-2026.08.19-SHA2-256SUMS.sig");

    #[test]
    fn real_yt_dlp_signature_verifies() {
        verify_signature(SUMS, SIG).unwrap();
    }

    #[test]
    fn changed_list_fails_the_signature() {
        let mut forged = SUMS.to_vec();
        forged[0] = if forged[0] == b'0' { b'1' } else { b'0' };
        assert!(verify_signature(&forged, SIG).is_err());
    }

    #[test]
    fn hash_lookup_in_list() {
        let list = std::str::from_utf8(SUMS).unwrap();
        assert_eq!(hash_in_list(list, "yt-dlp.exe").as_deref(), Some("66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a"));
        assert_eq!(hash_in_list(list, "yt-dlp"), Some("1fa6733c37ea6fb51c99ad8fe785e7b7e5f3246c9b980230329d4fb72ed8d4d6".into()));
        assert_eq!(hash_in_list(list, "missing.exe"), None);
        assert_eq!(hash_in_list(list, "yt-dlp_win.zip").as_deref(), Some("30b4c14aafab6082becff7881e41b76df46dc43ea7633479410a91e29da492bf"));
        assert_eq!(hash_in_list(list, "yt-dlp_macos.zip").as_deref(), Some("07e54b0865303c864006925913bce2604f8ee8cc6f18699bac9c309f9328a6d8"));
    }

    /// yt-dlp's folder build: the whole zip becomes the folder (replacing the old one), and
    /// the one-file program of earlier versions is used only until then.
    #[tokio::test]
    async fn folder_build_replaces_the_program_folder() {
        let tmp = testserver::Folder::new("tools-folder");
        let dir = tmp.path().to_owned();
        std::fs::create_dir_all(dir.join("yt-dlp")).unwrap();
        std::fs::write(dir.join("yt-dlp").join("stale.dll"), b"old").unwrap();
        let tools = Tools::new(dir.clone()).unwrap();
        assert_eq!(tools.path(Tool::YtDlp), None);
        if let Some(old) = Tool::YtDlp.old_file() {
            let old = dir.join(old);
            std::fs::write(&old, b"one-file build").unwrap();
            assert_eq!(tools.path(Tool::YtDlp), Some(old), "the old program until the update");
        }
        assert_eq!(tools.installed(Tool::YtDlp), None, "so the update installs the folder build");

        let program = Tool::YtDlp.file();
        let inside = program.file_name().unwrap().to_str().unwrap().to_owned();
        let zip_path = dir.join("yt-dlp.zip");
        {
            let mut w = zip::ZipWriter::new(File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            w.add_directory("_internal/", opts).unwrap();
            for (name, data) in [(inside.as_str(), "program"), ("_internal/python313.dll", "python"), ("_internal/lib/x.pyd", "module")] {
                w.start_file(name, opts.unix_permissions(0o755)).unwrap();
                io::Write::write_all(&mut w, data.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        let hash = sha256_file(&zip_path).unwrap();
        install(&zip_path, &hash, Unpack::Folder, &dir.join(&program)).await.unwrap();
        assert_eq!(std::fs::read(dir.join(&program)).unwrap(), b"program");
        assert_eq!(std::fs::read(dir.join("yt-dlp").join("_internal").join("lib").join("x.pyd")).unwrap(), b"module");
        assert!(!dir.join("yt-dlp").join("stale.dll").exists() && !dir.join("yt-dlp.new").exists());
        assert_eq!(tools.path(Tool::YtDlp), Some(dir.join(&program)), "the folder build comes first");
    }

    /// The hash every install is checked with, against values from elsewhere (Python's
    /// hashlib): the install tests here take the hashes they expect from this function.
    #[test]
    fn file_hashes() {
        let tmp = testserver::Folder::new("tools-hash");
        let path = tmp.path().join("file");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // More than it reads at once.
        std::fs::write(&path, vec![b'a'; 3_000_000]).unwrap();
        assert_eq!(sha256_file(&path).unwrap(), "2a152c894398719c0570f83fac34ac03a0f6e8e474b995c2403aa5434f7b9dd4");
    }

    #[test]
    fn hash_from_either_checksum_layout() {
        let powershell = "\r\nAlgorithm : SHA256\r\nHash      : A0C3101B4158D1DFB7D6A78A7BF0F3DE80C96BB423C152BEEC8BEB22786F2238\r\nPath      : C:\\a\\deno\\deno-x86_64-pc-windows-msvc.zip\r\n";
        let plain = "a0c3101b4158d1dfb7d6a78a7bf0f3de80c96bb423c152beec8beb22786f2238  deno-aarch64-apple-darwin.zip\n";
        let want = Some("a0c3101b4158d1dfb7d6a78a7bf0f3de80c96bb423c152beec8beb22786f2238".to_string());
        assert_eq!(first_hash(powershell), want);
        assert_eq!(first_hash(plain), want);
    }

    #[tokio::test]
    async fn install_checks_hash_and_unpacks() {
        let tmp = testserver::Folder::new("tools-install");
        let dir = tmp.path().to_owned();
        let zip_path = dir.join("tool.zip");
        {
            let mut w = zip::ZipWriter::new(File::create(&zip_path).unwrap());
            w.start_file("deno.exe", zip::write::SimpleFileOptions::default()).unwrap();
            io::Write::write_all(&mut w, b"program bytes").unwrap();
            w.finish().unwrap();
        }
        let good = sha256_file(&zip_path).unwrap();
        let dest = dir.join("deno.exe");

        let wrong = "0".repeat(64);
        assert!(install(&zip_path, &wrong, Unpack::One("deno.exe"), &dest).await.is_err());
        assert!(!dest.exists());

        install(&zip_path, &good, Unpack::One("deno.exe"), &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"program bytes");
    }

    #[test]
    fn daily_check_is_due_after_a_day() {
        let day = CHECK_EVERY_SECS;
        assert!(due(0, 1_791_000_000), "never checked");
        assert!(!due(1_791_000_000, 1_791_000_000 + day - 1));
        assert!(due(1_791_000_000, 1_791_000_000 + day));
        assert!(!due(1_791_000_000, 1_790_000_000), "a clock set back waits");
        // Records written before the field existed read as never checked.
        let old: Installed = serde_json::from_str(r#"{"version":"2026.08.19","nightly":false}"#).unwrap();
        assert_eq!(old.checked, 0);
    }

    /// A folder whose program runs cannot be moved aside on Windows: the update fails and
    /// the old folder stays whole.
    #[cfg(windows)]
    #[test]
    fn busy_folder_is_left_whole() {
        let tmp = testserver::Folder::new("tools-busy");
        let dir = tmp.path().to_owned();
        let (folder, staged) = (dir.join("yt-dlp"), dir.join("yt-dlp.new"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(folder.join("a.dll"), b"old a").unwrap();
        std::fs::write(folder.join("b.dll"), b"old b").unwrap();
        std::fs::write(staged.join("a.dll"), b"new a").unwrap();
        // Held open without sharing delete, as a loaded library or running program is.
        let held = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().read(true).share_mode(1).open(folder.join("b.dll")).unwrap()
        };
        assert!(swap_folder(&staged, &folder).is_err());
        assert_eq!(std::fs::read(folder.join("a.dll")).unwrap(), b"old a");
        assert_eq!(std::fs::read(folder.join("b.dll")).unwrap(), b"old b");
        drop(held);
        swap_folder(&staged, &folder).unwrap();
        assert_eq!(std::fs::read(folder.join("a.dll")).unwrap(), b"new a");
        assert!(!folder.join("b.dll").exists() && !dir.join("yt-dlp.old").exists() && !staged.exists());
    }
}
