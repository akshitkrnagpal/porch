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
/// The formula `brew upgrade` installs from (the tap is `BREW_FORMULA`).
const TAP_FORMULA_URL: &str =
    "https://raw.githubusercontent.com/akshitkrnagpal/homebrew-tap/HEAD/Formula/porch.rb";

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

/// A semantic version. Build metadata (`+...`) is kept in the text, which
/// names release assets, but ignored when comparing, as semver requires.
#[derive(Debug, Clone)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<String>,
    text: String,
}

fn is_identifier(part: &str) -> bool {
    !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

impl Version {
    pub fn parse(raw: &str) -> Option<Self> {
        let text = raw.trim().trim_start_matches('v');
        let (rest, build) = match text.split_once('+') {
            Some((rest, build)) => (rest, Some(build)),
            None => (text, None),
        };
        if build.is_some_and(|b| !b.split('.').all(is_identifier)) {
            return None;
        }
        let (core, pre) = match rest.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (rest, None),
        };
        let pre: Vec<String> = match pre {
            Some(pre) => {
                let parts: Vec<&str> = pre.split('.').collect();
                if !parts.iter().all(|p| is_identifier(p)) {
                    return None;
                }
                parts.into_iter().map(str::to_owned).collect()
            }
            None => Vec::new(),
        };
        let mut parts = core.split('.').map(|p| {
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse::<u64>().ok())
                .flatten()
        });
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

/// Semver precedence of two dot-separated pre-release identifiers: numeric
/// ones compare as numbers and sort before alphanumeric ones.
fn compare_identifier(a: &str, b: &str) -> Ordering {
    let number = |s: &str| {
        s.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| s.parse::<u128>().ok())
            .flatten()
    };
    match (number(a), number(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

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
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = compare_identifier(a, b);
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
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

/// How strictly `porch self-update` checks the build attestation
/// (`PORCH_UPDATE_VERIFY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyMode {
    /// Check it when the GitHub CLI can; otherwise say so and continue.
    #[default]
    Auto,
    /// Refuse to install unless it verifies.
    Require,
}

impl VerifyMode {
    pub fn parse(value: Option<&str>) -> Result<VerifyMode> {
        match value.map(str::trim).unwrap_or("") {
            "" => Ok(VerifyMode::Auto),
            v if v.eq_ignore_ascii_case("auto") => Ok(VerifyMode::Auto),
            v if v.eq_ignore_ascii_case("require") => Ok(VerifyMode::Require),
            other => bail!(
                "PORCH_UPDATE_VERIFY must be `require` or `auto` (the default), got {other:?}"
            ),
        }
    }

    pub fn from_env() -> Result<VerifyMode> {
        VerifyMode::parse(std::env::var("PORCH_UPDATE_VERIFY").ok().as_deref())
    }
}

/// Settings for the attestation check; `gh` is the GitHub CLI to run.
#[derive(Debug, Clone)]
pub struct Provenance {
    pub mode: VerifyMode,
    pub gh: PathBuf,
}

impl Provenance {
    pub fn from_env() -> Result<Provenance> {
        Ok(Provenance {
            mode: VerifyMode::from_env()?,
            gh: PathBuf::from("gh"),
        })
    }
}

/// What `gh attestation verify` could establish about an archive.
#[derive(Debug, PartialEq, Eq)]
enum Attestation {
    /// Verified; `pinned_ref` says whether the release tag was checked too.
    Verified { pinned_ref: bool },
    /// The check could not run here: no usable gh, no login, offline, a mirror.
    Unavailable { reason: String, detail: String },
    /// GitHub answered, and the archive has no attestation from this
    /// repository's release workflow for this tag.
    Failed { detail: String },
}

fn gh_output(gh: &Path, args: &[&std::ffi::OsStr]) -> std::io::Result<std::process::Output> {
    Command::new(gh)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
}

/// Run `gh attestation verify` for `tarball` and classify the result.
///
/// gh exits 4 when it has no login. Any other failure is a verification
/// failure only if GitHub's API is reachable with gh's credentials (checked
/// with `gh api rate_limit`, which costs no quota); otherwise the check could
/// not run (offline, a revoked token, an Enterprise-only login).
fn check_attestation(gh: &Path, repo: &str, tag: &str, tarball: &Path) -> Attestation {
    let unavailable = |reason: &str, detail: &[u8]| Attestation::Unavailable {
        reason: reason.to_owned(),
        detail: String::from_utf8_lossy(detail).trim().to_owned(),
    };
    let os = std::ffi::OsStr::new;
    if gh_output(gh, &[os("--version")]).is_err() {
        return unavailable("the GitHub CLI (gh) is not installed", b"");
    }
    let help = match gh_output(gh, &[os("attestation"), os("verify"), os("--help")]) {
        Ok(help) if help.status.success() => help,
        _ => {
            return unavailable(
                "this gh has no `attestation` command (it needs gh 2.49 or newer)",
                b"",
            )
        }
    };
    let pinned_ref = String::from_utf8_lossy(&help.stdout).contains("--source-ref");
    let workflow = format!("{repo}/.github/workflows/release.yml");
    let source_ref = format!("refs/tags/{tag}");
    let mut args = vec![
        os("attestation"),
        os("verify"),
        tarball.as_os_str(),
        os("--repo"),
        os(repo),
        os("--signer-workflow"),
        os(&workflow),
    ];
    if pinned_ref {
        args.extend([os("--source-ref"), os(&source_ref)]);
    }
    let result = match gh_output(gh, &args) {
        Ok(result) => result,
        Err(error) => {
            return unavailable(
                "gh attestation verify could not run",
                error.to_string().as_bytes(),
            )
        }
    };
    if result.status.success() {
        return Attestation::Verified { pinned_ref };
    }
    if result.status.code() == Some(4) {
        return unavailable(
            "gh is not logged in (run `gh auth login`, or set GH_TOKEN)",
            &result.stderr,
        );
    }
    let api_ok = gh_output(gh, &[os("api"), os("--silent"), os("rate_limit")])
        .is_ok_and(|probe| probe.status.success());
    if !api_ok {
        return unavailable(
            "gh could not reach the GitHub API, or its token was rejected",
            &result.stderr,
        );
    }
    Attestation::Failed {
        detail: String::from_utf8_lossy(&result.stderr).trim().to_owned(),
    }
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Check the archive's GitHub build attestation and report what was checked.
///
/// `SHA256SUMS` comes from the same release as the archive, so it shows only
/// that the download is intact. The attestation, checked by the GitHub CLI,
/// shows that this repository's release workflow built the archive from the
/// release's tag. When gh can't check it, the update continues with a clear
/// note, unless `PORCH_UPDATE_VERIFY=require`. A failed check always stops.
fn verify_provenance(
    github: &GitHub,
    provenance: &Provenance,
    tag: &str,
    tarball: &Path,
    out: &mut dyn Write,
) -> Result<()> {
    let result = if github.api != DEFAULT_API {
        Attestation::Unavailable {
            reason: format!(
                "PORCH_UPDATE_API points at {}, and attestations are checked against github.com only",
                github.api
            ),
            detail: String::new(),
        }
    } else {
        check_attestation(&provenance.gh, &github.repo, tag, tarball)
    };
    let workflow = format!("{}/.github/workflows/release.yml", github.repo);
    match result {
        Attestation::Verified { pinned_ref: true } => {
            writeln!(
                out,
                "Verified build attestation: built by {workflow} from refs/tags/{tag}"
            )?;
        }
        Attestation::Verified { pinned_ref: false } => {
            if provenance.mode == VerifyMode::Require {
                bail!(
                    "PORCH_UPDATE_VERIFY=require: this gh can't check which tag the release was built from (no --source-ref); update the GitHub CLI"
                );
            }
            writeln!(
                out,
                "Verified build attestation: built by {workflow} (update the GitHub CLI to also check it was built from {tag})"
            )?;
        }
        Attestation::Unavailable { reason, detail } => {
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!("\n{}", indent(&detail))
            };
            if provenance.mode == VerifyMode::Require {
                bail!("PORCH_UPDATE_VERIFY=require: cannot check the build attestation: {reason}{detail}");
            }
            writeln!(
                out,
                "Skipped the build attestation check: {reason}.{detail}\n\
                 Only the SHA-256 checksum was checked. It comes from the same release, so it shows\n\
                 the download is intact, not that Porch's release workflow built it. To check that,\n\
                 install the GitHub CLI (2.49 or newer) and run `gh auth login`, or set GH_TOKEN.\n\
                 Set PORCH_UPDATE_VERIFY=require to refuse updates that can't be checked."
            )?;
        }
        Attestation::Failed { detail } => {
            bail!(
                "the release archive's build attestation did not verify; not installing it.\n{}",
                indent(&detail)
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// GitHub client

pub struct GitHub {
    client: reqwest::Client,
    api: String,
    /// Scheme, host and port of `api`: the only origin a token is sent to.
    origin: url::Origin,
    /// `api` is plain HTTP on a loopback address (a test server).
    loopback_http: bool,
    repo: String,
    token: Option<String>,
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Which credentials a GitHub client may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// No token: the passive daily check, which needs none for a public repository.
    Anonymous,
    /// `GITHUB_TOKEN`/`GH_TOKEN`, then `gh auth token`.
    EnvOrGhCli,
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

impl GitHub {
    /// `api` must be `https://`, or `http://` on a loopback address.
    pub fn new(api: &str, repo: &str, token: Option<String>, timeout: Duration) -> Result<GitHub> {
        if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
            bail!("update repository must look like owner/name, got {repo:?}");
        }
        let api = api.trim_end_matches('/').to_owned();
        let parsed =
            url::Url::parse(&api).with_context(|| format!("update API {api:?} is not a URL"))?;
        let loopback_http = parsed.scheme() == "http" && is_loopback(&parsed);
        if parsed.scheme() != "https" && !loopback_http {
            bail!("update API {api:?} must use https://");
        }
        let client = reqwest::Client::builder()
            .user_agent(format!("porch/{CURRENT_VERSION}"))
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .timeout(timeout)
            .build()?;
        Ok(GitHub {
            client,
            origin: parsed.origin(),
            loopback_http,
            api,
            repo: repo.to_owned(),
            token,
        })
    }

    /// Repository from `PORCH_UPDATE_REPO`, API base from `PORCH_UPDATE_API`.
    /// With [`Auth::EnvOrGhCli`], a token from `GITHUB_TOKEN`/`GH_TOKEN` or
    /// `gh auth token`, but only for the default GitHub API: a token is never
    /// sent to a host named in `PORCH_UPDATE_API`.
    pub fn from_env(timeout: Duration, auth: Auth) -> Result<GitHub> {
        let repo = env_nonempty("PORCH_UPDATE_REPO").unwrap_or_else(|| DEFAULT_REPO.to_owned());
        let api = env_nonempty("PORCH_UPDATE_API").unwrap_or_else(|| DEFAULT_API.to_owned());
        let api = api.trim_end_matches('/').to_owned();
        let mut token = None;
        if auth == Auth::EnvOrGhCli && api == DEFAULT_API {
            token = env_nonempty("GITHUB_TOKEN").or_else(|| env_nonempty("GH_TOKEN"));
            if token.is_none() {
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
        }
        GitHub::new(&api, &repo, token, timeout)
    }

    /// The token, if one is set and `url` is on the API's own origin.
    fn token_for(&self, url: &str) -> Option<&str> {
        let token = self.token.as_deref()?;
        let url = url::Url::parse(url).ok()?;
        (url.origin() == self.origin).then_some(token)
    }

    /// GET `url`, with the token when it may go there. A public repository
    /// needs none, so if GitHub rejects the token (401/403: expired, or one
    /// for another GitHub host) the request is retried without it.
    async fn get(&self, url: &str, accept: &str) -> reqwest::Result<reqwest::Response> {
        let request = || {
            self.client
                .get(url)
                .header(reqwest::header::ACCEPT, accept)
                .header("X-GitHub-Api-Version", "2022-11-28")
        };
        let Some(token) = self.token_for(url) else {
            return request().send().await;
        };
        let response = request().bearer_auth(token).send().await?;
        if matches!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return request().send().await;
        }
        Ok(response)
    }

    pub async fn latest(&self) -> Result<Release> {
        let url = format!("{}/repos/{}/releases/latest", self.api, self.repo);
        let response = self
            .get(&url, "application/vnd.github+json")
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
        let url = url::Url::parse(&asset.url)
            .with_context(|| format!("{} has an invalid URL {:?}", asset.name, asset.url))?;
        if url.scheme() != "https" && !(self.loopback_http && is_loopback(&url)) {
            bail!("refusing to download {} over {}", asset.name, url.scheme());
        }
        // The token goes only to the API origin, and reqwest drops it when the
        // asset redirect leaves that host.
        let response = self
            .get(&asset.url, "application/octet-stream")
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

/// Copies of `dest` moved aside by earlier updates on Windows: `NAME.old`
/// and `NAME.<pid>.old`. Each is deleted once the process running it exits.
#[cfg_attr(not(windows), allow(dead_code))]
fn stale_asides(dest: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (dest.parent(), dest.file_name().and_then(|n| n.to_str())) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let prefix = format!("{name}.");
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let file = entry.file_name();
            let Some(file) = file.to_str() else {
                return false;
            };
            file.strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix("old"))
                .is_some_and(|middle| {
                    middle.is_empty()
                        || middle.strip_suffix('.').is_some_and(|pid| {
                            !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
                        })
                })
        })
        .map(|entry| entry.path())
        .collect()
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
            for stale in stale_asides(dest) {
                let _ = fs::remove_file(stale); // fails while that copy still runs
            }
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

/// The executable names of the companions on this platform. porch-login is
/// a script everywhere; the tray is an executable.
fn companion_names() -> Vec<String> {
    COMPANIONS
        .iter()
        .map(|c| {
            if *c == "porch-tray" {
                exe_name(c)
            } else {
                (*c).to_owned()
            }
        })
        .collect()
}

/// The release target to update to. On x86_64 Linux the default install is
/// the static CLI-only build; if a tray sits beside it (an earlier
/// `PORCH_TRAY=1` install, since overwritten by a default one), use the
/// glibc build that ships the tray, so the tray is updated too.
pub fn update_target<'a>(target: &'a str, exe_dir: &Path) -> &'a str {
    if target == "x86_64-unknown-linux-musl" && exe_dir.join("porch-tray").is_file() {
        "x86_64-unknown-linux-gnu"
    } else {
        target
    }
}

/// Download, verify, and install the latest release over `exe` (and its
/// companions beside it) and `app` when given. Testable with temp paths.
///
/// `porch` itself is replaced last: its version is what the next
/// `self-update` compares, so if anything fails earlier, rerunning the
/// update installs everything again rather than reporting "up to date".
pub async fn perform(
    github: &GitHub,
    provenance: &Provenance,
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
    if latest < current {
        if !is_source_build(exe) {
            bail!(
                "porch {current} is newer than the latest release ({latest}); --force does not downgrade.\n\
                 To install {latest} anyway, rerun the install script with PORCH_VERSION={latest}."
            );
        }
        writeln!(
            out,
            "Replacing source build {current} with the latest release, {latest} (older)."
        )?;
    } else if latest == current {
        writeln!(out, "Reinstalling porch {latest} (--force).")?;
    }
    let asset = select_asset(&release, target)?;
    let sums_asset = release
        .asset(SUMS_ASSET)
        .with_context(|| format!("release {} has no {SUMS_ASSET}", release.tag_name))?;
    if let Some(app) = app {
        // Refuse before anything is replaced, not halfway through.
        if app.exists() && !is_porch_app(app) {
            bail!("refusing to replace unrelated app at {}", app.display());
        }
    }
    if args.dry_run {
        writeln!(out, "Would download {} from {}", asset.name, github.repo)?;
        writeln!(out, "Would verify it against {SUMS_ASSET}")?;
        writeln!(
            out,
            "Would check its build attestation with the GitHub CLI ({})",
            match provenance.mode {
                VerifyMode::Auto => "skipped with a note if gh can't check it",
                VerifyMode::Require => "required",
            }
        )?;
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
    verify_provenance(github, provenance, &release.tag_name, &tarball, out)?;
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
    for companion in companion_names() {
        let (source, dest) = (package.join(&companion), exe_dir.join(&companion));
        if !dest.is_file() {
            continue;
        }
        if source.is_file() {
            replace_executable(&source, &dest, |_| Ok(()))?;
            writeln!(out, "Updated {}", dest.display())?;
        } else {
            writeln!(
                out,
                "Note: {} is left at its old version: the {target} release has no {companion}.\n\
                 Delete it if you no longer use it, or reinstall a build that includes it.",
                dest.display()
            )?;
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
    replace_executable(&new_porch, exe, version_check(latest.clone()))?;
    writeln!(out, "Updated {} to {latest}", exe.display())?;
    Ok(Outcome::Updated(latest.text))
}

fn proxy_running() -> Option<u32> {
    let dir = crate::state::dir();
    if dir.as_os_str().is_empty() {
        return None; // No home directory: never read a path relative to the cwd.
    }
    let identity: crate::state::ProcessIdentity =
        serde_json::from_slice(&fs::read(dir.join("proxy.identity")).ok()?).ok()?;
    crate::state::matches(&identity).then_some(identity.pid)
}

/// The version the Homebrew tap's formula installs. Releases reach the tap
/// a few minutes after GitHub (or later, if the tap update is skipped), and
/// `brew upgrade` can't install a release the tap doesn't have yet.
pub fn parse_formula_version(formula: &str) -> Option<Version> {
    formula.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("version ")?;
        Version::parse(rest.trim().trim_matches('"'))
    })
}

impl GitHub {
    /// Whether this client looks at Porch's own releases, which the tap follows.
    fn is_default(&self) -> bool {
        self.api == DEFAULT_API && self.repo == DEFAULT_REPO
    }

    /// The newest version a Homebrew install can upgrade to: the tap
    /// formula's version (anonymous; it's public), or the latest release when
    /// `PORCH_UPDATE_REPO`/`PORCH_UPDATE_API` point elsewhere.
    pub async fn latest_for_homebrew(&self) -> Result<Version> {
        if !self.is_default() {
            return self.latest().await?.version();
        }
        let response = self
            .client
            .get(TAP_FORMULA_URL)
            .send()
            .await
            .with_context(|| format!("cannot reach {TAP_FORMULA_URL}"))?;
        if !response.status().is_success() {
            bail!("{TAP_FORMULA_URL} returned {}", response.status());
        }
        parse_formula_version(&response.text().await?)
            .with_context(|| format!("{TAP_FORMULA_URL} has no version"))
    }
}

fn is_homebrew_install() -> bool {
    // Matching `Cellar/porch` in the resolved path needs no `brew` process.
    current_exe().is_ok_and(|exe| is_homebrew_path(&exe, &[]))
}

async fn check(github: &GitHub) -> Result<i32> {
    let homebrew = is_homebrew_install();
    let latest = if homebrew {
        github.latest_for_homebrew().await?
    } else {
        github.latest().await?.version()?
    };
    let current = current_version();
    remember(&latest);
    if latest > current {
        let how = if homebrew {
            format!("brew upgrade {BREW_FORMULA}")
        } else {
            "porch self-update".to_owned()
        };
        println!("porch {current} (current)\nporch {latest} (latest)\nUpdate available: run {how}");
        Ok(1)
    } else {
        println!("porch {current} is up to date (latest release {latest}).");
        Ok(0)
    }
}

pub async fn run(args: SelfUpdateArgs) -> Result<i32> {
    if args.check {
        let github = GitHub::from_env(Duration::from_secs(30), Auth::EnvOrGhCli)?;
        return Ok(match check(&github).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("porch: {error:#}");
                2
            }
        });
    }
    let exe = current_exe()?;
    if is_homebrew_path(&exe, &homebrew_prefixes()) {
        println!("porch is managed by Homebrew; running: brew upgrade {BREW_FORMULA}");
        if args.force {
            println!(
                "(--force has no effect on a Homebrew install; to reinstall, run: brew reinstall {BREW_FORMULA})"
            );
        }
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
    // Before any download, so a typo fails fast instead of skipping the check.
    let provenance = Provenance::from_env()?;
    let target = current_target().context("no prebuilt Porch release exists for this platform")?;
    let target = match exe.parent() {
        Some(dir) => update_target(target, dir),
        None => target,
    };
    let github = GitHub::from_env(Duration::from_secs(300), Auth::EnvOrGhCli)?;
    let app = if cfg!(target_os = "macos") {
        installed_app()
    } else {
        None
    };
    let outcome = perform(
        &github,
        &provenance,
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

    /// The newer release to announce now, if one is known and no notice was
    /// shown today.
    pub fn notice_due(&self, current: &str, now: u64) -> Option<String> {
        let latest = Version::parse(self.latest.as_deref()?)?;
        let current = Version::parse(current)?;
        (latest > current && now.saturating_sub(self.notified_at) >= DAY).then_some(latest.text)
    }
}

pub fn notice_message(latest: &str, homebrew: bool) -> String {
    if homebrew {
        format!("porch {latest} is available — run brew upgrade {BREW_FORMULA}")
    } else {
        format!("porch {latest} is available — run porch self-update")
    }
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

/// An existing state directory that is not a symlink. The passive notice
/// only writes into one: creating it is `state`'s job, which also sets its
/// permissions and ownership.
fn usable_state_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
}

/// Best-effort cache write for the passive notice; never creates the state
/// directory.
fn write_cache(path: &Path, cache: &NoticeCache) {
    let Some(dir) = path.parent() else { return };
    if !usable_state_dir(dir) {
        return;
    }
    let temporary = dir.join(format!("{CACHE_FILE}.{}.tmp", std::process::id()));
    let written = serde_json::to_vec(cache).ok().and_then(|bytes| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary).ok()?;
        file.write_all(&bytes).ok()?;
        fs::rename(&temporary, path).ok()
    });
    if written.is_none() {
        let _ = fs::remove_file(&temporary);
    }
}

fn cache_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CACHE_FILE)
}

/// Record a freshly observed latest version (after `--check` or an update),
/// through `state`, which prepares the directory (and under sudo, gives it
/// and the file to the invoking user).
fn remember(latest: &Version) {
    let dir = crate::state::dir();
    if dir.as_os_str().is_empty() {
        return; // No home directory: nowhere to record it.
    }
    let path = cache_path(&dir);
    let mut cache = read_cache(&path);
    cache.latest = Some(latest.text.clone());
    cache.checked_at = now();
    cache.attempted_at = cache.checked_at;
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = crate::state::atomic_write(CACHE_FILE, &bytes);
    }
}

fn truthy(key: &str) -> bool {
    env_nonempty(key).is_some_and(|v| !matches!(v.as_str(), "0" | "false" | "no" | "off"))
}

/// Whether this invocation may show a passive notice at all.
pub fn notice_allowed(args: &[String], stderr_tty: bool) -> bool {
    if !stderr_tty || truthy("PORCH_NO_UPDATE_CHECK") || truthy("CI") {
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
        if !path.parent().is_some_and(usable_state_dir) {
            // No state yet (or a symlink, which `state` refuses): nothing to
            // cache into. The first command that needs state creates it.
            return None;
        }
        let mut cache = read_cache(&path);
        let now = now();
        let mut pending = None;
        if cache.check_due(now) {
            cache.attempted_at = now;
            write_cache(&path, &cache);
            let cache_file = path.clone();
            // Everything slow happens on this thread, including building the
            // HTTP client (which loads the OS trust store). It records its
            // answer itself, so a check that outlasts `finish` still counts
            // when the command runs long enough.
            pending = std::thread::Builder::new()
                .name("porch-update-check".into())
                .spawn(move || {
                    let github = GitHub::from_env(BACKGROUND_TIMEOUT, Auth::Anonymous).ok()?;
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .ok()?;
                    let homebrew = is_homebrew_install();
                    let latest = runtime
                        .block_on(async {
                            tokio::time::timeout(BACKGROUND_TIMEOUT, async {
                                if homebrew {
                                    github.latest_for_homebrew().await
                                } else {
                                    github.latest().await?.version()
                                }
                            })
                            .await
                        })
                        .ok()?
                        .ok()?;
                    // Re-read: a concurrent porch may have written in the meantime.
                    let mut cache = read_cache(&cache_file);
                    cache.latest = Some(latest.text.clone());
                    cache.checked_at = self::now();
                    write_cache(&cache_file, &cache);
                    Some(latest.text)
                })
                .ok();
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
                    self.cache.latest = Some(latest);
                }
            }
        }
        let now = now();
        if let Some(latest) = self.cache.notice_due(CURRENT_VERSION, now) {
            let message = notice_message(&latest, is_homebrew_install());
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
        // Numeric pre-release identifiers compare as numbers (semver 11.4).
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0-alpha.beta"));
        assert!(v("1.0.0-beta.11") < v("1.0.0-rc.1"));
        assert!(v("1.0.0-1") < v("1.0.0-alpha"));
        // Build metadata parses, is kept in the text, and doesn't affect order.
        assert_eq!(v("1.0.0+build-1").to_string(), "1.0.0+build-1");
        assert_eq!(v("1.0.0+build-1"), v("1.0.0"));
        assert!(v("1.0.0-rc.1+b.2") < v("1.0.0"));
        for bad in [
            "1.0.0-",
            "1.0.0+",
            "1.0.0-rc..1",
            "1.0.0-r!c",
            "1.+0.0",
            "01.x.0",
            "1.0.0 rc",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn verify_mode_rejects_unknown_values() {
        assert_eq!(VerifyMode::parse(None).unwrap(), VerifyMode::Auto);
        assert_eq!(VerifyMode::parse(Some(" ")).unwrap(), VerifyMode::Auto);
        assert_eq!(VerifyMode::parse(Some("auto")).unwrap(), VerifyMode::Auto);
        assert_eq!(
            VerifyMode::parse(Some("require")).unwrap(),
            VerifyMode::Require
        );
        assert_eq!(
            VerifyMode::parse(Some("REQUIRE")).unwrap(),
            VerifyMode::Require
        );
        for typo in ["required", "1", "true", "yes", "off"] {
            let err = VerifyMode::parse(Some(typo)).unwrap_err().to_string();
            assert!(
                err.contains("PORCH_UPDATE_VERIFY") && err.contains(typo),
                "{err}"
            );
        }
    }

    /// A provenance setting whose gh does not exist.
    #[cfg(unix)]
    fn auto() -> Provenance {
        Provenance {
            mode: VerifyMode::Auto,
            gh: PathBuf::from("/nonexistent/porch-test/gh"),
        }
    }

    #[test]
    fn tokens_go_only_to_the_api_origin_over_https() {
        let github =
            |api: &str| GitHub::new(api, "owner/porch", Some("t".into()), Duration::from_secs(1));
        assert!(github("http://api.example.com").is_err());
        assert!(github("ftp://api.example.com").is_err());
        assert!(github("not a url").is_err());
        let api = github("https://api.github.com/").unwrap();
        assert_eq!(
            api.token_for("https://api.github.com/repos/o/r/releases/assets/1"),
            Some("t")
        );
        assert_eq!(api.token_for("https://evil.example/asset"), None);
        assert_eq!(api.token_for("http://api.github.com/asset"), None);
        assert_eq!(api.token_for("https://api.github.com:8443/asset"), None);
        let local = github("http://127.0.0.1:9").unwrap();
        assert_eq!(local.token_for("http://127.0.0.1:9/assets/x"), Some("t"));
        assert_eq!(local.token_for("http://127.0.0.1:10/assets/x"), None);
    }

    #[test]
    fn homebrew_formula_version_is_read_from_the_tap() {
        let formula = "class Porch < Formula\n  desc \"x\"\n  version \"0.5.2\"\nend\n";
        assert_eq!(parse_formula_version(formula).unwrap().to_string(), "0.5.2");
        assert!(parse_formula_version("class Porch\nend\n").is_none());
        assert_eq!(
            notice_message("1.0.0", true),
            format!("porch 1.0.0 is available — run brew upgrade {BREW_FORMULA}")
        );
        assert_eq!(
            notice_message("1.0.0", false),
            "porch 1.0.0 is available — run porch self-update"
        );
    }

    #[test]
    fn tray_installs_update_to_the_build_with_the_tray() {
        let dir = temp("target-test");
        assert_eq!(
            update_target("x86_64-unknown-linux-musl", &dir.0),
            "x86_64-unknown-linux-musl"
        );
        fs::write(dir.0.join("porch-tray"), b"tray").unwrap();
        assert_eq!(
            update_target("x86_64-unknown-linux-musl", &dir.0),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            update_target("aarch64-unknown-linux-musl", &dir.0),
            "aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn stale_windows_asides_are_found() {
        let dir = temp("aside-test");
        for name in [
            "porch.exe",
            "porch.exe.old",
            "porch.exe.123.old",
            "porch.exe.x1.old",
            "porch.exe.old.txt",
            "porch-tray.exe.old",
            "porch.exe..old",
        ] {
            fs::write(dir.0.join(name), b"").unwrap();
        }
        let mut found: Vec<_> = stale_asides(&dir.0.join("porch.exe"))
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, ["porch.exe.123.old", "porch.exe.old"]);
    }

    #[test]
    fn notice_cache_is_never_written_into_a_missing_state_dir() {
        let dir = temp("cache-missing-test");
        let state = dir.0.join("state");
        write_cache(&cache_path(&state), &NoticeCache::default());
        assert!(
            !state.exists(),
            "the notice must not create the state directory"
        );
    }

    /// A fake `gh`: `--version` works; `attestation verify --help` prints
    /// `help`; `attestation verify FILE ...` exits `verify` after printing
    /// `porch-test-stderr` and its arguments; `api` exits `api`.
    #[cfg(unix)]
    fn fake_gh(dir: &Path, help: &str, verify: i32, api: i32) -> PathBuf {
        let path = dir.join(format!("gh-{verify}-{api}-{}", help.len()));
        crate::test_support::write_executable(
            &path,
            &format!(
                "#!/bin/sh\n\
                 case \"$1 $2 $3\" in\n\
                 '--version  ') exit 0 ;;\n\
                 'attestation verify --help') printf '%s\\n' '{help}'; exit 0 ;;\n\
                 'attestation verify '*) echo porch-test-stderr \"$@\" >&2; exit {verify} ;;\n\
                 'api '*) exit {api} ;;\n\
                 esac\n\
                 exit 1\n"
            ),
        );
        path
    }

    #[cfg(unix)]
    #[test]
    fn attestation_results_are_classified() {
        let dir = temp("gh-test");
        let tarball = dir.0.join("porch.tar.gz");
        fs::write(&tarball, b"archive").unwrap();
        let check = |gh: &Path| check_attestation(gh, "owner/porch", "v1.2.3", &tarball);
        let flags = "--signer-workflow --source-ref";

        assert_eq!(
            check(&fake_gh(&dir.0, flags, 0, 0)),
            Attestation::Verified { pinned_ref: true }
        );
        // An older gh without --source-ref still checks repository and workflow.
        assert_eq!(
            check(&fake_gh(&dir.0, "--signer-workflow", 0, 0)),
            Attestation::Verified { pinned_ref: false }
        );
        // Not logged in (gh exits 4): can't check, not a failure.
        assert!(matches!(
            check(&fake_gh(&dir.0, flags, 4, 0)),
            Attestation::Unavailable { reason, detail }
                if reason.contains("not logged in") && detail.contains("porch-test-stderr")
        ));
        // Verification failed while the API answers: a real failure, with gh's stderr.
        match check(&fake_gh(&dir.0, flags, 1, 0)) {
            Attestation::Failed { detail } => {
                assert!(detail.contains("porch-test-stderr"), "{detail}");
                assert!(detail.contains("--source-ref refs/tags/v1.2.3"), "{detail}");
                assert!(
                    detail.contains("--signer-workflow owner/porch/.github/workflows/release.yml"),
                    "{detail}"
                );
            }
            other => panic!("{other:?}"),
        }
        // Verification failed and the API is unreachable (offline, bad token).
        assert!(matches!(
            check(&fake_gh(&dir.0, flags, 1, 1)),
            Attestation::Unavailable { reason, .. } if reason.contains("could not reach")
        ));
        // gh without the attestation command, and no gh at all.
        let old = dir.0.join("gh-old");
        crate::test_support::write_executable(
            &old,
            "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nexit 1\n",
        );
        assert!(matches!(
            check(&old),
            Attestation::Unavailable { reason, .. } if reason.contains("2.49")
        ));
        assert!(matches!(
            check(Path::new("/nonexistent/porch-test/gh")),
            Attestation::Unavailable { reason, .. } if reason.contains("not installed")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn verification_modes_report_and_enforce() {
        let dir = temp("provenance-test");
        let tarball = dir.0.join("porch.tar.gz");
        fs::write(&tarball, b"archive").unwrap();
        let github = GitHub::new(DEFAULT_API, "owner/porch", None, Duration::from_secs(1)).unwrap();
        let run = |gh: PathBuf, mode| {
            let mut out = Vec::new();
            let result = verify_provenance(
                &github,
                &Provenance { mode, gh },
                "v1.2.3",
                &tarball,
                &mut out,
            );
            (result, String::from_utf8(out).unwrap())
        };
        let flags = "--signer-workflow --source-ref";

        let (result, out) = run(fake_gh(&dir.0, flags, 0, 0), VerifyMode::Require);
        result.unwrap();
        assert!(out.contains("refs/tags/v1.2.3"), "{out}");

        // Can't check: a clear note in auto mode, an error when required.
        let (result, out) = run(fake_gh(&dir.0, flags, 4, 0), VerifyMode::Auto);
        result.unwrap();
        assert!(
            out.contains("Skipped the build attestation check: gh is not logged in"),
            "{out}"
        );
        assert!(
            out.contains("porch-test-stderr"),
            "gh's stderr is shown: {out}"
        );
        assert!(out.contains("PORCH_UPDATE_VERIFY=require"), "{out}");
        let (result, _) = run(fake_gh(&dir.0, flags, 4, 0), VerifyMode::Require);
        assert!(format!("{:#}", result.unwrap_err()).contains("not logged in"));

        // A real failure stops the update in every mode.
        let (result, _) = run(fake_gh(&dir.0, flags, 1, 0), VerifyMode::Auto);
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("did not verify") && err.contains("porch-test-stderr"),
            "{err}"
        );

        // Required, but gh can't pin the tag.
        let (result, _) = run(
            fake_gh(&dir.0, "--signer-workflow", 0, 0),
            VerifyMode::Require,
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("--source-ref"));

        // A mirror API: attestations can't be checked there.
        let mirror = GitHub::new(
            "https://mirror.example",
            "owner/porch",
            None,
            Duration::from_secs(1),
        )
        .unwrap();
        let mut out = Vec::new();
        let gh = fake_gh(&dir.0, flags, 0, 0);
        let required = Provenance {
            mode: VerifyMode::Require,
            gh: gh.clone(),
        };
        let err = verify_provenance(&mirror, &required, "v1.2.3", &tarball, &mut out).unwrap_err();
        assert!(format!("{err:#}").contains("PORCH_UPDATE_API"), "{err:#}");
        let auto = Provenance {
            mode: VerifyMode::Auto,
            gh,
        };
        verify_provenance(&mirror, &auto, "v1.2.3", &tarball, &mut out).unwrap();
        assert!(String::from_utf8_lossy(&out)
            .contains("Skipped the build attestation check: PORCH_UPDATE_API"));
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

    #[cfg(windows)]
    #[test]
    #[ignore = "helper process for windows_replacement_moves_a_running_executable_aside"]
    fn windows_running_helper() {
        std::thread::sleep(Duration::from_secs(60));
    }

    /// Windows can't overwrite or delete a running executable, only rename
    /// it. `replace_executable` moves it aside (`porch.exe.old`, or a unique
    /// name while an older aside still runs) and removes asides once free.
    #[cfg(windows)]
    #[test]
    fn windows_replacement_moves_a_running_executable_aside() {
        let dir = temp("windows-replace-test");
        let dest = dir.0.join("porch.exe");
        fs::copy(std::env::current_exe().unwrap(), &dest).unwrap();
        let mut running = Command::new(&dest)
            .args([
                "--exact",
                "update::tests::windows_running_helper",
                "--ignored",
                "--test-threads=1",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let while_running = || {
            let first = dir.0.join("first.exe");
            fs::write(&first, b"first build").unwrap();
            replace_executable(&first, &dest, |_| Ok(())).unwrap();
            assert_eq!(fs::read(&dest).unwrap(), b"first build");
            let aside = backup_path(&dest);
            assert!(aside.exists(), "the running copy stays as porch.exe.old");

            // porch.exe.old is still running: this update uses a unique aside.
            let second = dir.0.join("second.exe");
            fs::write(&second, b"second build").unwrap();
            replace_executable(&second, &dest, |_| Ok(())).unwrap();
            assert_eq!(fs::read(&dest).unwrap(), b"second build");
            assert!(aside.exists());
            assert_eq!(stale_asides(&dest), vec![aside.clone()]);

            // A failed verification restores the previous file.
            let broken = dir.0.join("broken.exe");
            fs::write(&broken, b"broken build").unwrap();
            let err = replace_executable(&broken, &dest, |_| bail!("does not start")).unwrap_err();
            assert!(format!("{err:#}").contains("verification"), "{err:#}");
            assert_eq!(fs::read(&dest).unwrap(), b"second build");
            aside
        };
        let result = while_running();
        let _ = running.kill();
        let _ = running.wait();
        std::thread::sleep(Duration::from_millis(200));
        let aside = result;
        // Once nothing runs it, the next update removes the old aside.
        let third = dir.0.join("third.exe");
        fs::write(&third, b"third build").unwrap();
        replace_executable(&third, &dest, |_| Ok(())).unwrap();
        assert!(!aside.exists());
        assert!(stale_asides(&dest).is_empty());
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

        assert_eq!(checked.notice_due("0.2.0", now).as_deref(), Some("99.0.0"));
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
    const REJECT_TOKENS: &str = "!reject-tokens";

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
        // `REJECT_TOKENS` acts like GitHub given an expired token: 401 for
        // any Authorization header, while anonymous requests work.
        let authorized = move |headers: &HeaderMap| match expect_token {
            None => Ok(()),
            Some(REJECT_TOKENS) if headers.contains_key("authorization") => {
                Err(StatusCode::UNAUTHORIZED)
            }
            Some(REJECT_TOKENS) => Ok(()),
            Some(token) => headers
                .get("authorization")
                .is_some_and(|v| v == format!("Bearer {token}").as_str())
                .then_some(())
                .ok_or(StatusCode::NOT_FOUND),
        };
        let app = Router::new()
            .route(
                "/repos/{owner}/{repo}/releases/latest",
                get(
                    move |State(s): State<Shared>, headers: HeaderMap| async move {
                        authorized(&headers)?;
                        Ok::<_, StatusCode>(axum::Json(s.0.clone()))
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
                        {
                            return Err(StatusCode::NOT_FOUND);
                        }
                        authorized(&headers)?;
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
            &auto(),
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
        let planned = perform(
            &github,
            &auto(),
            target,
            &exe,
            Some(&app),
            &dry_run,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(planned, Outcome::Planned("99.1.0".into()));
        assert!(fs::read_to_string(&exe).unwrap().contains("0.0.1"));

        let mut out = Vec::new();
        let outcome = perform(
            &github,
            &auto(),
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
            &auto(),
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
            &auto(),
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
            &auto(),
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

    /// A release fixture whose archive holds `porch` printing `prints`.
    #[cfg(unix)]
    async fn release_fixture(
        dir: &Path,
        version: &str,
        prints: &str,
        target: &str,
        token: Option<&'static str>,
    ) -> String {
        let (asset, bytes) = tarball(dir, version, target);
        // Swap in a porch that prints `prints` and repack.
        let package = dir.join("pkg").join(format!("porch-{version}-{target}"));
        if prints != version {
            fake_binary(&package.join("porch"), prints);
            let status = Command::new("tar")
                .arg("-czf")
                .arg(dir.join(&asset))
                .arg("-C")
                .arg(dir.join("pkg"))
                .arg(format!("porch-{version}-{target}"))
                .status()
                .unwrap();
            assert!(status.success());
        }
        let bytes = if prints != version {
            fs::read(dir.join(&asset)).unwrap()
        } else {
            bytes
        };
        let sums = format!("{}  {asset}\n", sha256_hex(&bytes));
        let files: BTreeMap<_, _> =
            [(asset, bytes), ("SHA256SUMS".to_owned(), sums.into_bytes())].into();
        fixture(&format!("v{version}"), files, token).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_token_falls_back_to_anonymous() {
        let dir = temp("reject-token-test");
        let target = "x86_64-unknown-linux-gnu";
        let base = release_fixture(&dir.0, "99.3.0", "99.3.0", target, Some(REJECT_TOKENS)).await;
        let install = dir.0.join("bin");
        fs::create_dir_all(&install).unwrap();
        let exe = install.join("porch");
        fake_binary(&exe, "0.0.1");
        let github = GitHub::new(
            &base,
            "owner/porch",
            Some("expired".into()),
            Duration::from_secs(10),
        )
        .unwrap();
        let outcome = perform(
            &github,
            &auto(),
            target,
            &exe,
            None,
            &SelfUpdateArgs::default(),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::Updated("99.3.0".into()));
        assert!(fs::read_to_string(&exe).unwrap().contains("99.3.0"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn porch_is_replaced_last_so_a_failed_update_is_retried() {
        let dir = temp("order-test");
        let target = "x86_64-unknown-linux-gnu";
        // The new porch reports the wrong version, so it fails verification.
        let base = release_fixture(&dir.0, "99.2.0", "0.0.0-broken", target, None).await;
        let install = dir.0.join("bin");
        fs::create_dir_all(&install).unwrap();
        let exe = install.join("porch");
        fake_binary(&exe, "0.0.1");
        fake_binary(&install.join("porch-tray"), "0.0.1");
        fs::write(install.join("porch-login"), "#!/bin/sh\n# old login\n").unwrap();
        let github = GitHub::new(&base, "owner/porch", None, Duration::from_secs(10)).unwrap();
        let mut out = Vec::new();
        let err = perform(
            &github,
            &auto(),
            target,
            &exe,
            None,
            &SelfUpdateArgs::default(),
            &mut out,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("verification"), "{err:#}");
        // porch keeps its old version, so the next self-update tries again.
        assert!(fs::read_to_string(&exe).unwrap().contains("0.0.1"));
        assert!(fs::read_to_string(install.join("porch-tray"))
            .unwrap()
            .contains("99.2.0"));
        // A companion the archive lacks is reported, not silently left behind.
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("porch-login is left at its old version"),
            "{out}"
        );
        assert!(fs::read_to_string(install.join("porch-login"))
            .unwrap()
            .contains("old login"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn force_does_not_downgrade_and_require_is_enforced() {
        let dir = temp("downgrade-test");
        let install = dir.0.join("bin");
        fs::create_dir_all(&install).unwrap();
        let exe = install.join("porch");
        fake_binary(&exe, "0.0.1");
        let target = "x86_64-unknown-linux-gnu";

        let old = fixture("v0.0.1", BTreeMap::new(), None).await;
        let github = GitHub::new(&old, "owner/porch", None, Duration::from_secs(10)).unwrap();
        let force = SelfUpdateArgs {
            force: true,
            ..Default::default()
        };
        let err = perform(
            &github,
            &auto(),
            target,
            &exe,
            None,
            &force,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("does not downgrade"), "{err:#}");

        // PORCH_UPDATE_VERIFY=require with a server that isn't api.github.com
        // stops before anything is replaced.
        let base = release_fixture(&dir.0, "99.4.0", "99.4.0", target, None).await;
        let github = GitHub::new(&base, "owner/porch", None, Duration::from_secs(10)).unwrap();
        let required = Provenance {
            mode: VerifyMode::Require,
            ..auto()
        };
        let err = perform(
            &github,
            &required,
            target,
            &exe,
            None,
            &SelfUpdateArgs::default(),
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("PORCH_UPDATE_VERIFY=require"),
            "{err:#}"
        );
        assert!(fs::read_to_string(&exe).unwrap().contains("0.0.1"));
    }
}
