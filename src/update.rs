//! `porch self-update` and the passive "newer release available" notice.
//!
//! Homebrew-managed binaries are upgraded through `brew`. Other installs fetch
//! the latest GitHub Release, verify the platform tarball against the release's
//! `SHA256SUMS`, and atomically replace the running executable (plus
//! `porch-tray`/`porch-login` beside it and, on macOS, the installed
//! `Porch.app`). Nothing here restarts the proxy, apps, or the tray.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_REPO: &str = "akshitkrnagpal/porch";
pub const BREW_FORMULA: &str = "akshitkrnagpal/tap/porch";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_API: &str = "https://api.github.com";
const SUMS_ASSET: &str = "SHA256SUMS";
const CACHE_FILE: &str = "update-check.json";
const DAY: u64 = 24 * 60 * 60;
const RETRY_AFTER_FAILURE: u64 = 60 * 60;
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(2);
const FINISH_WAIT: Duration = Duration::from_millis(500);
const APP_BUNDLE_ID: &str = "dev.porch.tray";
const COMPANIONS: [&str; 2] = ["porch-tray", "porch-login"];

/// Executable file name on this platform (`porch` or `porch.exe`).
fn exe_name(base: &str) -> String {
    format!("{base}{}", std::env::consts::EXE_SUFFIX)
}

#[derive(clap::Args, Debug, Default, Clone)]
pub struct SelfUpdateArgs {
    /// Only compare versions: exit 0 when up to date, 1 when an update is available, 2 on error.
    #[arg(long)]
    pub check: bool,
    /// Print what would happen without downloading, running brew, or replacing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Reinstall even when already on the latest release, or when running a source build.
    #[arg(long)]
    pub force: bool,
}

// ---------------------------------------------------------------------------
// Versions and assets

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Option<String>,
    text: String,
}

impl Version {
    pub fn parse(raw: &str) -> Option<Self> {
        let text = raw.trim().trim_start_matches('v');
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_owned())),
            Some(_) => return None,
            None => (text.split('+').next()?, None),
        };
        let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
        let parsed = [parts.next()??, parts.next()??, parts.next()??];
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            core: parsed,
            pre,
            text: text.to_owned(),
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

fn current_version() -> Version {
    Version::parse(CURRENT_VERSION).expect("Cargo package version is semver")
}

/// The release target triple of this build, when Porch publishes one.
pub fn current_target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        // Only a static build is published for ARM64 Linux.
        Some("aarch64-unknown-linux-musl")
    } else if cfg!(all(windows, target_arch = "x86_64", target_env = "msvc")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

pub fn asset_name(version: &Version, target: &str) -> String {
    format!("porch-{version}-{target}.tar.gz")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Asset {
    pub name: String,
    /// API URL; with `Accept: application/octet-stream` it serves the bytes
    /// for public and (authenticated) private repositories alike.
    pub url: String,
    #[serde(default)]
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn version(&self) -> Result<Version> {
        Version::parse(&self.tag_name)
            .with_context(|| format!("latest release tag {:?} is not a version", self.tag_name))
    }
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// Pick the tarball for `target` from a release.
pub fn select_asset<'a>(release: &'a Release, target: &str) -> Result<&'a Asset> {
    let name = asset_name(&release.version()?, target);
    release.asset(&name).with_context(|| {
        let available: Vec<_> = release.assets.iter().map(|a| a.name.as_str()).collect();
        format!(
            "release {} has no {name} (available: {})",
            release.tag_name,
            if available.is_empty() {
                "none".to_owned()
            } else {
                available.join(", ")
            }
        )
    })
}

// ---------------------------------------------------------------------------
// SHA256SUMS

/// Parse `sha256sum` output: `<64 hex>  <name>` (or `*<name>` binary mode).
pub fn parse_sums(text: &str) -> Result<BTreeMap<String, String>> {
    let mut sums = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (digest, name) = line
            .split_once(char::is_whitespace)
            .with_context(|| format!("SHA256SUMS line {} is malformed", number + 1))?;
        let name = name.trim().trim_start_matches('*');
        if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) || name.is_empty() {
            bail!("SHA256SUMS line {} is malformed", number + 1);
        }
        let digest = digest.to_ascii_lowercase();
        if let Some(previous) = sums.insert(name.to_owned(), digest.clone()) {
            if previous != digest {
                bail!("SHA256SUMS lists {name} twice with different digests");
            }
        }
    }
    Ok(sums)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn verify_checksum(bytes: &[u8], name: &str, sums: &BTreeMap<String, String>) -> Result<()> {
    let expected = sums
        .get(name)
        .with_context(|| format!("SHA256SUMS has no entry for {name}"))?;
    let actual = sha256_hex(bytes);
    if &actual != expected {
        bail!("checksum mismatch for {name}: expected {expected}, got {actual}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Build provenance

/// Check the archive's GitHub build attestation with `gh attestation verify`.
///
/// `SHA256SUMS` only proves the download is intact; the attestation proves the
/// release workflow in the Porch repository built it. Verification runs when
/// the GitHub CLI is installed, and `PORCH_UPDATE_VERIFY=require` makes it
/// mandatory. A failed verification always stops the update.
fn verify_provenance(github: &GitHub, tarball: &Path, out: &mut dyn Write) -> Result<()> {
    if github.api != DEFAULT_API {
        // A mirror or test server: attestations live on github.com only.
        return Ok(());
    }
    let required = env_nonempty("PORCH_UPDATE_VERIFY").is_some_and(|v| v == "require");
    let gh = Command::new("gh")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !gh {
        if required {
            bail!("PORCH_UPDATE_VERIFY=require needs the GitHub CLI (gh) to check the build attestation");
        }
        writeln!(
            out,
            "Skipped build attestation check (install the GitHub CLI to enable it)"
        )?;
        return Ok(());
    }
    let workflow = format!("{}/.github/workflows/release.yml", github.repo);
    let result = Command::new("gh")
        .args(["attestation", "verify"])
        .arg(tarball)
        .args(["--repo", &github.repo, "--signer-workflow", &workflow])
        .stdin(std::process::Stdio::null())
        .output()
        .context("cannot run gh attestation verify")?;
    if !result.status.success() {
        bail!(
            "the release archive's build attestation did not verify; not installing it.\n{}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    writeln!(out, "Verified build attestation from {workflow}")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// GitHub client

pub struct GitHub {
    client: reqwest::Client,
    api: String,
    repo: String,
    token: Option<String>,
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl GitHub {
    pub fn new(api: &str, repo: &str, token: Option<String>, timeout: Duration) -> Result<GitHub> {
        if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
            bail!("update repository must look like owner/name, got {repo:?}");
        }
        let client = reqwest::Client::builder()
            .user_agent(format!("porch/{CURRENT_VERSION}"))
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .timeout(timeout)
            .build()?;
        Ok(GitHub {
            client,
            api: api.trim_end_matches('/').to_owned(),
            repo: repo.to_owned(),
            token,
        })
    }

    /// Repository from `PORCH_UPDATE_REPO`, API base from `PORCH_UPDATE_API`,
    /// token from `GITHUB_TOKEN`/`GH_TOKEN` (and, when allowed, `gh auth token`).
    pub fn from_env(timeout: Duration, allow_gh_cli: bool) -> Result<GitHub> {
        let repo = env_nonempty("PORCH_UPDATE_REPO").unwrap_or_else(|| DEFAULT_REPO.to_owned());
        let api = env_nonempty("PORCH_UPDATE_API").unwrap_or_else(|| DEFAULT_API.to_owned());
        let mut token = env_nonempty("GITHUB_TOKEN").or_else(|| env_nonempty("GH_TOKEN"));
        if token.is_none() && allow_gh_cli {
            token = Command::new("gh")
                .args(["auth", "token"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty());
        }
        GitHub::new(&api, &repo, token, timeout)
    }

    fn request(&self, url: &str, accept: &str) -> reqwest::RequestBuilder {
        let request = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    pub async fn latest(&self) -> Result<Release> {
        let url = format!("{}/repos/{}/releases/latest", self.api, self.repo);
        let response = self
            .request(&url, "application/vnd.github+json")
            .send()
            .await
            .with_context(|| format!("cannot reach {url}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            if self.token.is_none() {
                bail!(
                    "no published release found for {} (the repository may be private: set GITHUB_TOKEN or GH_TOKEN)",
                    self.repo
                );
            }
            bail!("no published release found for {}", self.repo);
        }
        if !status.is_success() {
            bail!("GitHub API returned {status} for {url}");
        }
        response
            .json()
            .await
            .context("unexpected GitHub release response")
    }

    pub async fn download(&self, asset: &Asset) -> Result<Vec<u8>> {
        // reqwest drops Authorization when the asset redirect leaves the API host.
        let response = self
            .request(&asset.url, "application/octet-stream")
            .send()
            .await
            .with_context(|| format!("cannot download {}", asset.name))?;
        if !response.status().is_success() {
            bail!("downloading {} returned {}", asset.name, response.status());
        }
        Ok(response.bytes().await?.to_vec())
    }
}

// ---------------------------------------------------------------------------
// Install locations

/// True when `exe` comes from the porch-cli npm packages, which the project's
/// package manager owns and pins.
pub fn is_npm_path(exe: &Path) -> bool {
    exe.components().any(|c| c.as_os_str() == "node_modules")
        && exe
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with("porch-cli-"))
}

/// True when `exe` (already canonicalized) lives in a Homebrew Cellar.
pub fn is_homebrew_path(exe: &Path, prefixes: &[PathBuf]) -> bool {
    if prefixes
        .iter()
        .any(|prefix| exe.starts_with(prefix.join("Cellar")))
    {
        return true;
    }
    // Custom prefixes: <prefix>/Cellar/porch/<version>/bin/porch.
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    parts
        .windows(2)
        .any(|w| w[0] == "Cellar" && w[1] == "porch")
}

fn homebrew_prefixes() -> Vec<PathBuf> {
    let mut prefixes: Vec<PathBuf> = ["HOMEBREW_PREFIX"]
        .iter()
        .filter_map(|k| env_nonempty(k).map(PathBuf::from))
        .collect();
    if let Some(cellar) = env_nonempty("HOMEBREW_CELLAR") {
        if let Some(parent) = Path::new(&cellar).parent() {
            prefixes.push(parent.to_owned());
        }
    }
    if let Some(prefix) = Command::new("brew")
        .arg("--prefix")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    {
        prefixes.push(PathBuf::from(prefix));
    }
    prefixes.extend(
        ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"]
            .iter()
            .map(PathBuf::from),
    );
    prefixes
}

/// A binary inside a Cargo `target/` directory is a source build.
pub fn is_source_build(exe: &Path) -> bool {
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    parts.windows(2).any(|w| {
        w[0] == "target"
            && (w[1] == "debug" || w[1] == "release" || w[1].to_string_lossy().contains('-'))
    })
}

fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate the running porch executable")?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

/// Where scripts/install.py puts the app: `~/Applications/Porch.app`
/// (`PORCH_APP_DIR` overrides the directory).
fn installed_app() -> Option<PathBuf> {
    let dir = env_nonempty("PORCH_APP_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Applications")))?;
    let app = dir.join("Porch.app");
    is_porch_app(&app).then_some(app)
}

fn is_porch_app(app: &Path) -> bool {
    fs::read_to_string(app.join("Contents/Info.plist"))
        .is_ok_and(|plist| plist.contains(&format!("<string>{APP_BUNDLE_ID}</string>")))
}

// ---------------------------------------------------------------------------
// Atomic replacement

struct TempDir(PathBuf);

impl TempDir {
    fn new(parent: &Path, label: &str) -> Result<TempDir> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = parent.join(format!(".porch-{label}-{}-{nanos}", std::process::id()));
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        Ok(TempDir(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

fn backup_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    dest.with_file_name(name)
}

/// Replace `dest` with `source`: copy to a temp file beside `dest`, fsync,
/// keep `dest.old` as a backup, rename into place, then run `verify`. On
/// failure the backup is restored; on success it is removed. A process that
/// still has the old executable open keeps running from the old inode.
pub fn replace_executable(
    source: &Path,
    dest: &Path,
    verify: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let dir = dest
        .parent()
        .with_context(|| format!("{} has no parent directory", dest.display()))?;
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let staging = dir.join(format!(".{name}.update-{}", std::process::id()));
    let backup = backup_path(dest);
    let _ = fs::remove_file(&staging);
    let result = (|| -> Result<()> {
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o755);
            }
            let mut output = options.open(&staging).with_context(|| {
                format!(
                    "cannot write in {} (reinstall with sufficient permissions)",
                    dir.display()
                )
            })?;
            let mut input = fs::File::open(source)?;
            std::io::copy(&mut input, &mut output)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                output.set_permissions(fs::Permissions::from_mode(0o755))?;
            }
            output.sync_all()?;
        }
        let had_original = dest.exists();
        #[cfg(unix)]
        if had_original {
            let _ = fs::remove_file(&backup);
            if fs::hard_link(dest, &backup).is_err() {
                fs::copy(dest, &backup)
                    .with_context(|| format!("cannot back up {}", dest.display()))?;
            }
        }
        // A running executable cannot be replaced or deleted on Windows, but it
        // can be renamed. Move it aside; a backup from an earlier update that is
        // still running keeps its name and this one gets a unique one.
        #[cfg(windows)]
        let backup = if had_original {
            let _ = fs::remove_file(&backup);
            let aside = if backup.exists() {
                dest.with_file_name(format!("{name}.{}.old", std::process::id()))
            } else {
                backup
            };
            fs::rename(dest, &aside)
                .with_context(|| format!("cannot move {} aside", dest.display()))?;
            aside
        } else {
            backup
        };
        if let Err(error) = fs::rename(&staging, dest) {
            #[cfg(windows)]
            if had_original {
                let _ = fs::rename(&backup, dest);
            }
            return Err(error).with_context(|| format!("cannot replace {}", dest.display()));
        }
        sync_dir(dir);
        if let Err(error) = verify(dest) {
            if had_original {
                fs::rename(&backup, dest).with_context(|| {
                    format!(
                        "verification failed ({error:#}) and restoring {} also failed",
                        backup.display()
                    )
                })?;
                sync_dir(dir);
            }
            return Err(error.context(format!("new {} failed verification", dest.display())));
        }
        let _ = fs::remove_file(&backup);
        sync_dir(dir);
        Ok(())
    })();
    let _ = fs::remove_file(&staging);
    result
}

fn copy_dir(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = dest.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Replace an installed Porch.app bundle with `source`, staged beside it.
/// Refuses to replace a bundle that is not Porch's.
pub fn replace_app(source: &Path, dest: &Path) -> Result<()> {
    if dest.exists() && !is_porch_app(dest) {
        bail!("refusing to replace unrelated app at {}", dest.display());
    }
    let parent = dest
        .parent()
        .with_context(|| format!("{} has no parent directory", dest.display()))?;
    let staging = TempDir::new(parent, "app-update")?;
    let staged = staging.0.join("Porch.app");
    copy_dir(source, &staged).context("cannot stage the new Porch.app")?;
    let backup = staging.0.join("Porch.app.old");
    let had_original = dest.exists();
    if had_original {
        fs::rename(dest, &backup)?;
    }
    if let Err(error) = fs::rename(&staged, dest) {
        if had_original {
            fs::rename(&backup, dest)?;
        }
        return Err(error).with_context(|| format!("cannot replace {}", dest.display()));
    }
    sync_dir(parent);
    Ok(())
}

fn version_check(expected: Version) -> impl FnOnce(&Path) -> Result<()> {
    move |binary| {
        let output = Command::new(binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("cannot run {}", binary.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() || !stdout.split_whitespace().any(|w| w == expected.text) {
            bail!(
                "`{} --version` printed {:?}",
                binary.display(),
                stdout.trim()
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Self-update

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate(String),
    Planned(String),
    Updated(String),
}

/// Download, verify, and install the latest release over `exe` (and its
/// companions beside it) and `app` when given. Testable with temp paths.
pub async fn perform(
    github: &GitHub,
    target: &str,
    exe: &Path,
    app: Option<&Path>,
    args: &SelfUpdateArgs,
    out: &mut dyn Write,
) -> Result<Outcome> {
    let release = github.latest().await?;
    let latest = release.version()?;
    let current = current_version();
    if latest <= current && !args.force {
        writeln!(out, "porch {current} is up to date.")?;
        return Ok(Outcome::UpToDate(latest.text));
    }
    let asset = select_asset(&release, target)?;
    let sums_asset = release
        .asset(SUMS_ASSET)
        .with_context(|| format!("release {} has no {SUMS_ASSET}", release.tag_name))?;
    if args.dry_run {
        writeln!(out, "Would download {} from {}", asset.name, github.repo)?;
        writeln!(out, "Would verify it against {SUMS_ASSET}")?;
        writeln!(out, "Would replace {}", exe.display())?;
        if let Some(app) = app {
            writeln!(out, "Would replace {}", app.display())?;
        }
        return Ok(Outcome::Planned(latest.text));
    }
    writeln!(out, "Downloading {} ...", asset.name)?;
    let sums = parse_sums(&String::from_utf8_lossy(
        &github.download(sums_asset).await?,
    ))?;
    let archive = github.download(asset).await?;
    verify_checksum(&archive, &asset.name, &sums)?;
    writeln!(out, "Verified SHA-256 {}", sha256_hex(&archive))?;

    let exe_dir = exe
        .parent()
        .with_context(|| format!("{} has no parent directory", exe.display()))?;
    let work = TempDir::new(&std::env::temp_dir(), "update")?;
    let tarball = work.0.join(&asset.name);
    fs::write(&tarball, &archive)?;
    verify_provenance(github, &tarball, out)?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work.0)
        .stdin(std::process::Stdio::null())
        .status()
        .context("cannot run tar to extract the release")?;
    if !status.success() {
        bail!("tar could not extract {}", asset.name);
    }
    let package = work.0.join(asset.name.trim_end_matches(".tar.gz"));
    let new_porch = package.join(exe_name("porch"));
    if !new_porch.is_file() {
        bail!("{} does not contain porch", asset.name);
    }
    replace_executable(&new_porch, exe, version_check(latest.clone()))?;
    writeln!(out, "Updated {} to {latest}", exe.display())?;
    for companion in COMPANIONS {
        // porch-login is a script on every platform; the tray is an executable.
        let companion = if companion == "porch-tray" {
            exe_name(companion)
        } else {
            companion.to_owned()
        };
        let (source, dest) = (package.join(&companion), exe_dir.join(&companion));
        if source.is_file() && dest.is_file() {
            replace_executable(&source, &dest, |_| Ok(()))?;
            writeln!(out, "Updated {}", dest.display())?;
        }
    }
    if let Some(app) = app {
        let source = package.join("Porch.app");
        if source.is_dir() {
            replace_app(&source, app)?;
            writeln!(
                out,
                "Updated {} (quit and reopen Porch to use it)",
                app.display()
            )?;
        }
    }
    Ok(Outcome::Updated(latest.text))
}

fn proxy_running() -> Option<u32> {
    let identity: crate::state::ProcessIdentity =
        serde_json::from_slice(&fs::read(crate::state::dir().join("proxy.identity")).ok()?).ok()?;
    crate::state::matches(&identity).then_some(identity.pid)
}

async fn check(github: &GitHub) -> Result<i32> {
    let latest = github.latest().await?.version()?;
    let current = current_version();
    remember(&latest);
    if latest > current {
        println!("porch {current} (current)\nporch {latest} (latest)\nUpdate available: run porch self-update");
        Ok(1)
    } else {
        println!("porch {current} is up to date (latest release {latest}).");
        Ok(0)
    }
}

pub async fn run(args: SelfUpdateArgs) -> Result<i32> {
    if args.check {
        let github = GitHub::from_env(Duration::from_secs(30), true)?;
        return Ok(match check(&github).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("porch: {error:#}");
                2
            }
        });
    }
    let exe = current_exe()?;
    if is_npm_path(&exe) {
        println!("porch was installed from npm (porch-cli); update it with your package manager, for example:\n  npm install --save-dev porch-cli@latest   (or pnpm up porch-cli, yarn up porch-cli, npm i -g porch-cli@latest)");
        return Ok(0);
    }
    if is_homebrew_path(&exe, &homebrew_prefixes()) {
        println!("porch is managed by Homebrew; running: brew upgrade {BREW_FORMULA}");
        if args.dry_run {
            return Ok(0);
        }
        let status = Command::new("brew")
            .args(["upgrade", BREW_FORMULA])
            .status()
            .context("cannot run brew")?;
        if status.success() {
            note_proxy_restart();
        }
        return Ok(status.code().unwrap_or(1));
    }
    if is_source_build(&exe) && !args.force {
        bail!(
            "{} looks like a source build; rebuild it with cargo, or pass --force to replace it with a release",
            exe.display()
        );
    }
    let target = current_target().context("no prebuilt Porch release exists for this platform")?;
    let github = GitHub::from_env(Duration::from_secs(300), true)?;
    let app = if cfg!(target_os = "macos") {
        installed_app()
    } else {
        None
    };
    let outcome = perform(
        &github,
        target,
        &exe,
        app.as_deref(),
        &args,
        &mut std::io::stdout(),
    )
    .await?;
    if let Outcome::Updated(version) = outcome {
        if let Some(version) = Version::parse(&version) {
            remember(&version);
        }
        note_proxy_restart();
    }
    Ok(0)
}

fn note_proxy_restart() {
    if let Some(pid) = proxy_running() {
        println!(
            "The running Porch proxy (pid {pid}) is still the previous version; it will be restarted on next use."
        );
    }
}

// ---------------------------------------------------------------------------
// Passive update notice

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NoticeCache {
    /// Last successful check (unix seconds).
    pub checked_at: u64,
    /// Last attempted check, successful or not.
    pub attempted_at: u64,
    pub latest: Option<String>,
    /// Last time the notice line was printed.
    pub notified_at: u64,
}

impl NoticeCache {
    /// A network check is due at most once per day, and at most hourly after failures.
    pub fn check_due(&self, now: u64) -> bool {
        now.saturating_sub(self.checked_at) >= DAY
            && now.saturating_sub(self.attempted_at) >= RETRY_AFTER_FAILURE
    }

    /// The notice to print now, if a newer release is known and none was shown today.
    pub fn notice_due(&self, current: &str, now: u64) -> Option<String> {
        let latest = Version::parse(self.latest.as_deref()?)?;
        let current = Version::parse(current)?;
        (latest > current && now.saturating_sub(self.notified_at) >= DAY)
            .then(|| notice_message(&latest.text))
    }
}

pub fn notice_message(latest: &str) -> String {
    format!("porch {latest} is available — run porch self-update")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_cache(path: &Path) -> NoticeCache {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_cache(path: &Path, cache: &NoticeCache) {
    let Some(dir) = path.parent() else { return };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let temporary = dir.join(format!("{CACHE_FILE}.{}.tmp", std::process::id()));
    let written = serde_json::to_vec(cache)
        .ok()
        .and_then(|bytes| fs::write(&temporary, bytes).ok())
        .and_then(|_| fs::rename(&temporary, path).ok());
    if written.is_none() {
        let _ = fs::remove_file(&temporary);
    }
}

fn cache_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CACHE_FILE)
}

/// Record a freshly observed latest version (after `--check` or an update).
fn remember(latest: &Version) {
    let path = cache_path(&crate::state::dir());
    let mut cache = read_cache(&path);
    cache.latest = Some(latest.text.clone());
    cache.checked_at = now();
    cache.attempted_at = cache.checked_at;
    write_cache(&path, &cache);
}

fn truthy(key: &str) -> bool {
    env_nonempty(key).is_some_and(|v| !matches!(v.as_str(), "0" | "false" | "no" | "off"))
}

/// Whether this invocation may show a passive notice at all.
pub fn notice_allowed(args: &[String], stderr_tty: bool) -> bool {
    if !stderr_tty || truthy("PORCH_NO_UPDATE_CHECK") || truthy("CI") {
        return false;
    }
    // A project pins its porch-cli version; the package manager updates it.
    if std::env::current_exe().is_ok_and(|e| is_npm_path(&e)) {
        return false;
    }
    // `--json` after `--` belongs to a child command, not to porch.
    if args
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--json")
    {
        return false;
    }
    first_command(args).is_none_or(|c| !QUIET.contains(&c))
}

/// Commands that speak protocols or are long-lived background helpers.
const QUIET: [&str; 4] = ["self-update", "mcp", "tray", "help"];

fn first_command(args: &[String]) -> Option<&str> {
    let mut iter = args.iter().take_while(|a| *a != "--");
    while let Some(arg) = iter.next() {
        if arg == "--state-dir" {
            iter.next();
        } else if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}

fn state_dir_from(args: &[String]) -> PathBuf {
    let mut iter = args.iter().take_while(|a| *a != "--");
    while let Some(arg) = iter.next() {
        if arg == "--state-dir" {
            if let Some(dir) = iter.next() {
                return PathBuf::from(dir);
            }
        } else if let Some(dir) = arg.strip_prefix("--state-dir=") {
            return PathBuf::from(dir);
        }
    }
    crate::state::dir()
}

type Pending = std::thread::JoinHandle<Option<String>>;

/// Passive notice handle: created before the command runs, finished after.
pub struct Notice {
    path: PathBuf,
    cache: NoticeCache,
    pending: Option<Pending>,
}

impl Notice {
    /// Start a background check if one is due. Never blocks on the network.
    pub fn start(args: &[String]) -> Option<Notice> {
        use std::io::IsTerminal;
        if !notice_allowed(args, std::io::stderr().is_terminal()) {
            return None;
        }
        #[cfg(unix)]
        if unsafe { libc::geteuid() } == 0 {
            return None; // Never create root-owned files in a user's state dir.
        }
        let path = cache_path(&state_dir_from(args));
        let mut cache = read_cache(&path);
        let now = now();
        let mut pending = None;
        if cache.check_due(now) {
            if let Ok(github) = GitHub::from_env(BACKGROUND_TIMEOUT, false) {
                cache.attempted_at = now;
                write_cache(&path, &cache);
                pending = std::thread::Builder::new()
                    .name("porch-update-check".into())
                    .spawn(move || {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .ok()?;
                        let release = runtime
                            .block_on(async {
                                tokio::time::timeout(BACKGROUND_TIMEOUT, github.latest()).await
                            })
                            .ok()?
                            .ok()?;
                        Some(release.version().ok()?.text)
                    })
                    .ok();
            }
        }
        Some(Notice {
            path,
            cache,
            pending,
        })
    }

    /// Collect a finished background check (waiting briefly) and print the
    /// dim notice line to stderr if one is due.
    pub fn finish(mut self) {
        if let Some(handle) = self.pending.take() {
            let deadline = std::time::Instant::now() + FINISH_WAIT;
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if handle.is_finished() {
                if let Ok(Some(latest)) = handle.join() {
                    // Re-read: a concurrent porch may have written in the meantime.
                    self.cache = read_cache(&self.path);
                    self.cache.latest = Some(latest);
                    self.cache.checked_at = now();
                    write_cache(&self.path, &self.cache);
                }
            }
        }
        let now = now();
        if let Some(message) = self.cache.notice_due(CURRENT_VERSION, now) {
            if std::env::var_os("NO_COLOR").is_some() {
                eprintln!("{message}");
            } else {
                eprintln!("\x1b[2m{message}\x1b[0m");
            }
            let mut cache = read_cache(&self.path);
            cache.notified_at = now;
            write_cache(&self.path, &cache);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> TempDir {
        TempDir::new(&std::env::temp_dir(), label).unwrap()
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn versions_order_semantically() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("v0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.2") > v("1.0.0-rc.1"));
        assert_eq!(v("v0.3.1").to_string(), "0.3.1");
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("latest").is_none());
        assert!(Version::parse(CURRENT_VERSION).is_some());
    }

    fn release(tag: &str, names: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            assets: names
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    url: format!("https://api.example/{n}"),
                    browser_download_url: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn asset_selection_matches_version_and_target() {
        let r = release(
            "v0.3.1",
            &[
                "SHA256SUMS",
                "porch-0.3.1-aarch64-apple-darwin.tar.gz",
                "porch-0.3.1-x86_64-unknown-linux-gnu.tar.gz",
            ],
        );
        assert_eq!(
            select_asset(&r, "aarch64-apple-darwin").unwrap().name,
            "porch-0.3.1-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            select_asset(&r, "x86_64-unknown-linux-gnu").unwrap().name,
            "porch-0.3.1-x86_64-unknown-linux-gnu.tar.gz"
        );
        let missing = select_asset(&r, "x86_64-apple-darwin").unwrap_err();
        assert!(missing
            .to_string()
            .contains("porch-0.3.1-x86_64-apple-darwin.tar.gz"));
        // An asset from another version never matches.
        let stale = release("v0.4.0", &["porch-0.3.1-aarch64-apple-darwin.tar.gz"]);
        assert!(select_asset(&stale, "aarch64-apple-darwin").is_err());
        assert!(select_asset(&release("nightly", &[]), "aarch64-apple-darwin").is_err());
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(current_target(), Some("aarch64-apple-darwin"));
        }
    }

    #[test]
    fn sums_parse_and_verify() {
        let good = sha256_hex(b"archive");
        let text = format!(
            "# comment\n{good}  porch-a.tar.gz\n{} *porch-b.tar.gz\n\n",
            sha256_hex(b"other").to_uppercase()
        );
        let sums = parse_sums(&text).unwrap();
        assert_eq!(sums.len(), 2);
        assert_eq!(sums["porch-b.tar.gz"], sha256_hex(b"other"));
        verify_checksum(b"archive", "porch-a.tar.gz", &sums).unwrap();
        verify_checksum(b"other", "porch-b.tar.gz", &sums).unwrap();
        let err = verify_checksum(b"tampered", "porch-a.tar.gz", &sums).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
        assert!(verify_checksum(b"archive", "missing.tar.gz", &sums).is_err());
        assert!(parse_sums("abc  file").is_err());
        assert!(parse_sums(&good).is_err());
        let conflicting = format!("{good}  x\n{}  x\n", sha256_hex(b"y"));
        assert!(parse_sums(&conflicting).is_err());
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn npm_installs_are_detected() {
        assert!(is_npm_path(Path::new(
            "/w/app/node_modules/porch-cli-darwin-arm64/porch"
        )));
        assert!(is_npm_path(Path::new(
            "/w/app/node_modules/.pnpm/porch-cli-linux-x64@0.5.0/node_modules/porch-cli-linux-x64/porch"
        )));
        assert!(!is_npm_path(Path::new("/home/me/.local/bin/porch")));
        assert!(!is_npm_path(Path::new("/w/node_modules/other/porch")));
    }
    #[test]
    fn homebrew_and_source_paths_are_detected() {
        let prefixes = vec![PathBuf::from("/opt/homebrew"), PathBuf::from("/usr/local")];
        for brew in [
            "/opt/homebrew/Cellar/porch/0.3.0/bin/porch",
            "/usr/local/Cellar/porch/0.3.0/bin/porch",
            "/home/linuxbrew/.linuxbrew/Cellar/porch/0.3.0/bin/porch",
            "/custom/brew/Cellar/porch/0.3.0/bin/porch",
        ] {
            assert!(is_homebrew_path(Path::new(brew), &prefixes), "{brew}");
        }
        for other in [
            "/Users/me/.local/bin/porch",
            "/usr/local/bin/porch",
            "/opt/homebrew-backup/porch",
            "/home/me/Cellar-notes/porch",
        ] {
            assert!(!is_homebrew_path(Path::new(other), &prefixes), "{other}");
        }
        assert!(is_source_build(Path::new(
            "/src/porch/target/release/porch"
        )));
        assert!(is_source_build(Path::new(
            "/src/porch/target/aarch64-apple-darwin/release/porch"
        )));
        assert!(!is_source_build(Path::new("/Users/me/.local/bin/porch")));
    }

    #[cfg(unix)]
    fn fake_binary(path: &Path, version: &str) {
        crate::test_support::write_executable(path, &format!("#!/bin/sh\necho porch {version}\n"));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replacement_keeps_running_inode_and_rolls_back() {
        // `replace_executable` writes the new binary and then runs it, as a
        // real self-update does in a process that starts nothing else
        // meanwhile. Here other tests start processes in parallel; one that
        // forks while the binary is open for writing keeps it open until it
        // execs, and running it then fails with "Text file busy". So the
        // body runs alone, in a fresh copy of this test binary.
        const ISOLATED: &str = "PORCH_TEST_ISOLATED_REPLACEMENT";
        if std::env::var_os(ISOLATED).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "update::tests::atomic_replacement_keeps_running_inode_and_rolls_back",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(ISOLATED, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let dir = temp("replace-test");
        let dest = dir.0.join("porch");
        let source = dir.0.join("new-porch");
        fake_binary(&dest, "0.1.0");
        fake_binary(&source, "9.9.9");
        let mut running = fs::File::open(&dest).unwrap();
        let old_inode = running.metadata().unwrap().ino();
        replace_executable(
            &source,
            &dest,
            version_check(Version::parse("9.9.9").unwrap()),
        )
        .unwrap();
        let mut old = String::new();
        running.read_to_string(&mut old).unwrap();
        assert!(old.contains("0.1.0"), "open handle keeps the old inode");
        assert!(fs::read_to_string(&dest).unwrap().contains("9.9.9"));
        assert_ne!(fs::metadata(&dest).unwrap().ino(), old_inode);
        assert_eq!(fs::metadata(&dest).unwrap().mode() & 0o777, 0o755);
        let mut names: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["new-porch", "porch"], "no backup or temp left");

        // A binary that fails verification is rolled back from porch.old.
        let broken = dir.0.join("broken");
        fake_binary(&broken, "0.0.0-broken");
        let err = replace_executable(
            &broken,
            &dest,
            version_check(Version::parse("10.0.0").unwrap()),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("verification"));
        assert!(fs::read_to_string(&dest).unwrap().contains("9.9.9"));
        assert!(!backup_path(&dest).exists());
        assert!(!dir
            .0
            .join(".porch.update-".to_owned() + &std::process::id().to_string())
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn app_replacement_refuses_unrelated_bundles() {
        let dir = temp("app-test");
        let make = |root: &Path, id: &str, marker: &str| {
            fs::create_dir_all(root.join("Contents/MacOS")).unwrap();
            fs::write(
                root.join("Contents/Info.plist"),
                format!("<plist><dict><key>CFBundleIdentifier</key><string>{id}</string></dict></plist>"),
            )
            .unwrap();
            fs::write(root.join("Contents/MacOS/porch-tray"), marker).unwrap();
        };
        let new_app = dir.0.join("new/Porch.app");
        let installed = dir.0.join("Applications/Porch.app");
        make(&new_app, APP_BUNDLE_ID, "new");
        make(&installed, APP_BUNDLE_ID, "old");
        replace_app(&new_app, &installed).unwrap();
        assert_eq!(
            fs::read_to_string(installed.join("Contents/MacOS/porch-tray")).unwrap(),
            "new"
        );
        assert_eq!(fs::read_dir(dir.0.join("Applications")).unwrap().count(), 1);
        let foreign = dir.0.join("Other/Porch.app");
        make(&foreign, "other.app", "foreign");
        assert!(replace_app(&new_app, &foreign).is_err());
        assert_eq!(
            fs::read_to_string(foreign.join("Contents/MacOS/porch-tray")).unwrap(),
            "foreign"
        );
    }

    #[test]
    fn notice_is_throttled_daily() {
        let now = 10 * DAY;
        let fresh = NoticeCache::default();
        assert!(fresh.check_due(now));
        let checked = NoticeCache {
            checked_at: now - DAY + 1,
            attempted_at: now - DAY + 1,
            latest: Some("99.0.0".into()),
            notified_at: 0,
        };
        assert!(!checked.check_due(now));
        assert!(checked.check_due(now + 1));
        // A recent failed attempt delays the retry by an hour.
        let failed = NoticeCache {
            attempted_at: now - 60,
            ..Default::default()
        };
        assert!(!failed.check_due(now));
        assert!(failed.check_due(now + RETRY_AFTER_FAILURE));

        assert_eq!(
            checked.notice_due("0.2.0", now).as_deref(),
            Some("porch 99.0.0 is available — run porch self-update")
        );
        let notified = NoticeCache {
            notified_at: now - 10,
            ..checked.clone()
        };
        assert_eq!(notified.notice_due("0.2.0", now), None);
        assert!(notified.notice_due("0.2.0", now - 10 + DAY).is_some());
        assert_eq!(checked.notice_due("99.0.0", now), None);
        assert_eq!(checked.notice_due("100.0.0", now), None);
        assert_eq!(NoticeCache::default().notice_due("0.2.0", now), None);
    }

    #[test]
    fn notice_is_suppressed_for_machine_output_and_non_tty() {
        // These assertions depend on the env; skip when the suite runs with opt-outs.
        if truthy("PORCH_NO_UPDATE_CHECK") || truthy("CI") {
            assert!(!notice_allowed(&strings(&["list"]), true));
            return;
        }
        assert!(notice_allowed(&strings(&["list"]), true));
        assert!(!notice_allowed(&strings(&["list"]), false));
        assert!(!notice_allowed(&strings(&["list", "--json"]), true));
        assert!(!notice_allowed(
            &strings(&["--state-dir", "/x", "list", "--json"]),
            true
        ));
        assert!(!notice_allowed(&strings(&["self-update"]), true));
        assert!(!notice_allowed(&strings(&["mcp"]), true));
        assert!(!notice_allowed(
            &strings(&["--state-dir", "/x", "tray"]),
            true
        ));
        // `--json` after `--` belongs to the child command.
        assert!(notice_allowed(
            &strings(&["run", "--", "tool", "--json"]),
            true
        ));
        assert_eq!(
            state_dir_from(&strings(&["--state-dir", "/tmp/s", "list"])),
            PathBuf::from("/tmp/s")
        );
        assert_eq!(
            state_dir_from(&strings(&["--state-dir=/tmp/t", "list"])),
            PathBuf::from("/tmp/t")
        );
    }

    #[test]
    fn notice_cache_round_trips() {
        let dir = temp("cache-test");
        let path = cache_path(&dir.0);
        assert_eq!(read_cache(&path), NoticeCache::default());
        let cache = NoticeCache {
            checked_at: 1,
            attempted_at: 2,
            latest: Some("0.3.1".into()),
            notified_at: 3,
        };
        write_cache(&path, &cache);
        assert_eq!(read_cache(&path), cache);
        fs::write(&path, b"not json").unwrap();
        assert_eq!(read_cache(&path), NoticeCache::default());
    }

    /// Serve a fake GitHub API + asset host on loopback. The release lists
    /// every file, with asset URLs pointing back at this server.
    #[cfg(unix)]
    async fn fixture(
        tag: &str,
        files: BTreeMap<String, Vec<u8>>,
        expect_token: Option<&'static str>,
    ) -> String {
        use axum::{
            extract::{Path as UrlPath, State},
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };
        type Shared = std::sync::Arc<(serde_json::Value, BTreeMap<String, Vec<u8>>)>;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let release = serde_json::json!({
            "tag_name": tag,
            "assets": files.keys().map(|n| serde_json::json!({
                "name": n,
                "url": format!("{base}/assets/{n}"),
                "browser_download_url": format!("{base}/download/{n}"),
            })).collect::<Vec<_>>(),
        });
        let authorized = move |headers: &HeaderMap| match expect_token {
            None => true,
            Some(token) => headers
                .get("authorization")
                .is_some_and(|v| v == format!("Bearer {token}").as_str()),
        };
        let app = Router::new()
            .route(
                "/repos/{owner}/{repo}/releases/latest",
                get(
                    move |State(s): State<Shared>, headers: HeaderMap| async move {
                        if !authorized(&headers) {
                            return Err(StatusCode::NOT_FOUND);
                        }
                        Ok(axum::Json(s.0.clone()))
                    },
                ),
            )
            .route(
                "/assets/{name}",
                get(
                    move |State(s): State<Shared>,
                          UrlPath(name): UrlPath<String>,
                          headers: HeaderMap| async move {
                        if headers
                            .get("accept")
                            .is_none_or(|v| v != "application/octet-stream")
                            || !authorized(&headers)
                        {
                            return Err(StatusCode::NOT_FOUND);
                        }
                        s.1.get(&name).cloned().ok_or(StatusCode::NOT_FOUND)
                    },
                ),
            )
            .with_state(std::sync::Arc::new((release, files)));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    #[cfg(unix)]
    fn tarball(dir: &Path, version: &str, target: &str) -> (String, Vec<u8>) {
        let name = format!("porch-{version}-{target}");
        let package = dir.join("pkg").join(&name);
        fs::create_dir_all(package.join("Porch.app/Contents/MacOS")).unwrap();
        fake_binary(&package.join("porch"), version);
        fake_binary(&package.join("porch-tray"), version);
        // Release archives store the app's executables as hard links to the
        // copies at the package root (scripts/package.py).
        for name in ["porch", "porch-tray"] {
            fs::hard_link(
                package.join(name),
                package.join("Porch.app/Contents/MacOS").join(name),
            )
            .unwrap();
        }
        fs::write(
            package.join("Porch.app/Contents/Info.plist"),
            format!("<string>{APP_BUNDLE_ID}</string><string>{version}</string>"),
        )
        .unwrap();
        let archive = dir.join(format!("{name}.tar.gz"));
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.join("pkg"))
            .arg(&name)
            .status()
            .unwrap();
        assert!(status.success());
        (format!("{name}.tar.gz"), fs::read(archive).unwrap())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn downloads_verifies_and_installs_from_local_fixture() {
        let dir = temp("download-test");
        let target = "x86_64-unknown-linux-gnu";
        let (asset, bytes) = tarball(&dir.0, "99.1.0", target);
        let sums = format!("{}  {asset}\n", sha256_hex(&bytes));
        let files: BTreeMap<_, _> = [
            (asset.clone(), bytes.clone()),
            ("SHA256SUMS".to_owned(), sums.into_bytes()),
        ]
        .into();
        let base = fixture("v99.1.0", files.clone(), Some("secret-token")).await;

        // Everything lives in a temp dir; the real installed binary is never touched.
        let install = dir.0.join("bin");
        fs::create_dir_all(&install).unwrap();
        let exe = install.join("porch");
        fake_binary(&exe, "0.0.1");
        fake_binary(&install.join("porch-tray"), "0.0.1");
        let app = dir.0.join("Applications/Porch.app");
        fs::create_dir_all(app.join("Contents")).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            format!("<string>{APP_BUNDLE_ID}</string>"),
        )
        .unwrap();

        // Without a token the private fixture hides the release.
        let anonymous = GitHub::new(&base, "owner/porch", None, Duration::from_secs(10)).unwrap();
        let err = perform(
            &anonymous,
            target,
            &exe,
            None,
            &SelfUpdateArgs::default(),
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("GITHUB_TOKEN"), "{err:#}");

        let github = GitHub::new(
            &base,
            "owner/porch",
            Some("secret-token".into()),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(
            github
                .latest()
                .await
                .unwrap()
                .version()
                .unwrap()
                .to_string(),
            "99.1.0"
        );

        // --dry-run leaves everything untouched.
        let dry_run = SelfUpdateArgs {
            dry_run: true,
            ..Default::default()
        };
        let planned = perform(&github, target, &exe, Some(&app), &dry_run, &mut Vec::new())
            .await
            .unwrap();
        assert_eq!(planned, Outcome::Planned("99.1.0".into()));
        assert!(fs::read_to_string(&exe).unwrap().contains("0.0.1"));

        let mut out = Vec::new();
        let outcome = perform(
            &github,
            target,
            &exe,
            Some(&app),
            &SelfUpdateArgs::default(),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::Updated("99.1.0".into()));
        assert!(fs::read_to_string(&exe).unwrap().contains("99.1.0"));
        assert!(fs::read_to_string(install.join("porch-tray"))
            .unwrap()
            .contains("99.1.0"));
        assert!(
            !install.join("porch-login").exists(),
            "absent companions stay absent"
        );
        assert!(fs::read_to_string(app.join("Contents/Info.plist"))
            .unwrap()
            .contains("99.1.0"));
        for name in ["porch", "porch-tray"] {
            assert!(
                fs::read_to_string(app.join("Contents/MacOS").join(name))
                    .unwrap()
                    .contains("99.1.0"),
                "{name} inside the app"
            );
        }
        assert!(!backup_path(&exe).exists());

        // A release for another platform is reported, not guessed.
        let err = perform(
            &github,
            "aarch64-apple-darwin",
            &exe,
            None,
            &SelfUpdateArgs::default(),
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("aarch64-apple-darwin"), "{err:#}");

        // A tampered archive is rejected before anything is replaced.
        fake_binary(&exe, "0.0.1");
        let mut tampered = files.clone();
        tampered.insert(asset.clone(), b"tampered".to_vec());
        let tampered_base = fixture("v99.1.0", tampered, None).await;
        let github =
            GitHub::new(&tampered_base, "owner/porch", None, Duration::from_secs(10)).unwrap();
        let err = perform(
            &github,
            target,
            &exe,
            None,
            &SelfUpdateArgs::default(),
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err:#}");
        assert!(fs::read_to_string(&exe).unwrap().contains("0.0.1"));

        // Already current: nothing is downloaded.
        let current_base = fixture(&format!("v{CURRENT_VERSION}"), BTreeMap::new(), None).await;
        let github =
            GitHub::new(&current_base, "owner/porch", None, Duration::from_secs(10)).unwrap();
        let outcome = perform(
            &github,
            target,
            &exe,
            None,
            &SelfUpdateArgs::default(),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::UpToDate(CURRENT_VERSION.into()));
    }
}
