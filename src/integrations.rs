//! Operating-system and external-program integrations. Tests use explicit fixture paths.
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{IpAddr, UdpSocket},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, BufReader};

fn env(name: &str) -> Option<String> {
    std::env::var(format!("PORCH_{name}"))
        .ok()
        .or_else(|| std::env::var(format!("PORTLESS_{name}")).ok())
}
fn disabled(name: &str) -> bool {
    env(name).is_some_and(|v| matches!(v.as_str(), "0" | "false"))
}
/// Write a private state file atomically (temporary file, then rename), so a
/// crash or full disk never leaves it empty or half written.
fn write_state(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    let name = path
        .file_name()
        .context("state file has no name")?
        .to_string_lossy();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes.as_ref())?;
        file.sync_all()?;
        crate::state::inherit_owner(&temporary)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
/// Helper stdout beyond this is an error, never silently truncated (stderr is diagnostic
/// only and may be capped): callers such as NSS
/// cleanup infer absence from a complete listing.
const COMMAND_OUTPUT_LIMIT: u64 = 32 * 1024 * 1024;
/// How long a helper may run: `PORCH_COMMAND_TIMEOUT_MS`, 30 seconds by default.
fn helper_timeout() -> Duration {
    Duration::from_millis(
        env("COMMAND_TIMEOUT_MS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(30_000u64),
    )
}
/// How long a helper that waits on the user (a password, Touch ID or
/// confirmation dialog) may run: `PORCH_PROMPT_TIMEOUT_MS`, 10 minutes by default.
fn prompt_timeout() -> Duration {
    Duration::from_millis(
        env("PROMPT_TIMEOUT_MS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(600_000u64),
    )
}
fn command(program: &str, args: &[String]) -> Result<String> {
    command_within(program, args, helper_timeout())
}
fn command_within(program: &str, args: &[String], timeout: Duration) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().with_context(|| {
        format!("could not start {program}; install it and ensure it is on PATH")
    })?;
    let stdout = child.stdout.take().context("missing command stdout")?;
    let stderr = child.stderr.take().context("missing command stderr")?;
    let read = |reader: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut data = Vec::new();
            reader
                .take(COMMAND_OUTPUT_LIMIT + 1)
                .read_to_end(&mut data)
                .map(|_| data)
        })
    };
    let stdout = read(Box::new(stdout));
    let stderr = read(Box::new(stderr));
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} timed out after {}ms", timeout.as_millis());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let out = stdout
        .join()
        .map_err(|_| anyhow::anyhow!("command stdout reader failed"))??;
    let err = stderr
        .join()
        .map_err(|_| anyhow::anyhow!("command stderr reader failed"))??;
    // Before the exit status: a helper writing past the limit may be killed by
    // SIGPIPE once the reader stops, and must still be reported as oversized.
    if out.len() as u64 > COMMAND_OUTPUT_LIMIT {
        bail!("{program} produced more than {COMMAND_OUTPUT_LIMIT} bytes of output");
    }
    require_status(status)
        .with_context(|| format!("{program} failed: {}", String::from_utf8_lossy(&err)))?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}
pub(crate) fn exec(program: &str, args: &[&str]) -> Result<String> {
    command(
        program,
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
}
/// Run a helper that may wait on the user, such as a trust dialog: it gets
/// minutes rather than the usual 30 seconds.
fn exec_prompt(program: &str, args: &[&str]) -> Result<String> {
    command_within(
        program,
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        prompt_timeout(),
    )
}
fn valid_hostname(host: &str) -> Result<String> {
    if host.len() > 253
        || host.split('.').any(|s| {
            s.is_empty()
                || s.len() > 63
                || s.starts_with('-')
                || s.ends_with('-')
                || !s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        bail!("invalid hostname: {host}");
    }
    Ok(host.to_ascii_lowercase())
}
fn route_hosts(name: &str, settings: &crate::state::ProxySettings) -> Result<Vec<String>> {
    let name = name.trim_end_matches('.');
    if settings.lan {
        return Ok(vec![valid_hostname(&format!(
            "{}.local",
            name.trim_end_matches(".local")
        ))?]);
    }
    if settings
        .tlds
        .iter()
        .any(|t| name.ends_with(&format!(".{t}")))
    {
        return Ok(vec![valid_hostname(name)?]);
    }
    settings
        .tlds
        .iter()
        .map(|t| valid_hostname(&format!("{name}.{t}")))
        .collect()
}
fn hosts_content(original: &str, names: &[String]) -> Result<String> {
    let mut result = String::new();
    let mut inside = false;
    for line in original.split_inclusive('\n') {
        match line.trim() {
            "# porch-start" => {
                if inside {
                    bail!("nested porch hosts block");
                }
                inside = true;
            }
            "# porch-end" => {
                if !inside {
                    bail!("unmatched porch hosts marker");
                }
                inside = false;
            }
            _ if !inside => result.push_str(line),
            _ => {}
        }
    }
    if inside {
        bail!("unterminated porch hosts block");
    }
    if !names.is_empty() {
        if !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("# porch-start\n");
        for n in names {
            valid_hostname(n)?;
            result.push_str(&format!("127.0.0.1 {n}\n::1 {n}\n"));
        }
        result.push_str("# porch-end\n");
    }
    Ok(result)
}
fn hosts_path() -> PathBuf {
    std::env::var_os("PORCH_HOSTS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(windows) {
                PathBuf::from(
                    std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
                )
                .join("System32/drivers/etc/hosts")
            } else {
                PathBuf::from("/etc/hosts")
            }
        })
}
fn hosts(action: &str) -> Result<()> {
    if !matches!(action, "sync" | "clean") {
        bail!("usage: hosts sync|clean");
    }
    let settings = crate::state::proxy_settings()?;
    let state = crate::state::load()?;
    let names = if action == "sync" {
        state
            .routes
            .iter()
            .filter(|r| crate::state::route_alive(&state, r))
            .map(|r| route_hosts(&r.name, &settings))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    let path = hosts_path();
    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
        bail!("refusing symlink hosts file");
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file=options.open(&path).context("writing hosts file requires administrator privileges; run porch hosts sync once with elevation")?;
    file.lock_exclusive()?;
    rewrite_locked(&mut file, |original| hosts_content(original, &names))
}
/// Rewrite a locked file in place through the locking handle. Windows locks are
/// mandatory and per handle, so a second handle couldn't read it. The new text
/// is written from the start, padded with blank lines over any old tail, and
/// synced before the file is cut to length: an interruption leaves a complete
/// file with trailing blank lines, never an empty or partial one. (A rename
/// isn't used: /etc/hosts is often a bind mount, as in containers.)
fn rewrite_locked(file: &mut fs::File, update: impl FnOnce(&str) -> Result<String>) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut original = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut original)?;
    let updated = update(&original)?;
    if original == updated {
        return Ok(());
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(updated.as_bytes())?;
    let tail = original.len().saturating_sub(updated.len());
    if tail > 0 {
        file.write_all(&vec![b'\n'; tail])?;
    }
    file.flush()?;
    file.sync_all()?;
    file.set_len(updated.len() as u64)?;
    file.sync_all()?;
    Ok(())
}
/// Capability probe only: open for writing without modifying the hosts file.
pub fn hosts_sync_capable() -> bool {
    let path = hosts_path();
    if fs::symlink_metadata(&path).map_or(true, |m| m.file_type().is_symlink()) {
        return false;
    }
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).is_ok()
}
#[derive(Debug)]
pub struct InvalidHostsToken;
impl std::fmt::Display for InvalidHostsToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid hosts sync token")
    }
}
impl std::error::Error for InvalidHostsToken {}
fn localhost_hosts_hint(settings: &crate::state::ProxySettings) -> Result<bool> {
    if settings.lan
        || !settings
            .tlds
            .iter()
            .all(|t| t == "localhost" || t.ends_with(".localhost"))
    {
        return Ok(false);
    }
    let path = crate::state::dir().join("hosts-localhost-hint");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(&path) {
        Ok(_) => {
            crate::state::inherit_owner(&path)?;
            eprintln!("Browsers resolve .localhost without hosts entries; for other clients run `sudo porch hosts sync` if needed.");
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    Ok(true)
}
async fn hosts_response(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let diagnostic = response.text().await?;
    bail!("hosts synchronization returned {status}: {diagnostic}")
}
pub fn auto_hosts_sync() -> Result<()> {
    if disabled("SYNC_HOSTS") || crate::state::proxy_settings()?.unprivileged {
        return Ok(());
    }
    hosts("sync")
}
fn valid_token(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub(crate) fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |v, (a, b)| v | (a ^ b)) == 0
}
pub fn hosts_sync_token() -> Result<String> {
    crate::state::load()?;
    let p = crate::state::dir().join("proxy.hosts-sync-token");
    if let Ok(token) = fs::read_to_string(&p) {
        if valid_token(token.trim()) {
            return Ok(token.trim().to_owned());
        }
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("secure random: {e}"))?;
    let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    match o.open(&p) {
        Ok(mut f) => {
            f.write_all(token.as_bytes())?;
            f.sync_all()?;
            crate::state::inherit_owner(&p)?;
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let t = fs::read_to_string(p)?;
            if !valid_token(t.trim()) {
                bail!("invalid hosts sync token file");
            }
            Ok(t.trim().into())
        }
        Err(e) => Err(e.into()),
    }
}
pub fn hosts_sync_proof(token: &str, challenge: &str) -> Option<String> {
    if !valid_token(token) || !valid_token(challenge) {
        return None;
    }
    let mut key = [0u8; 64];
    key[..64].copy_from_slice(token.as_bytes());
    let mut inner = Sha256::new();
    inner.update(key.map(|b| b ^ 0x36));
    inner.update(challenge.as_bytes());
    let mut outer = Sha256::new();
    outer.update(key.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    Some(format!("{:x}", outer.finalize()))
}
pub fn verify_hosts_sync(token: &str, challenge: &str, proof: &str) -> bool {
    hosts_sync_proof(token, challenge).is_some_and(|p| constant_eq(&p, proof))
}
pub fn authenticated_hosts_sync(token: &str) -> Result<()> {
    if !constant_eq(&hosts_sync_token()?, token) {
        return Err(InvalidHostsToken.into());
    }
    auto_hosts_sync()
}
pub async fn request_hosts_sync() -> Result<()> {
    if disabled("SYNC_HOSTS") || crate::state::proxy_settings()?.unprivileged {
        return Ok(());
    }
    let direct_error = match auto_hosts_sync() {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    let settings = crate::state::proxy_settings()?;
    if localhost_hosts_hint(&settings)? {
        return Ok(());
    }
    // Written by this daemon before readiness. Avoid sending a doomed request
    // to a user-owned proxy with no hosts-file write capability.
    if let Ok(bytes) = fs::read(crate::state::dir().join("proxy.hosts-capability")) {
        if let Ok(capability) = serde_json::from_slice::<Value>(&bytes) {
            let ready =
                fs::read_to_string(crate::state::dir().join("proxy.ready")).unwrap_or_default();
            if capability["pid"].as_u64() == ready.trim().parse::<u64>().ok()
                && capability["writable"] == false
            {
                return Err(direct_error).context(format!("the running proxy cannot write the hosts file; for custom domains run `sudo porch hosts sync`, or use the no-sudo default: `porch proxy start --port {}`", crate::ports::DEFAULT_PROXY_PORT));
            }
        }
    }
    let token = fs::read_to_string(crate::state::dir().join("proxy.hosts-sync-token"))?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("secure random: {e}"))?;
    let challenge = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    // HTTP/1.1 as before: reqwest's rustls backend would otherwise offer h2.
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(3));
    if settings.tls {
        let pem = fs::read(crate::state::dir().join("ca.pem"))?;
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
    }
    let client = builder.build()?;
    let url = format!(
        "{}://localhost:{}/_porch/hosts-sync",
        if settings.tls { "https" } else { "http" },
        settings.port
    );
    let response = hosts_response(
        client
            .head(&url)
            .header("x-porch-hosts-sync-challenge", &challenge)
            .send()
            .await?,
    )
    .await?;
    let proof = response
        .headers()
        .get("x-porch-hosts-sync-proof")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_hosts_sync(token.trim(), &challenge, proof) {
        bail!("proxy failed hosts sync identity challenge");
    }
    hosts_response(
        client
            .post(url)
            .header("x-porch-hosts-sync-token", token.trim())
            .send()
            .await?,
    )
    .await?;
    Ok(())
}

pub(crate) fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub(crate) fn systemd_quote(s: &str) -> Result<String> {
    if s.contains(['\n', '\r', '\0']) {
        bail!("invalid service argument");
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}
pub(crate) fn windows_quote(s: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in s.chars() {
        if c == '\\' {
            slashes += 1;
        } else {
            if c == '\"' {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
            } else {
                out.push_str(&"\\".repeat(slashes));
            }
            slashes = 0;
            out.push(c);
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('\"');
    out
}
fn settings_args(s: &crate::state::ProxySettings) -> Vec<String> {
    let mut a = vec![
        "proxy".into(),
        "start".into(),
        "--foreground".into(),
        // Startup services cannot prompt; the interactive app/trust command
        // performs CA installation before a browser needs it.
        "--skip-trust".into(),
        "--port".into(),
        s.port.to_string(),
        if s.tls {
            "--https".into()
        } else {
            "--no-tls".into()
        },
    ];
    for t in &s.tlds {
        a.extend(["--tld".into(), t.clone()]);
    }
    if s.lan {
        a.push("--lan".into());
    }
    if s.wildcard {
        a.push("--wildcard".into());
    }
    if s.unprivileged {
        a.push("--unprivileged".into());
    }
    for (flag, value) in [
        ("--ip", s.lan_ip.clone()),
        ("--bind", s.bind.clone()),
        (
            "--cert",
            s.cert.as_ref().map(|p| p.to_string_lossy().into_owned()),
        ),
        (
            "--key",
            s.key.as_ref().map(|p| p.to_string_lossy().into_owned()),
        ),
    ] {
        if let Some(v) = value {
            a.extend([flag.into(), v]);
        }
    }
    if let Some(port) = s.redirect_port {
        a.extend(["--redirect-port".into(), port.to_string()]);
    }
    a
}
fn service_settings(args: &[String]) -> Result<crate::state::ProxySettings> {
    let mut s = crate::state::ProxySettings::default();
    let mut explicit_port = false;
    let mut explicit_tls = false;
    if let Some(v) = env("HTTPS") {
        s.tls = !matches!(v.as_str(), "0" | "false");
        explicit_tls = true;
    }
    if let Some(v) = env("PORT") {
        s.port = v.parse()?;
        explicit_port = true;
    }
    if let Some(v) = env("TLD") {
        s.tlds = v.split(',').map(str::to_owned).collect();
    }
    if let Some(v) = env("LAN") {
        s.lan = matches!(v.as_str(), "1" | "true");
    }
    s.lan_ip = env("LAN_IP");
    if let Some(v) = env("WILDCARD") {
        s.wildcard = matches!(v.as_str(), "1" | "true");
    }
    if let Some(v) = env("UNPRIVILEGED") {
        s.unprivileged = matches!(v.as_str(), "1" | "true");
    }
    let mut tlds = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = args[i]
            .split_once('=')
            .map_or((args[i].as_str(), None), |(a, b)| (a, Some(b.to_owned())));
        let mut value = || -> Result<String> {
            if let Some(v) = inline.clone() {
                Ok(v)
            } else {
                i += 1;
                Ok(args
                    .get(i)
                    .context(format!("{flag} needs a value"))?
                    .clone())
            }
        };
        match flag {
            "--port" | "-p" => {
                s.port = value()?.parse()?;
                explicit_port = true;
            }
            "--https" => {
                s.tls = true;
                explicit_tls = true;
            }
            "--no-tls" => {
                s.tls = false;
                explicit_tls = true;
            }
            "--lan" => s.lan = true,
            "--wildcard" => s.wildcard = true,
            "--unprivileged" => s.unprivileged = true,
            "--tld" => tlds.extend(value()?.split(',').map(str::to_owned)),
            "--ip" => {
                s.lan_ip = Some(value()?);
                s.lan = true;
            }
            "--bind" => s.bind = Some(value()?),
            "--cert" => s.cert = Some(value()?.into()),
            "--key" => s.key = Some(value()?.into()),
            "--redirect-port" => s.redirect_port = Some(value()?.parse()?),
            "--state-dir" => {
                std::env::set_var("PORCH_STATE_DIR", value()?);
            }
            _ => bail!("unknown service option {flag}"),
        };
        i += 1;
    }
    if !tlds.is_empty() {
        s.tlds = tlds;
    }
    let explicit_unprivileged = s.unprivileged;
    if s.unprivileged && !explicit_tls {
        s.tls = false;
    }
    if !explicit_port {
        // HTTPS and HTTP share the no-sudo default port; 443 takes an explicit `--port 443`.
        s.port = crate::ports::DEFAULT_PROXY_PORT;
    }
    // High ports never need hosts-file edits or elevation.
    if !explicit_unprivileged && s.port >= crate::ports::FIRST_UNPRIVILEGED_PORT {
        s.unprivileged = true;
    }
    if s.port == 0 {
        bail!("proxy port must be nonzero");
    }
    for t in &s.tlds {
        valid_hostname(t)?;
    }
    if s.cert.is_some() != s.key.is_some() {
        bail!("--cert and --key must be used together");
    }
    Ok(s)
}
/// launchd label, systemd unit and Windows task of the proxy's startup service.
const SERVICE_LABEL: &str = "dev.porch.proxy";
const SERVICE_UNIT: &str = "porch.service";
/// The SYSTEM scheduled task Porch 0.5.2 and earlier installed on Windows.
const LEGACY_TASK: &str = "PorchProxy";
#[derive(Serialize, Deserialize)]
struct ServiceRecord {
    platform: String,
    path: PathBuf,
    state_dir: PathBuf,
    settings: crate::state::ProxySettings,
    /// `user` (a login item that runs as you) or `system` (a Linux system unit
    /// that runs as you with only the right to bind low ports). Records from
    /// Porch 0.5.2 and earlier have none: their service ran as root.
    #[serde(default)]
    scope: Option<String>,
    /// The Windows task name.
    #[serde(default)]
    task: Option<String>,
    /// The root-owned copy of porch a Linux system unit runs.
    #[serde(default)]
    binary: Option<PathBuf>,
}
fn service_record() -> Option<ServiceRecord> {
    serde_json::from_slice(&fs::read(crate::state::dir().join("service.json")).ok()?).ok()
}
fn service_platform() -> String {
    std::env::var("PORCH_SERVICE_PLATFORM").unwrap_or_else(|_| std::env::consts::OS.into())
}
#[cfg(unix)]
fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}
#[cfg(not(unix))]
fn is_root() -> bool {
    false
}
/// The account the service is for: the sudo caller when elevated, else this
/// user. `(uid, gid, home)`.
#[cfg(unix)]
fn service_owner() -> Result<(u32, u32, PathBuf)> {
    let uid = std::env::var("SUDO_UID")
        .ok()
        .filter(|_| is_root())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| unsafe { libc::getuid() });
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return Ok((uid, unsafe { libc::getgid() }, user_home()?));
    }
    let (gid, home) = unsafe {
        (
            (*pw).pw_gid,
            std::ffi::CStr::from_ptr((*pw).pw_dir)
                .to_string_lossy()
                .into_owned(),
        )
    };
    // Tests and isolated setups point HOME elsewhere; honour it for the user itself.
    let home = if is_root() {
        PathBuf::from(home)
    } else {
        user_home().unwrap_or_else(|_| PathBuf::from(home))
    };
    Ok((uid, gid, home))
}
#[cfg(not(unix))]
fn service_owner() -> Result<(u32, u32, PathBuf)> {
    Ok((0, 0, PathBuf::new()))
}
/// The per-user startup file (`PORCH_SERVICE_FILE` overrides it in tests).
fn user_service_path(platform: &str) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("PORCH_SERVICE_FILE") {
        return Ok(p.into());
    }
    if platform == "windows" {
        // Task Scheduler keeps its own copy; this one records what was installed.
        return Ok(crate::state::dir().join("porch-service.xml"));
    }
    let home = service_owner()?.2;
    Ok(match platform {
        "macos" => home
            .join("Library/LaunchAgents")
            .join(format!("{SERVICE_LABEL}.plist")),
        "linux" => std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty() && !is_root())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("systemd/user")
            .join(SERVICE_UNIT),
        _ => bail!("startup service supports macOS, Linux and Windows"),
    })
}
/// The system-level unit: the root LaunchDaemon of Porch 0.5.2 and earlier on
/// macOS, the system unit on Linux (`PORCH_SYSTEM_SERVICE_FILE` overrides it).
fn system_service_path(platform: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PORCH_SYSTEM_SERVICE_FILE") {
        return Some(p.into());
    }
    match platform {
        "macos" => Some(format!("/Library/LaunchDaemons/{SERVICE_LABEL}.plist").into()),
        "linux" => Some(format!("/etc/systemd/system/{SERVICE_UNIT}").into()),
        _ => None,
    }
}
/// Where a Linux system unit's root-owned copy of porch lives.
fn system_binary_path() -> PathBuf {
    match std::env::var_os("PORCH_SYSTEM_SERVICE_FILE") {
        Some(p) => PathBuf::from(p).with_file_name("porch-service-bin"),
        None => "/usr/local/lib/porch/porch".into(),
    }
}
fn system_paths_overridden() -> bool {
    std::env::var_os("PORCH_SYSTEM_SERVICE_FILE").is_some()
}
fn windows_task(user: Option<&str>) -> String {
    let user: String = user
        .map(str::to_owned)
        .or_else(|| std::env::var("USERNAME").ok())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{LEGACY_TASK}-{user}")
}
/// Is a system-level startup unit installed (a root LaunchDaemon from Porch
/// 0.5.2 or earlier, or a Linux system unit)? Removing it needs sudo.
pub fn system_service_installed() -> bool {
    let platform = service_platform();
    system_service_path(&platform).is_some_and(|p| p.exists())
}
/// Whether `porch service ARGS` must run with sudo: only installing or
/// removing the Linux system unit (`--system`) does.
pub fn service_needs_elevation(args: &[String]) -> bool {
    !system_paths_overridden()
        && args
            .first()
            .is_some_and(|a| a == "install" || a == "uninstall")
        && args.iter().any(|a| a == "--system")
}
/// A problem with the installed startup service worth telling the user about,
/// for `porch service status` and `porch doctor`.
pub fn service_warning() -> Option<String> {
    let platform = service_platform();
    let record = service_record();
    let root_unit = || -> bool {
        match (platform.as_str(), system_service_path(&platform)) {
            ("macos", Some(p)) => p.exists(),
            ("linux", Some(p)) => fs::read_to_string(p)
                .is_ok_and(|body| !body.contains("AmbientCapabilities=CAP_NET_BIND_SERVICE")),
            _ => false,
        }
    };
    let legacy_windows = platform == "windows"
        && record.as_ref().is_some_and(|r| r.scope.is_none())
        || fs::read_to_string(crate::state::dir().join("porch-service.xml"))
            .is_ok_and(|body| body.contains("S-1-5-18"));
    if root_unit() || legacy_windows || record.as_ref().is_some_and(|r| r.scope.is_none()) {
        return Some("the startup service runs Porch as root (installed by Porch 0.5.2 or earlier); run `porch service install` to replace it with one that runs as you".into());
    }
    if let Some(binary) = record.and_then(|r| r.binary) {
        let current = std::env::current_exe().ok()?;
        let digest = |p: &Path| fs::read(p).ok().map(Sha256::digest);
        if digest(&binary) != digest(&current) {
            return Some(format!(
                "the startup service runs a copy of an older porch ({}); run `porch service install --system` again to update it",
                binary.display()
            ));
        }
    }
    None
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceScope {
    User,
    System,
}
/// Where the startup service runs. Wherever this user can bind the proxy's
/// ports (always on Windows, port 80/443 on macOS, every port from 1024), it is
/// a login item that runs as you. Only Linux without a lowered
/// `ip_unprivileged_port_start` needs a system unit for ports below 1024, and
/// that unit still runs as you, with just the right to bind them.
fn service_scope(
    platform: &str,
    settings: &crate::state::ProxySettings,
    root: bool,
    system: bool,
    bindable: impl Fn(u16) -> bool,
) -> Result<ServiceScope> {
    let low: Vec<u16> = std::iter::once(settings.port)
        .chain(settings.redirect_port.filter(|_| settings.tls))
        .filter(|p| *p < crate::ports::FIRST_UNPRIVILEGED_PORT)
        .collect();
    if platform == "linux"
        && (system || (!low.is_empty() && (root || !low.iter().all(|p| bindable(*p)))))
    {
        if low.is_empty() {
            bail!("the proxy's ports need no privileges; run `porch service install` without --system")
        }
        return Ok(ServiceScope::System);
    }
    if system {
        bail!("--system is only for Linux ports below 1024");
    }
    if root {
        bail!("run `porch service install` without sudo: the startup service runs as you");
    }
    // macOS lets any user bind low ports only on the wildcard address.
    let wildcard = settings
        .bind
        .as_deref()
        .is_none_or(|b| b == "0.0.0.0" || b == "::");
    if platform == "macos" && !low.is_empty() && !(wildcard && low.iter().all(|p| bindable(*p))) {
        bail!(
            "macOS doesn't let you serve port {} {}without sudo; install with the default port: `porch service install --port {}`",
            low[0],
            if wildcard { "" } else { "on that address " },
            crate::ports::DEFAULT_PROXY_PORT
        );
    }
    Ok(ServiceScope::User)
}
/// May this user bind `port` at all? A port that is merely in use (by the
/// proxy `porch setup` just started, say) still counts.
fn user_may_bind(port: u16) -> bool {
    match crate::sockets::reserve(port) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::PermissionDenied,
    }
}
fn render_service(
    platform: &str,
    scope: ServiceScope,
    exe: &Path,
    state: &Path,
    argv: &[String],
    owner: (u32, u32),
    windows_user: &str,
) -> Result<String> {
    let exe = exe.to_string_lossy();
    let log = state.join("proxy.log");
    Ok(match (platform, scope) {
        // KeepAlive only after a failure: `porch proxy stop` (a clean exit)
        // stays stopped, as does a proxy an upgrade replaced.
        ("macos", ServiceScope::User) => format!(
            concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
                "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
                "<plist version=\"1.0\"><dict>",
                "<key>Label</key><string>{label}</string>",
                "<key>ProgramArguments</key><array><string>{exe}</string>{args}</array>",
                "<key>EnvironmentVariables</key><dict><key>PORCH_STATE_DIR</key><string>{state}</string></dict>",
                "<key>StandardOutPath</key><string>{log}</string>",
                "<key>StandardErrorPath</key><string>{log}</string>",
                "<key>RunAtLoad</key><true/>",
                "<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>",
                "<key>ThrottleInterval</key><integer>10</integer>",
                "</dict></plist>\n"
            ),
            label = SERVICE_LABEL,
            exe = xml(&exe),
            args = argv
                .iter()
                .map(|a| format!("<string>{}</string>", xml(a)))
                .collect::<String>(),
            state = xml(&state.to_string_lossy()),
            log = xml(&log.to_string_lossy()),
        ),
        ("linux", _) => {
            let exec = std::iter::once(systemd_quote(&exe))
                .chain(argv.iter().map(|a| systemd_quote(a)))
                .collect::<Result<Vec<_>>>()?
                .join(" ");
            let env = systemd_quote(&format!("PORCH_STATE_DIR={}", state.display()))?;
            match scope {
                ServiceScope::User => format!(
                    "# {SERVICE_LABEL}: managed by `porch service install`\n[Unit]\nDescription=Porch proxy\nAfter=network.target\n[Service]\nExecStart={exec}\nEnvironment={env}\nRestart=on-failure\nRestartSec=5\n[Install]\nWantedBy=default.target\n"
                ),
                // Never root: the process runs as you and holds only the right
                // to bind ports below 1024, which it can't pass on.
                ServiceScope::System => format!(
                    "# {SERVICE_LABEL}: managed by `porch service install --system`\n[Unit]\nDescription=Porch proxy\nAfter=network.target\n[Service]\nUser={uid}\nGroup={gid}\nExecStart={exec}\nEnvironment={env}\nAmbientCapabilities=CAP_NET_BIND_SERVICE\nCapabilityBoundingSet=CAP_NET_BIND_SERVICE\nNoNewPrivileges=yes\nRestart=on-failure\nRestartSec=5\n[Install]\nWantedBy=multi-user.target\n",
                    uid = owner.0,
                    gid = owner.1,
                ),
            }
        }
        ("windows", _) => {
            // A logon task runs in the user's session, so start the detached
            // proxy and let this console close, as the discovery agent does.
            let line = argv
                .iter()
                .filter(|a| *a != "--foreground")
                .map(|a| windows_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                concat!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
                    "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">",
                    "<RegistrationInfo><Description>{label}: Porch proxy, managed by `porch service install`</Description></RegistrationInfo>",
                    "<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>",
                    "<Principals><Principal id=\"Author\"><UserId>{user}</UserId>",
                    "<LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>",
                    "<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
                    "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
                    "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
                    "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>",
                    "<Actions Context=\"Author\"><Exec><Command>{exe}</Command>",
                    "<Arguments>{args}</Arguments></Exec></Actions></Task>\n"
                ),
                label = SERVICE_LABEL,
                user = xml(windows_user),
                exe = xml(&exe),
                args = xml(&format!(
                    "{line} --state-dir {}",
                    windows_quote(&state.to_string_lossy())
                )),
            )
        }
        _ => bail!("unsupported startup service platform"),
    })
}
/// `DOMAIN\user` of the current account, which a logon task is bound to.
fn windows_account() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    }
}
/// Homebrew upgrades delete the versioned Cellar path, so point a startup file
/// at the stable `<prefix>/bin/porch` link when one exists.
fn stable_exe(exe: &Path) -> PathBuf {
    let parts: Vec<_> = exe.components().collect();
    if let Some(i) = parts
        .windows(2)
        .position(|w| w[0].as_os_str() == "Cellar" && w[1].as_os_str() == "porch")
    {
        let link = parts[..i].iter().collect::<PathBuf>().join("bin/porch");
        if link.exists() {
            return link;
        }
    }
    exe.to_path_buf()
}
/// Write a file atomically with `mode` (temporary file, then rename).
fn write_unit(path: &Path, body: impl AsRef<[u8]>, mode: u32) -> Result<()> {
    let parent = path.parent().context("startup file has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{SERVICE_LABEL}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode).custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut f = options.open(&temporary)?;
        #[cfg(unix)]
        {
            // The umask may have narrowed the mode.
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        f.write_all(body.as_ref())?;
        f.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("writing {}", path.display()))
}
fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("removing {}", path.display()))),
    }
}
fn launchd_loaded(target: &str) -> bool {
    exec("launchctl", &["print", target]).is_ok()
}
/// Unload a launchd job and wait until launchd has let go of it: a bootstrap
/// straight after a bootout fails while the old job is still being torn down.
fn launchd_unload(target: &str) {
    let _ = exec("launchctl", &["bootout", target]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while launchd_loaded(target) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn launchd_load(domain: &str, path: &Path) -> Result<()> {
    let target = format!("{domain}/{SERVICE_LABEL}");
    launchd_unload(&target);
    let path = path.to_string_lossy();
    let mut attempt = 0;
    loop {
        match exec("launchctl", &["bootstrap", domain, &path]) {
            Ok(_) => return Ok(()),
            Err(_) if launchd_loaded(&target) => return Ok(()),
            Err(e) if attempt >= 3 => return Err(e),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}
fn systemctl(user: bool, args: &[&str]) -> Result<String> {
    let mut all = Vec::new();
    if user {
        all.push("--user");
    }
    all.extend_from_slice(args);
    exec("systemctl", &all)
}
fn systemd_active(user: bool) -> bool {
    systemctl(user, &["is-active", "--quiet", SERVICE_UNIT]).is_ok()
}
fn task_exists(task: &str) -> bool {
    exec("schtasks", &["/Query", "/TN", task]).is_ok()
}
/// Remove the per-user startup service. Not being loaded counts as removed;
/// its file is always deleted, and only a service that is still running
/// afterwards is an error.
fn uninstall_user_scope(platform: &str) -> Result<()> {
    let path = user_service_path(platform)?;
    let record = service_record();
    let mut still = None;
    match platform {
        "macos" => {
            let target = format!("gui/{}/{SERVICE_LABEL}", service_owner()?.0);
            // Only ever touch the job Porch's own startup file describes.
            if path.exists() {
                launchd_unload(&target);
                if launchd_loaded(&target) {
                    still = Some(target);
                }
            }
        }
        "linux" => {
            if path.exists() {
                if is_root() {
                    // Elevated cleanup: reach the user's manager if it can.
                    let machine = format!("--machine={}@", service_owner()?.0);
                    let _ = exec(
                        "systemctl",
                        &["--user", &machine, "disable", "--now", SERVICE_UNIT],
                    );
                } else {
                    let _ = systemctl(true, &["disable", "--now", SERVICE_UNIT]);
                    if systemd_active(true) {
                        still = Some(format!("user unit {SERVICE_UNIT}"));
                    }
                }
            }
        }
        "windows" => {
            let task = record
                .as_ref()
                .and_then(|r| r.task.clone())
                .unwrap_or_else(|| windows_task(None));
            if path.exists() || record.as_ref().is_some_and(|r| r.task.is_some()) {
                let _ = exec("schtasks", &["/End", "/TN", &task]);
                let _ = exec("schtasks", &["/Delete", "/TN", &task, "/F"]);
                if task_exists(&task) {
                    still = Some(format!("task {task}"));
                }
            }
        }
        _ => bail!("unsupported startup service platform"),
    }
    // A record-only Windows file of the legacy task is the system scope's.
    let legacy_file = platform == "windows"
        && fs::read_to_string(&path).is_ok_and(|body| body.contains("S-1-5-18"));
    if !legacy_file {
        remove_if_present(&path)?;
    }
    if platform == "linux" && !is_root() {
        let _ = systemctl(true, &["daemon-reload"]);
    }
    if let Some(what) = still {
        bail!("the startup service ({what}) is still loaded after removing it");
    }
    Ok(())
}
/// Remove the system-level startup service: the root LaunchDaemon or SYSTEM
/// task of Porch 0.5.2 and earlier, or a Linux system unit. Needs root (or
/// an administrator on Windows).
fn uninstall_system_scope(platform: &str) -> Result<()> {
    match platform {
        "macos" => {
            let Some(path) = system_service_path(platform) else {
                return Ok(());
            };
            if !path.exists() {
                return Ok(());
            }
            let target = format!("system/{SERVICE_LABEL}");
            launchd_unload(&target);
            remove_if_present(&path).context(
                "removing the old root startup service needs sudo: run `porch service uninstall`",
            )?;
            if launchd_loaded(&target) {
                bail!("the old root startup service is still loaded after removing it");
            }
        }
        "linux" => {
            let Some(path) = system_service_path(platform) else {
                return Ok(());
            };
            if !path.exists() {
                return Ok(());
            }
            let _ = systemctl(false, &["disable", "--now", SERVICE_UNIT]);
            remove_if_present(&path).context(
                "removing the system startup service needs sudo: run `porch service uninstall`",
            )?;
            let _ = systemctl(false, &["daemon-reload"]);
            if let Some(binary) = service_record().and_then(|r| r.binary) {
                if binary == system_binary_path() {
                    remove_if_present(&binary)?;
                }
            }
            if systemd_active(false) {
                bail!("the system startup service is still running after removing it");
            }
        }
        "windows" => {
            if task_exists(LEGACY_TASK) {
                let _ = exec("schtasks", &["/End", "/TN", LEGACY_TASK]);
                let _ = exec("schtasks", &["/Delete", "/TN", LEGACY_TASK, "/F"]);
                if task_exists(LEGACY_TASK) {
                    bail!("removing the old SYSTEM startup task needs an administrator: run `porch service uninstall` from an elevated prompt");
                }
            }
            let legacy = crate::state::dir().join("porch-service.xml");
            if fs::read_to_string(&legacy).is_ok_and(|body| body.contains("S-1-5-18")) {
                remove_if_present(&legacy)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn system_scope_present(platform: &str) -> bool {
    match platform {
        "windows" => {
            service_record().is_some_and(|r| r.scope.is_none())
                || fs::read_to_string(crate::state::dir().join("porch-service.xml"))
                    .is_ok_and(|body| body.contains("S-1-5-18"))
        }
        _ => system_service_path(platform).is_some_and(|p| p.exists()),
    }
}
fn user_scope_present(platform: &str) -> bool {
    user_service_path(platform)
        .is_ok_and(|p| fs::read_to_string(p).is_ok_and(|body| !body.contains("S-1-5-18")))
}
/// Delete service.json once nothing it describes is installed any more.
fn forget_service_record(platform: &str) -> Result<()> {
    if !user_scope_present(platform) && !system_scope_present(platform) {
        remove_if_present(&crate::state::dir().join("service.json"))?;
    }
    Ok(())
}
/// The per-user startup file, if there is one, starts the proxy for another
/// state directory. On macOS and Linux that file is shared by every state
/// directory of this user, so an isolated `PORCH_STATE_DIR` must never remove
/// the real one; like discovery's `installed_here`, only a file that names
/// this state directory counts as ours. Windows keeps the file in the state
/// directory itself, so it is always this install's.
fn user_service_elsewhere(platform: &str) -> Option<PathBuf> {
    if platform == "windows" {
        return None;
    }
    let path = user_service_path(platform).ok()?;
    let body = fs::read_to_string(&path).ok()?;
    let state = crate::state::dir();
    let marker = match platform {
        "macos" => format!(
            "<key>PORCH_STATE_DIR</key><string>{}</string>",
            xml(&state.to_string_lossy())
        ),
        _ => format!(
            "Environment={}",
            systemd_quote(&format!("PORCH_STATE_DIR={}", state.display())).ok()?
        ),
    };
    let ours = body.contains(SERVICE_LABEL) && body.contains(&marker);
    (!ours).then_some(path)
}
/// Remove the per-user startup service, as the user, unless it belongs to
/// another state directory. `porch clean` runs this before it elevates, like
/// per-user trust.
pub fn uninstall_user_service() -> Result<()> {
    let platform = service_platform();
    if !matches!(platform.as_str(), "macos" | "linux" | "windows") {
        return Ok(());
    }
    if let Some(path) = user_service_elsewhere(&platform) {
        eprintln!(
            "Left the startup service in place: {} starts Porch for another state directory, not {}.",
            path.display(),
            crate::state::dir().display()
        );
        return Ok(());
    }
    uninstall_user_scope(&platform)?;
    forget_service_record(&platform)
}
/// `porch service uninstall` without elevation: both scopes, as far as this
/// process's privileges go. Every step runs even when an earlier one fails.
fn service_uninstall(platform: &str) -> Result<()> {
    uninstall_scopes(platform, true)
}
/// Both startup scopes. Only with `any_user_file` is a per-user startup file
/// that serves another state directory removed too (see
/// [`user_service_elsewhere`]); `porch clean` leaves it alone.
fn uninstall_scopes(platform: &str, any_user_file: bool) -> Result<()> {
    let mut errors = Vec::new();
    if any_user_file || user_service_elsewhere(platform).is_none() {
        if let Err(e) = uninstall_user_scope(platform) {
            errors.push(e);
        }
    }
    if system_scope_present(platform) {
        if let Err(e) = uninstall_system_scope(platform) {
            errors.push(e);
        }
    }
    forget_service_record(platform)?;
    match errors.into_iter().next() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
/// Copy this porch to a root-owned path for a system unit, so the user can't
/// replace what it runs.
#[cfg(unix)]
fn install_system_binary(target: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let exe = std::env::current_exe()?;
    let exe = stable_exe(&exe.canonicalize().unwrap_or(exe));
    let parent = target.parent().context("service binary has no parent")?;
    fs::create_dir_all(parent)?;
    if !system_paths_overridden() {
        let meta = fs::symlink_metadata(parent)?;
        if meta.file_type().is_symlink() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            bail!(
                "{} must be a directory only root can write to",
                parent.display()
            );
        }
    }
    write_unit(target, fs::read(&exe)?, 0o755)
        .with_context(|| format!("copying porch to {}", target.display()))?;
    Ok(target.to_path_buf())
}
#[cfg(not(unix))]
fn install_system_binary(_: &Path) -> Result<PathBuf> {
    bail!("system startup units are only for Linux")
}
async fn service_install(args: &[String]) -> Result<()> {
    let system = args.iter().any(|a| a == "--system");
    let args: Vec<String> = args.iter().filter(|a| *a != "--system").cloned().collect();
    let platform = service_platform();
    let mut settings = service_settings(&args)?;
    let scope = service_scope(
        &platform,
        &settings,
        is_root() && !system_paths_overridden(),
        system,
        user_may_bind,
    )?;
    if scope == ServiceScope::System && !system {
        // The user part (removing a login item) runs as the user; only the
        // system unit is installed with sudo.
        uninstall_user_scope(&platform)?;
        let mut elevated = vec!["install".to_owned(), "--system".to_owned()];
        elevated.extend(args);
        return Box::pin(crate::cli::integration("service", elevated)).await;
    }
    if scope == ServiceScope::User {
        // The proxy runs as you, so it can't edit the hosts file: no sudo mode.
        settings.unprivileged = true;
        if system_scope_present(&platform) {
            // Replace an old root service (it also holds the proxy's port).
            if platform == "windows" || system_paths_overridden() || is_root() {
                uninstall_system_scope(&platform)?;
            } else {
                Box::pin(crate::cli::integration(
                    "service",
                    vec!["uninstall".into(), "--system".into()],
                ))
                .await
                .context("removing the old startup service that ran as root")?;
            }
        }
    }
    crate::state::save_proxy_settings(&settings)?;
    let state = crate::state::dir();
    let argv = settings_args(&settings);
    let owner = service_owner()?;
    let task = windows_task(None);
    let (path, exe, binary) = match scope {
        ServiceScope::User => {
            let exe = std::env::current_exe()?;
            (
                user_service_path(&platform)?,
                stable_exe(&exe.canonicalize().unwrap_or(exe)),
                None,
            )
        }
        ServiceScope::System => {
            let path = system_service_path(&platform).context("no system unit on this platform")?;
            let binary = install_system_binary(&system_binary_path())?;
            (path, binary.clone(), Some(binary))
        }
    };
    let body = render_service(
        &platform,
        scope,
        &exe,
        &state,
        &argv,
        (owner.0, owner.1),
        &windows_account(),
    )?;
    // The service starts its own proxy: stop one started by hand (or an older
    // service's) so the two don't fight over the port.
    if crate::cli::proxy_running() {
        crate::cli::stop_proxy().await?;
    }
    write_unit(
        &path,
        &body,
        if scope == ServiceScope::System {
            0o644
        } else {
            0o600
        },
    )?;
    let p = path.to_string_lossy().into_owned();
    // Reinstalling restarts the service, so new options apply now.
    let loaded = match (platform.as_str(), scope) {
        ("macos", _) => launchd_load(&format!("gui/{}", owner.0), &path),
        ("linux", scope) => {
            let user = scope == ServiceScope::User;
            systemctl(user, &["daemon-reload"])
                .and_then(|_| systemctl(user, &["enable", SERVICE_UNIT]))
                .and_then(|_| systemctl(user, &["restart", SERVICE_UNIT]))
                .map(|_| ())
        }
        ("windows", _) => {
            let _ = exec("schtasks", &["/End", "/TN", &task]);
            exec("schtasks", &["/Create", "/TN", &task, "/XML", &p, "/F"])
                .and_then(|_| exec("schtasks", &["/Run", "/TN", &task]))
                .map(|_| ())
        }
        _ => unreachable!("render_service rejects other platforms"),
    };
    if let Err(e) = loaded {
        // Nothing half installed: no startup file and no record.
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(state.join("service.json"));
        // The next app starts the proxy again.
        let _ = fs::remove_file(state.join("proxy.stopped"));
        return Err(e.context("loading the startup service"));
    }
    let _ = fs::remove_file(state.join("proxy.stopped"));
    write_state(
        &state.join("service.json"),
        serde_json::to_vec_pretty(&ServiceRecord {
            platform: platform.clone(),
            path,
            state_dir: state.clone(),
            settings,
            scope: Some(match scope {
                ServiceScope::User => "user".into(),
                ServiceScope::System => "system".into(),
            }),
            task: (platform == "windows").then_some(task),
            binary: binary.clone(),
        })?,
    )?;
    if binary.is_some() {
        println!("The startup service runs as you, from a root-owned copy of porch; after upgrading Porch, run `porch service install` again.");
    }
    Ok(())
}
fn service_status() -> Result<()> {
    match fs::read_to_string(crate::state::dir().join("service.json")) {
        Ok(raw) => {
            let record: ServiceRecord = serde_json::from_str(&raw)?;
            println!(
                "installed: {}\nruns as: {}\nstate: {}\nHTTPS: {}\nport: {}\nTLDs: {}\nLAN: {}\nwildcard: {}",
                record.path.display(),
                match record.scope.as_deref() {
                    Some("user") => "you (login item)",
                    Some(_) => "you (system unit, may bind ports below 1024)",
                    None => "root",
                },
                record.state_dir.display(),
                record.settings.tls,
                record.settings.port,
                record.settings.tlds.join(","),
                record.settings.lan,
                record.settings.wildcard
            );
        }
        Err(_) if system_service_installed() => println!("installed (system-level, no record)"),
        Err(_) => println!("not installed"),
    }
    if let Some(warning) = service_warning() {
        println!("warning: {warning}");
    }
    Ok(())
}
/// `porch service status|uninstall`, synchronously; installing goes through
/// [`service_command`].
fn service(action: &str, args: &[String]) -> Result<()> {
    let platform = service_platform();
    match action {
        "status" => service_status(),
        "uninstall" if args.iter().any(|a| a == "--system") => {
            uninstall_system_scope(&platform)?;
            forget_service_record(&platform)
        }
        "uninstall" => service_uninstall(&platform),
        _ => bail!("usage: service install|status|uninstall"),
    }
}
async fn service_command(action: &str, args: &[String]) -> Result<()> {
    let platform = service_platform();
    match action {
        "install" => service_install(args).await,
        // The user part as this user; the system part (if any) with sudo.
        "uninstall"
            if !args.iter().any(|a| a == "--system")
                && cfg!(unix)
                && !is_root()
                && !system_paths_overridden()
                && system_service_installed() =>
        {
            let mut errors = Vec::new();
            if let Err(e) = uninstall_user_scope(&platform) {
                errors.push(e);
            }
            if let Err(e) = Box::pin(crate::cli::integration(
                "service",
                vec!["uninstall".into(), "--system".into()],
            ))
            .await
            {
                errors.push(e);
            }
            forget_service_record(&platform)?;
            match errors.into_iter().next() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
        _ => service(action, args),
    }
}

#[derive(Serialize, Deserialize)]
struct TrustRecord {
    platform: String,
    fingerprint: String,
    installed: Option<PathBuf>,
    update: Option<String>,
    windows: bool,
    /// `user` (no sudo: login keychain or NSS databases) or `system`. Records
    /// written before per-user trust existed are system-wide.
    #[serde(default = "system_scope")]
    scope: String,
    /// macOS login keychain holding a per-user CA.
    #[serde(default)]
    keychain: Option<PathBuf>,
    /// Linux NSS databases (Chrome/Chromium, Firefox profiles) holding a per-user CA.
    #[serde(default)]
    nss_dbs: Vec<PathBuf>,
    /// macOS trust limited to TLS (`-p ssl -p basic`). Trust installed before
    /// Porch 0.5.3 applies to every purpose until `porch trust` runs again.
    #[serde(default)]
    tls_only: bool,
}
fn system_scope() -> String {
    "system".into()
}
fn trust_record() -> Result<Option<TrustRecord>> {
    match fs::read(crate::state::dir().join("trust.json")) {
        Ok(raw) => Ok(Some(serde_json::from_slice(&raw)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
/// What the trust stores actually say about the current local CA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaTrust {
    /// No CA has been created yet.
    NoCa,
    /// Trusted, in this scope (`user` or `system`), checked in the store itself.
    Trusted(String),
    /// Porch never installed trust for this CA.
    NotInstalled,
    /// The recorded trust is for a different (older) CA.
    Stale,
    /// The record says installed, but the store no longer has it.
    Removed,
    /// The store couldn't be checked (tool missing); the record says `scope`.
    Unverified(String),
}
/// Check the current CA against both Porch's record and the real trust store.
pub fn ca_trust() -> CaTrust {
    let ca = crate::state::dir().join("ca.pem");
    let Ok(fingerprint) = ca_fingerprint(&ca) else {
        return CaTrust::NoCa;
    };
    let Ok(Some(record)) = trust_record() else {
        return CaTrust::NotInstalled;
    };
    if record.fingerprint != fingerprint {
        return CaTrust::Stale;
    }
    let ca_arg = ca.to_string_lossy().into_owned();
    let present: Option<bool> = match (record.platform.as_str(), record.scope.as_str()) {
        ("macos", _) => {
            // Evaluates the CA against the user's and system trust settings.
            Command::new("security")
                .args(["verify-cert", "-c", &ca_arg, "-p", "basic", "-L", "-q"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()
                .map(|s| s.success())
        }
        ("linux", "user") => {
            let name = nss_name(&fingerprint);
            let mut all = Some(true);
            for db in &record.nss_dbs {
                let sql = format!("sql:{}", db.display());
                match exec("certutil", &["-L", "-d", &sql]) {
                    Ok(listing) if !nss_listing_has(&listing, &name) => all = Some(false),
                    Ok(_) => {}
                    Err(_) => {
                        all = all.and(None);
                    }
                }
            }
            all
        }
        (_, "system") if record.installed.is_some() => {
            let installed = record.installed.as_ref().unwrap();
            Some(
                fs::read(installed)
                    .ok()
                    .zip(fs::read(&ca).ok())
                    .is_some_and(|(a, b)| a == b),
            )
        }
        ("windows", _) => exec("certutil", &["-user", "-verifystore", "Root", &fingerprint])
            .map(|_| true)
            .ok(),
        _ => None,
    };
    match present {
        Some(true) => CaTrust::Trusted(record.scope),
        Some(false) => CaTrust::Removed,
        None => CaTrust::Unverified(record.scope),
    }
}
/// Scope of the recorded CA trust (`user` or `system`), if any.
pub fn trust_scope() -> Option<String> {
    trust_record().ok().flatten().map(|r| r.scope)
}
fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set; cannot locate per-user trust stores")
}
fn nss_name(fingerprint: &str) -> String {
    format!("Porch Local CA {}", &fingerprint[..12])
}
/// Per-user NSS databases: Chrome/Chromium's shared one plus every Firefox profile.
fn nss_databases(home: &Path) -> Vec<PathBuf> {
    let mut dbs = vec![home.join(".pki/nssdb")];
    for base in [
        ".mozilla/firefox",
        "snap/firefox/common/.mozilla/firefox",
        ".var/app/org.mozilla.firefox/.mozilla/firefox",
    ] {
        let Ok(entries) = fs::read_dir(home.join(base)) else {
            continue;
        };
        let mut profiles: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join("cert9.db").is_file())
            .collect();
        profiles.sort();
        dbs.extend(profiles);
    }
    dbs
}
fn macos_login_keychain(home: &Path) -> PathBuf {
    let dir = home.join("Library/Keychains");
    let legacy = dir.join("login.keychain");
    if !dir.join("login.keychain-db").exists() && legacy.exists() {
        legacy
    } else {
        dir.join("login.keychain-db")
    }
}
fn find_program(name: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|p| p.join(name).is_file())
}
fn trust_platform() -> String {
    std::env::var("PORCH_TRUST_PLATFORM").unwrap_or_else(|_| std::env::consts::OS.into())
}
/// DER of the first certificate in a PEM file, the same one `openssl x509 -in` reads.
fn ca_der(ca: &Path) -> Result<Vec<u8>> {
    let pem = fs::read(ca).with_context(|| format!("could not read {}", ca.display()))?;
    let der = crate::pem::certs(&mut std::io::BufReader::new(pem.as_slice()))
        .next()
        .with_context(|| format!("no PEM certificate in {}", ca.display()))?
        .with_context(|| format!("invalid PEM certificate in {}", ca.display()))?;
    Ok(der.to_vec())
}
/// Uppercase hex SHA-1 of the certificate DER, without separators. This is the
/// format `openssl x509 -fingerprint -sha1` produced (minus colons) in Porch 0.3.x,
/// so existing trust records keep matching.
fn ca_fingerprint(ca: &Path) -> Result<String> {
    use sha1::Sha1;
    let der = ca_der(ca)?;
    Ok(Sha1::digest(&der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect())
}
fn verify_porch_ca(path: &Path) -> Result<()> {
    let der = ca_der(path)?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der)
        .map_err(|e| anyhow::anyhow!("invalid CA certificate {}: {e}", path.display()))?;
    if !cert
        .subject()
        .iter_common_name()
        .any(|cn| cn.as_str() == Ok("Porch Local CA"))
    {
        bail!("refusing to alter trust for a certificate not issued as Porch Local CA");
    }
    Ok(())
}
/// Whether a successful `certutil -L -d <db>` listing contains `nickname`. Absence is
/// only ever concluded from a full listing, never from a failed lookup's error text.
fn nss_listing_has(listing: &str, nickname: &str) -> bool {
    listing.lines().any(|line| {
        line.strip_prefix(nickname)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace) && rest.trim().contains(','))
    })
}
fn linux_ca_layout(os_release: &str) -> (&'static str, &'static str) {
    let os = os_release.to_lowercase();
    if os.contains("arch") {
        (
            "/etc/ca-certificates/trust-source/anchors",
            "update-ca-trust",
        )
    } else if ["fedora", "rhel", "centos"].iter().any(|s| os.contains(s)) {
        ("/etc/pki/ca-trust/source/anchors", "update-ca-trust")
    } else if os.contains("suse") {
        ("/etc/pki/trust/anchors", "update-ca-certificates")
    } else if os.contains("debian")
        || os.contains("ubuntu")
        || find_program("update-ca-certificates")
    {
        ("/usr/local/share/ca-certificates", "update-ca-certificates")
    } else if Path::new("/etc/ca-certificates/trust-source").is_dir() {
        (
            "/etc/ca-certificates/trust-source/anchors",
            "update-ca-trust",
        )
    } else {
        ("/etc/pki/ca-trust/source/anchors", "update-ca-trust")
    }
}
fn trust(system: bool) -> Result<()> {
    let ca = crate::state::dir().join("ca.pem");
    if !ca.is_file() {
        bail!("local CA missing; start the HTTPS proxy first");
    }
    verify_porch_ca(&ca)?;
    let fingerprint = ca_fingerprint(&ca)?;
    // A new CA replaced the trusted one (the domains changed): remove the old
    // CA's trust first, so no retired root stays trusted.
    if trust_record()?.is_some_and(|r| r.fingerprint != fingerprint) {
        untrust().context("couldn't remove trust for the previous Porch CA; trust record kept, run `porch trust` again")?;
    }
    if let Some(existing) = trust_record()? {
        if existing.scope == "system" && !system && existing.fingerprint == fingerprint {
            println!("Porch local CA is already trusted system-wide");
            return Ok(());
        }
        if existing.scope == "user" && system {
            bail!("the Porch CA is trusted for this user; its per-user trust must be removed first, so run `porch trust --system` from a normal (non-root) shell");
        }
    }
    let platform = trust_platform();
    let mut record = TrustRecord {
        platform: platform.clone(),
        fingerprint,
        installed: None,
        update: None,
        windows: false,
        // Windows trust always goes to the current user's Root store.
        scope: if system && platform != "windows" {
            "system"
        } else {
            "user"
        }
        .into(),
        keychain: None,
        nss_dbs: Vec::new(),
        tls_only: true,
    };
    let ca_arg = ca.to_string_lossy();
    match platform.as_str() {
        "macos" if !system => {
            // The login keychain needs no sudo; macOS confirms with a password or Touch ID dialog.
            let keychain = macos_login_keychain(&user_home()?);
            exec_prompt(
                "security",
                &[
                    "add-trusted-cert",
                    "-r",
                    "trustRoot",
                    // Trusted for TLS (and basic path checks) only, never for
                    // S/MIME or code signing.
                    "-p",
                    "ssl",
                    "-p",
                    "basic",
                    "-k",
                    &keychain.to_string_lossy(),
                    &ca_arg,
                ],
            )?;
            record.keychain = Some(keychain);
        }
        "linux" if !system => {
            if !find_program("certutil") {
                bail!("per-user trust needs NSS certutil: install libnss3-tools (Debian/Ubuntu) or nss-tools (Fedora/Arch), or trust system-wide with `porch trust --system` (asks for sudo)");
            }
            let name = nss_name(&record.fingerprint);
            for db in nss_databases(&user_home()?) {
                if !db.exists() {
                    fs::create_dir_all(&db)?;
                }
                let sql = format!("sql:{}", db.display());
                if !db.join("cert9.db").exists() {
                    exec("certutil", &["-N", "-d", &sql, "--empty-password"])?;
                }
                exec(
                    "certutil",
                    &["-A", "-d", &sql, "-t", "C,,", "-n", &name, "-i", &ca_arg],
                )?;
                record.nss_dbs.push(db);
                // Record as we go so a partial failure can still be cleaned up.
                write_state(
                    &crate::state::dir().join("trust.json"),
                    serde_json::to_vec_pretty(&record)?,
                )?;
            }
            trust_wsl_windows(&ca_arg, &mut record)?;
        }
        "macos" => {
            exec_prompt(
                "security",
                &[
                    "add-trusted-cert",
                    "-d",
                    "-r",
                    "trustRoot",
                    "-p",
                    "ssl",
                    "-p",
                    "basic",
                    "-k",
                    "/Library/Keychains/System.keychain",
                    &ca_arg,
                ],
            )?;
        }
        "windows" => {
            // Windows asks for confirmation before adding a root.
            exec_prompt("certutil", &["-user", "-addstore", "Root", &ca_arg])?;
            record.windows = true;
        }
        "linux" => {
            let release_path = std::env::var_os("PORCH_OS_RELEASE")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/etc/os-release".into());
            let (root, update) =
                linux_ca_layout(&fs::read_to_string(release_path).unwrap_or_default());
            if !find_program(update) {
                bail!("no supported CA update tool {update} found; install ca-certificates");
            }
            let root = std::env::var_os("PORCH_TRUST_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.into());
            fs::create_dir_all(&root)?;
            let target = root.join(format!("porch-{}.crt", &record.fingerprint[..12]));
            // World-readable like every other anchor: tools reading the hashed
            // certificate directory as a normal user must be able to open it.
            write_anchor(&target, &fs::read(&ca)?)?;
            record.installed = Some(target);
            record.update = Some(update.into());
            // Record before updating so a failed update can be retried/cleaned safely.
            write_state(
                &crate::state::dir().join("trust.json"),
                serde_json::to_vec_pretty(&record)?,
            )?;
            exec(update, &[])?;
            trust_wsl_windows(&ca_arg, &mut record)?;
        }
        _ => bail!("unsupported trust store platform"),
    }
    write_state(
        &crate::state::dir().join("trust.json"),
        serde_json::to_vec_pretty(&record)?,
    )?;
    println!(
        "Porch local CA trusted {}",
        if record.scope == "system" {
            "system-wide"
        } else {
            "for this user"
        }
    );
    Ok(())
}
/// A public trust anchor: mode 0644, replacing whatever was there.
fn write_anchor(target: &Path, der_or_pem: &[u8]) -> Result<()> {
    let _ = fs::remove_file(target);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(target)?;
    #[cfg(unix)]
    {
        // The umask may have narrowed the mode.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    file.write_all(der_or_pem)?;
    file.sync_all()?;
    Ok(())
}
/// Whether `keychain` holds a certificate with this SHA-1 fingerprint, from a
/// complete listing. `None` when the keychain couldn't be listed.
fn keychain_has(keychain: &str, fingerprint: &str) -> Option<bool> {
    let listing = exec("security", &["find-certificate", "-a", "-Z", keychain]).ok()?;
    Some(listing_has_sha1(&listing, fingerprint))
}
fn listing_has_sha1(listing: &str, fingerprint: &str) -> bool {
    listing.lines().any(|line| {
        line.trim()
            .strip_prefix("SHA-1 hash:")
            .is_some_and(|h| h.trim().eq_ignore_ascii_case(fingerprint))
    })
}
/// The PEM of the certificate with this fingerprint in `keychain`, for removing
/// its trust setting when Porch's own copy of the CA is gone.
fn keychain_pem(keychain: &str, fingerprint: &str) -> Option<String> {
    let listing = exec(
        "security",
        &["find-certificate", "-a", "-Z", "-p", keychain],
    )
    .ok()?;
    pem_with_sha1(&listing, fingerprint)
}
/// The PEM block following `SHA-1 hash: <fingerprint>` in a `find-certificate -Z -p` listing.
fn pem_with_sha1(listing: &str, fingerprint: &str) -> Option<String> {
    let mut current = false;
    let mut pem = String::new();
    for line in listing.lines() {
        if let Some(h) = line.trim().strip_prefix("SHA-1 hash:") {
            current = h.trim().eq_ignore_ascii_case(fingerprint);
            pem.clear();
            continue;
        }
        if current {
            pem.push_str(line);
            pem.push('\n');
            if line.starts_with("-----END CERTIFICATE-----") {
                return Some(pem);
            }
        }
    }
    None
}
/// Remove a macOS trust setting (`-d`: the admin one) for the certificate with
/// this fingerprint, from Porch's copy of it or the keychain's. A setting that
/// is already gone is fine.
fn macos_remove_trust(ca: Option<&Path>, keychain: &str, fingerprint: &str, admin: bool) {
    let exported;
    let path = match ca {
        Some(path) => path.to_path_buf(),
        None => {
            let Some(pem) = keychain_pem(keychain, fingerprint) else {
                return;
            };
            exported = crate::state::dir().join(format!(".untrust-{}.pem", std::process::id()));
            if write_state(&exported, pem).is_err() {
                return;
            }
            exported.clone()
        }
    };
    let path_arg = path.to_string_lossy().into_owned();
    let mut args = vec!["remove-trusted-cert"];
    if admin {
        args.push("-d");
    }
    args.push(&path_arg);
    // macOS confirms a trust change with a password or Touch ID dialog.
    let _ = exec_prompt("security", &args);
    if ca.is_none() {
        let _ = fs::remove_file(&path);
    }
}
/// Delete the certificate with this fingerprint from `keychain`, unless a
/// complete listing shows it is already gone (deleted in Keychain Access).
fn macos_delete_certificate(keychain: &str, fingerprint: &str) -> Result<()> {
    if keychain_has(keychain, fingerprint) == Some(false) {
        return Ok(());
    }
    match exec(
        "security",
        &["delete-certificate", "-Z", fingerprint, keychain],
    ) {
        Ok(_) => Ok(()),
        // Gone between the listing and the delete counts as removed.
        Err(_) if keychain_has(keychain, fingerprint) == Some(false) => Ok(()),
        Err(e) => Err(e),
    }
}
/// WSL: browsers run on Windows, so also trust the CA in the Windows user store.
fn trust_wsl_windows(ca_arg: &str, record: &mut TrustRecord) -> Result<()> {
    let wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
        || fs::read_to_string("/proc/version")
            .is_ok_and(|s| s.to_lowercase().contains("microsoft"));
    if wsl && find_program("certutil.exe") {
        let windows_path = exec("wslpath", &["-w", ca_arg])?;
        exec_prompt(
            "certutil.exe",
            &["-user", "-addstore", "Root", windows_path.trim()],
        )?;
        record.windows = true;
    }
    Ok(())
}
/// Remove per-user trust; a system-wide record is left for the elevated `clean`.
/// Runs as the invoking user so per-user stores keep their ownership.
pub fn untrust_user_scope() -> Result<()> {
    if trust_record()?.is_some_and(|r| r.scope == "user") {
        untrust()?;
    }
    Ok(())
}
fn untrust() -> Result<()> {
    let path = crate::state::dir().join("trust.json");
    let Some(r) = trust_record()? else {
        return Ok(());
    };
    if r.fingerprint.len() != 40 || !r.fingerprint.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid recorded trust fingerprint; certificate retained");
    }
    // The trusted CA may be the current one or one retired when the domains
    // changed (`ca.pem.retired-*`): find the file with the recorded fingerprint.
    let dir = crate::state::dir();
    let mut candidates = vec![dir.join("ca.pem")];
    if let Ok(entries) = fs::read_dir(&dir) {
        candidates.extend(
            entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("ca.pem.retired-")
                })
                .map(|e| e.path()),
        );
    }
    // Porch's own copy may be gone (deleted by hand). Every removal below is
    // keyed by the recorded fingerprint (or a nickname or path derived from it),
    // so it can still only remove the certificate Porch installed.
    let ca = candidates
        .into_iter()
        .find(|c| ca_fingerprint(c).is_ok_and(|f| f == r.fingerprint));
    if let Some(ca) = &ca {
        verify_porch_ca(ca)?;
    }
    if r.update
        .as_ref()
        .is_some_and(|s| !matches!(s.as_str(), "update-ca-certificates" | "update-ca-trust"))
    {
        bail!("invalid recorded trust update command");
    }
    if let Some(path) = &r.installed {
        let expected = format!("porch-{}.crt", &r.fingerprint[..12]);
        let allowed = std::env::var_os("PORCH_TRUST_DIR")
            .map(PathBuf::from)
            .map(|dir| vec![dir])
            .unwrap_or_else(|| {
                vec![
                    PathBuf::from("/usr/local/share/ca-certificates"),
                    PathBuf::from("/etc/pki/ca-trust/source/anchors"),
                    PathBuf::from("/etc/ca-certificates/trust-source/anchors"),
                    PathBuf::from("/etc/pki/trust/anchors"),
                ]
            });
        if path.file_name().and_then(|s| s.to_str()) != Some(expected.as_str())
            || !allowed
                .iter()
                .any(|dir| path.parent() == Some(dir.as_path()))
        {
            bail!("invalid installed CA path; refusing removal");
        }
    }
    match r.platform.as_str() {
        "macos" if r.scope == "user" => {
            let home = user_home()?;
            let keychain = r
                .keychain
                .clone()
                .unwrap_or_else(|| macos_login_keychain(&home));
            if keychain.parent() != Some(home.join("Library/Keychains").as_path()) {
                bail!("invalid recorded keychain path; refusing removal");
            }
            let keychain = keychain.to_string_lossy();
            // Drop the user trust setting first; a missing setting is not an error.
            macos_remove_trust(ca.as_deref(), &keychain, &r.fingerprint, false);
            macos_delete_certificate(&keychain, &r.fingerprint)?;
        }
        "linux" if r.scope == "user" => {
            let home = user_home()?;
            let name = nss_name(&r.fingerprint);
            for db in &r.nss_dbs {
                if !db.is_absolute() || !db.starts_with(&home) {
                    bail!("invalid recorded NSS database path; refusing removal");
                }
                match fs::metadata(db) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        return Err(anyhow::Error::from(e).context(format!(
                            "could not access NSS database {}; trust record kept for retry",
                            db.display()
                        )))
                    }
                }
                let sql = format!("sql:{}", db.display());
                // Only delete the nickname this fingerprint installed, and only where a full
                // listing shows it. Any certutil failure keeps the trust record for retry.
                let listing = exec("certutil", &["-L", "-d", &sql]).with_context(|| {
                    format!(
                        "could not list {} to check for the Porch CA; trust record kept for retry",
                        db.display()
                    )
                })?;
                if nss_listing_has(&listing, &name) {
                    exec("certutil", &["-D", "-d", &sql, "-n", &name]).with_context(|| {
                        format!("could not remove Porch CA from {}", db.display())
                    })?;
                }
            }
        }
        "macos" => {
            let keychain = "/Library/Keychains/System.keychain";
            // The admin trust setting outlives the certificate: remove it too.
            macos_remove_trust(ca.as_deref(), keychain, &r.fingerprint, true);
            macos_delete_certificate(keychain, &r.fingerprint)?;
        }
        "linux" => {
            if let Some(p) = r.installed {
                if p.exists() {
                    // Only remove the certificate installed by this state directory.
                    if ca_fingerprint(&p)? != r.fingerprint {
                        bail!("installed CA changed; refusing to delete unrelated certificate");
                    }
                    fs::remove_file(p)?;
                }
            }
            if let Some(update) = r.update {
                exec(&update, &[])?;
            }
        }
        "windows" => {}
        _ => bail!("invalid recorded trust platform"),
    }
    if r.windows {
        let certutil = if r.platform == "linux" {
            "certutil.exe"
        } else {
            "certutil"
        };
        // Windows asks for confirmation before deleting a root.
        if let Err(e) = exec_prompt(certutil, &["-user", "-delstore", "Root", &r.fingerprint]) {
            // Already gone (removed in the certificate manager) counts as
            // removed, but only when certutil itself runs and can't find it.
            let runs = Command::new(certutil)
                .arg("-?")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok();
            let found = exec(certutil, &["-user", "-verifystore", "Root", &r.fingerprint]).is_ok();
            if !runs || found {
                return Err(e);
            }
        }
    }
    fs::remove_file(path)?;
    Ok(())
}
pub fn has_managed_hosts() -> bool {
    fs::read_to_string(hosts_path()).is_ok_and(|s| s.contains("# porch-start"))
}
pub fn clean() -> Result<()> {
    // Trust removal must succeed before the coordinator deletes the CA and state.
    prune_shares(true)?;
    untrust()?;
    if crate::state::dir().join("service.json").exists() {
        // As `porch service uninstall`, but never another install's login item.
        uninstall_scopes(&service_platform(), false)?;
    }
    if has_managed_hosts() {
        hosts("clean")?;
    }
    Ok(())
}

pub struct ShareSession {
    pub urls: BTreeMap<String, String>,
    children: Vec<tokio::process::Child>,
    tailscale: Vec<(String, u16)>,
    files: Vec<PathBuf>,
    registry: Option<PathBuf>,
    tree_guards: Vec<crate::process::TreeGuard>,
}
impl ShareSession {
    /// Resolve as soon as an owned tunnel exits; the launcher should stop its app.
    pub async fn wait_failure(&mut self) -> Result<()> {
        if self.children.is_empty() {
            return std::future::pending().await;
        }
        let waits = self
            .children
            .iter_mut()
            .map(|child| Box::pin(child.wait()))
            .collect::<Vec<_>>();
        let (status, _, _) = futures_util::future::select_all(waits).await;
        bail!("sharing tunnel exited unexpectedly: {}", status?);
    }
}
impl ShareSession {
    /// End the tunnels (SIGTERM, then SIGKILL after a short grace period),
    /// turn Tailscale sharing off and remove the session's files. Blocking.
    fn cleanup(&mut self) {
        #[cfg(unix)]
        for child in &mut self.children {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
            }
        }
        // Let tunnels close their connections cleanly (cloudflared is started
        // with a 1 s grace period).
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while self
            .children
            .iter_mut()
            .any(|c| matches!(c.try_wait(), Ok(None)))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(25));
        }
        for child in &mut self.children {
            if matches!(child.try_wait(), Ok(None)) {
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                }
                let _ = child.start_kill();
            }
        }
        for (mode, port) in &self.tailscale {
            let _ = exec(
                "tailscale",
                &[mode, "--yes", &format!("--https={port}"), "off"],
            );
        }
        if let Some(path) = &self.registry {
            let _ = fs::remove_file(path);
        }
        for p in &self.files {
            let _ = fs::remove_file(p);
        }
        self.children.clear();
        self.tailscale.clear();
        self.registry = None;
        self.files.clear();
    }
}
impl Drop for ShareSession {
    fn drop(&mut self) {
        // The cleanup waits on processes and runs `tailscale`: on a
        // multi-threaded runtime, move this worker's other tasks elsewhere
        // while it does.
        match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
            Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
                tokio::task::block_in_place(|| self.cleanup())
            }
            _ => self.cleanup(),
        }
    }
}
fn used_tailscale_ports(value: &Value, set: &mut BTreeSet<u16>) {
    if let Some(web) = value["Web"].as_object() {
        for host in web.keys() {
            if let Some(port) = host.rsplit(':').next().and_then(|p| p.parse().ok()) {
                set.insert(port);
            }
        }
    }
    if let Some(tcp) = value["TCP"].as_object() {
        for p in tcp.keys() {
            if let Ok(p) = p.parse() {
                set.insert(p);
            }
        }
    }
    if let Some(children) = value["Foreground"].as_object() {
        for v in children.values() {
            used_tailscale_ports(v, set);
        }
    }
}
fn capability(value: &Value, name: &str) -> bool {
    value["Self"]["Capabilities"].as_array().is_some_and(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .any(|s| s == name || s.ends_with(&format!("/{name}")))
    }) || value["Self"]["CapMap"].as_object().is_some_and(|o| {
        o.keys()
            .any(|s| s == name || s.ends_with(&format!("/{name}")))
    })
}
fn extract_tunnel_url(provider: &str, output: &str) -> Option<String> {
    if provider == "cloudflare"
        && !output.contains("Registered tunnel connection")
        && !output.contains(" registered")
    {
        return None;
    }
    for (index, _) in output.match_indices("https://") {
        let candidate = output[index..]
            .split(|c: char| c.is_whitespace() || ['"', '\'', '<', '>'].contains(&c))
            .next()?
            .trim_end_matches([')', ',', '.']);
        let Ok(url) = url::Url::parse(candidate) else {
            continue;
        };
        let host = url.host_str()?;
        if provider == "cloudflare" {
            if host.ends_with(".trycloudflare.com") && !host.starts_with("api.") {
                return Some(candidate.trim_end_matches('/').into());
            }
        } else {
            let before = &output[..index];
            if ["forwarding", "url=", "\"url\"", "started tunnel"]
                .iter()
                .any(|s| before.to_lowercase().contains(s))
                && host != "ngrok.com"
                && !host.ends_with(".ngrok.com")
            {
                return Some(candidate.trim_end_matches('/').into());
            }
        }
    }
    None
}
async fn start_tunnel(provider: &str, port: u16, session: &mut ShareSession) -> Result<String> {
    let binary = if provider == "ngrok" {
        "ngrok"
    } else {
        "cloudflared"
    };
    exec(
        binary,
        &[if provider == "ngrok" {
            "version"
        } else {
            "--version"
        }],
    )?;
    let target = format!("http://127.0.0.1:{port}");
    let args = if provider == "ngrok" {
        vec![
            "http".into(),
            "--log=stdout".into(),
            "--log-format=logfmt".into(),
            "--host-header=rewrite".into(),
            target,
        ]
    } else {
        let config =
            crate::state::dir().join(format!("cloudflare-{}-{port}.json", std::process::id()));
        write_state(&config, "{}")?;
        session.files.push(config.clone());
        vec![
            "tunnel".into(),
            "--no-autoupdate".into(),
            "--grace-period".into(),
            "1s".into(),
            "--config".into(),
            config.to_string_lossy().into_owned(),
            "--url".into(),
            target,
        ]
    };
    let mut command = tokio::process::Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::process::configure(&mut command);
    let mut child = command.spawn()?;
    let tree_guard = crate::process::TreeGuard::new(&child)?;
    let stdout = child.stdout.take().context("missing tunnel stdout")?;
    let stderr = child.stderr.take().context("missing tunnel stderr")?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);
    for reader in [
        Box::pin(stdout) as std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        Box::pin(stderr),
    ] {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).await.is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let timeout = env("TUNNEL_TIMEOUT_MS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000u64);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
    let mut output = String::new();
    let result = loop {
        tokio::select! {
            line=rx.recv()=>{match line{Some(line)=>{output.push_str(&line);output.push('\n');if output.len()>32768{let mut cut=output.len()-16384;while !output.is_char_boundary(cut){cut+=1;}output.drain(..cut);}
            if let Some(url)=extract_tunnel_url(provider,&output){break Ok(url);}},None=>break Err(anyhow::anyhow!("{binary} exited before connecting: {}",output.trim()))}},
            _=tokio::time::sleep_until(deadline)=>break Err(anyhow::anyhow!("timed out waiting for {binary} to connect")),
            status=child.wait()=>break Err(anyhow::anyhow!("{binary} exited before connecting ({:?}): {}",status?,output.trim())),
        }
    };
    match result {
        Ok(url) => {
            session.children.push(child);
            session.tree_guards.push(tree_guard);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            Ok(url)
        }
        Err(e) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
            }
            let _ = child.kill().await;
            Err(e)
        }
    }
}
/// Serializes Tailscale port allocation between this user's Porch launchers,
/// whatever their state directory (`tailscale serve` is machine-wide). It
/// lives in a directory only this user can write: `$XDG_RUNTIME_DIR` when set,
/// else the temporary directory (per user on macOS and Windows). If another
/// user got there first (a shared /tmp), the 0700 state directory is used, so
/// they can't stop `--tailscale` from working.
fn tailscale_lock() -> Result<fs::File> {
    #[cfg(unix)]
    let user = unsafe { libc::geteuid() }.to_string();
    #[cfg(not(unix))]
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let key = format!("{:x}", Sha256::digest(user.as_bytes()));
    let name = format!("porch-tailscale-{}.lock", &key[..16]);
    let open = |path: PathBuf| -> Result<fs::File> {
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if file.metadata()?.uid() != unsafe { libc::geteuid() } {
                bail!("Tailscale lock belongs to another user");
            }
        }
        Ok(file)
    };
    let shared = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty() && Path::new(d).is_dir())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let file = match open(shared.join(&name)) {
        Ok(file) => file,
        Err(_) => open(crate::state::dir().join("tailscale.lock"))?,
    };
    file.lock_exclusive()?;
    Ok(file)
}
pub async fn start_shares(port: u16, providers: &[String]) -> Result<ShareSession> {
    if port == 0 {
        bail!("share target port must be nonzero");
    }
    crate::state::load()?;
    let mut session = ShareSession {
        urls: BTreeMap::new(),
        children: Vec::new(),
        tailscale: Vec::new(),
        files: Vec::new(),
        registry: None,
        tree_guards: Vec::new(),
    };
    let funnel = providers.iter().any(|p| p == "funnel");
    let tailscale = funnel
        || providers
            .iter()
            .any(|p| matches!(p.as_str(), "tailscale" | "serve"));
    if tailscale {
        // Serialize status+registration across Porch launchers to reserve independent HTTPS ports.
        let _lock = tailscale_lock()?;
        exec("tailscale", &["version"])?;
        let status: Value = serde_json::from_str(&exec("tailscale", &["status", "--json"])?)?;
        if !capability(&status, "https") {
            bail!("Tailscale HTTPS certificates are not enabled; enable HTTPS in tailnet DNS settings");
        }
        if funnel && !capability(&status, "funnel") {
            bail!("Tailscale Funnel is not enabled for this node");
        }
        let dns = status["Self"]["DNSName"]
            .as_str()
            .map(|s| s.trim_end_matches('.').to_owned())
            .or_else(|| {
                Some(format!(
                    "{}.{}",
                    status["Self"]["HostName"].as_str()?,
                    status["CurrentTailnet"]["MagicDNSSuffix"]
                        .as_str()?
                        .trim_end_matches('.')
                ))
            })
            .context("Tailscale is not connected or has no DNS name")?;
        valid_hostname(&dns)?;
        let used: Value = serde_json::from_str(&exec("tailscale", &["serve", "status", "--json"])?)
            .context("invalid Tailscale serve status")?;
        let mut ports = BTreeSet::new();
        used_tailscale_ports(&used, &mut ports);
        let available = if funnel {
            [443, 8443, 10000].into_iter().find(|p| !ports.contains(p))
        } else {
            std::iter::once(443)
                .chain(8443..=65535)
                .find(|p| !ports.contains(p))
        }
        .context("all Tailscale sharing ports are occupied")?;
        let mode = if funnel { "funnel" } else { "serve" };
        exec(
            "tailscale",
            &[
                mode,
                "--bg",
                "--yes",
                &format!("--https={available}"),
                &format!("http://127.0.0.1:{port}"),
            ],
        )?;
        session.tailscale.push((mode.into(), available));
        session.urls.insert(
            "PORTLESS_TAILSCALE_URL".into(),
            format!(
                "https://{dns}{}",
                if available == 443 {
                    String::new()
                } else {
                    format!(":{available}")
                }
            ),
        );
    }
    for provider in ["ngrok", "cloudflare"] {
        if providers
            .iter()
            .any(|p| p == provider || (provider == "cloudflare" && p == "cloudflared"))
        {
            let url = start_tunnel(provider, port, &mut session).await?;
            session
                .urls
                .insert(format!("PORTLESS_{}_URL", provider.to_uppercase()), url);
        }
    }
    for p in providers {
        if !matches!(
            p.as_str(),
            "tailscale" | "serve" | "funnel" | "ngrok" | "cloudflare" | "cloudflared"
        ) {
            bail!("unknown sharing provider {p}");
        }
    }
    if !providers.is_empty() {
        let path = crate::state::dir().join(format!("sharing-{}-{port}.json", std::process::id()));
        let owner =
            crate::state::identity(std::process::id()).context("cannot identify sharing owner")?;
        let tunnels = session
            .children
            .iter()
            .filter_map(|c| c.id().and_then(crate::state::identity))
            .collect();
        write_state(
            &path,
            serde_json::to_vec(&SharingRecord {
                owner,
                local_port: port,
                tailscale: session.tailscale.clone(),
                tunnels,
            })?,
        )?;
        session.registry = Some(path);
    }
    let porch_urls: Vec<_> = session
        .urls
        .iter()
        .filter_map(|(key, value)| {
            key.strip_prefix("PORTLESS_")
                .map(|suffix| (format!("PORCH_{suffix}"), value.clone()))
        })
        .collect();
    session.urls.extend(porch_urls);
    Ok(session)
}
#[derive(Serialize, Deserialize)]
struct SharingRecord {
    owner: crate::state::ProcessIdentity,
    local_port: u16,
    tailscale: Vec<(String, u16)>,
    tunnels: Vec<crate::state::ProcessIdentity>,
}
fn contains_target(value: &Value, target: &str) -> bool {
    match value {
        Value::String(s) => s.trim_end_matches('/') == target,
        Value::Object(o) => o.values().any(|v| contains_target(v, target)),
        Value::Array(a) => a.iter().any(|v| contains_target(v, target)),
        _ => false,
    }
}
fn owned_by_state_user(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(state) = fs::metadata(crate::state::dir()) else {
            return false;
        };
        let Ok(uid) = exec("ps", &["-p", &pid.to_string(), "-o", "uid="]) else {
            return false;
        };
        uid.trim().parse::<u32>().ok() == Some(state.uid())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}
/// End sharing left behind by launchers that are gone. One bad record never
/// stops the others: an unreadable one is removed, and a Tailscale failure is
/// reported and (outside `--force`) kept for the next prune.
pub fn prune_shares(force: bool) -> Result<()> {
    for entry in fs::read_dir(crate::state::dir())? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("sharing-")
            || !name.ends_with(".json")
            || !entry.file_type().is_ok_and(|t| t.is_file())
        {
            continue;
        }
        let record: SharingRecord = match fs::read(entry.path())
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(serde_json::from_slice(&b)?))
        {
            Ok(record) => record,
            Err(e) => {
                eprintln!("Removing unreadable sharing record {name}: {e}");
                let _ = fs::remove_file(entry.path());
                continue;
            }
        };
        if crate::state::matches(&record.owner) {
            continue;
        }
        for tunnel in record.tunnels {
            if crate::state::matches(&tunnel) && owned_by_state_user(tunnel.pid) {
                #[cfg(unix)]
                unsafe {
                    libc::kill(
                        -(tunnel.pid as i32),
                        if force { libc::SIGKILL } else { libc::SIGTERM },
                    );
                }
                // The tunnel's launcher is gone, so nothing shares its console
                // to interrupt it: end it outright.
                #[cfg(windows)]
                {
                    let _ = force;
                    crate::shutdown::kill_tree(tunnel.pid);
                }
            }
        }
        let mut tailscale_failed = false;
        for (mode, port) in &record.tailscale {
            let result = (|| -> Result<()> {
                let raw = exec("tailscale", &["serve", "status", "--json"])?;
                let value: Value = serde_json::from_str(&raw)?;
                let host_target = format!("http://127.0.0.1:{}", record.local_port);
                let ours = value["Web"].as_object().is_some_and(|o| {
                    o.iter().any(|(host, v)| {
                        host.ends_with(&format!(":{port}")) && contains_target(v, &host_target)
                    })
                });
                if ours {
                    exec(
                        "tailscale",
                        &[mode, "--yes", &format!("--https={port}"), "off"],
                    )?;
                }
                Ok(())
            })();
            if let Err(e) = result {
                eprintln!("Couldn't turn off Tailscale sharing on port {port}: {e:#}");
                tailscale_failed = true;
            }
        }
        if !tailscale_failed || force {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn lan_ip(settings: &crate::state::ProxySettings) -> Result<IpAddr> {
    if let Some(ip) = settings.lan_ip.as_ref().or(env("LAN_IP").as_ref()) {
        return Ok(ip.parse()?);
    }
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("1.1.1.1:53")?;
    Ok(socket.local_addr()?.ip())
}
struct Announcements {
    daemon: mdns_sd::ServiceDaemon,
    names: Vec<String>,
}
impl Drop for Announcements {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = self.daemon.unregister(name);
        }
        let _ = self.daemon.shutdown();
    }
}
/// Announce every live route over mDNS, following route and address changes.
/// It never gives up: a route that can't be announced is logged and skipped,
/// and state or mDNS daemon errors are retried.
pub async fn lan_watch(settings: crate::state::ProxySettings) -> Result<()> {
    let mut guard: Option<Announcements> = None;
    let mut previous = Vec::new();
    let mut previous_ip = None;
    // Report each distinct problem once, not every two seconds.
    let mut reported = BTreeSet::new();
    let mut report = |message: String| {
        if reported.len() > 256 {
            reported.clear();
        }
        if reported.insert(message.clone()) {
            eprintln!("LAN announcements: {message}");
        }
    };
    loop {
        tokio::time::sleep(Duration::from_secs(if guard.is_some() { 2 } else { 0 })).await;
        let ip = match lan_ip(&settings) {
            Ok(ip) => ip,
            Err(e) => {
                report(format!("address detection: {e}"));
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let state = match crate::state::load() {
            Ok(state) => state,
            Err(e) => {
                report(format!("reading routes: {e:#}"));
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let mut names = Vec::new();
        for r in state
            .routes
            .iter()
            .filter(|r| crate::state::route_alive(&state, r))
        {
            match route_hosts(&r.name, &settings) {
                Ok(hosts) => names.extend(hosts),
                Err(e) => report(format!("skipping {}: {e}", r.name)),
            }
        }
        names.sort();
        names.dedup();
        if guard.is_some() && previous_ip == Some(ip) && names == previous {
            continue;
        }
        if guard.is_none() {
            match mdns_sd::ServiceDaemon::new() {
                Ok(daemon) => {
                    guard = Some(Announcements {
                        daemon,
                        names: Vec::new(),
                    })
                }
                Err(e) => {
                    report(format!("starting mDNS: {e}"));
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }
        }
        let announcements = guard.as_mut().expect("daemon started above");
        for name in announcements.names.drain(..) {
            let _ = announcements.daemon.unregister(&name);
        }
        let mut daemon_failed = false;
        for host in &names {
            let instance = host.trim_end_matches(".local");
            let info = match mdns_sd::ServiceInfo::new(
                if settings.tls {
                    "_https._tcp.local."
                } else {
                    "_http._tcp.local."
                },
                instance,
                &format!("{host}."),
                ip,
                settings.port,
                None,
            ) {
                Ok(info) => info,
                Err(e) => {
                    report(format!("skipping {host}: {e}"));
                    continue;
                }
            };
            let fullname = info.get_fullname().to_owned();
            match announcements.daemon.register(info) {
                Ok(()) => announcements.names.push(fullname),
                Err(e) => {
                    report(format!("announcing {host}: {e}"));
                    daemon_failed = true;
                }
            }
        }
        if daemon_failed {
            // Start a fresh daemon and announce everything again.
            guard = None;
            previous.clear();
            previous_ip = None;
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        previous = names;
        previous_ip = Some(ip);
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ComposeRoute {
    name: String,
    service: String,
    target: u16,
}
#[derive(Debug)]
struct ComposeArgs {
    globals: Vec<String>,
    subcommand: String,
    rest: Vec<String>,
}
const COMPOSE_VALUES: &[&str] = &[
    "-f",
    "--file",
    "-p",
    "--project-name",
    "--profile",
    "--env-file",
    "--project-directory",
    "--ansi",
    "--progress",
    "--parallel",
];
fn compose_args(args: &[String]) -> Result<ComposeArgs> {
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        let flag = args[i].split('=').next().unwrap();
        if COMPOSE_VALUES.contains(&flag) && !args[i].contains('=') {
            i += 1;
            if i >= args.len() {
                bail!("{flag} needs a value");
            }
        }
        i += 1;
    }
    Ok(ComposeArgs {
        globals: args[..i].to_vec(),
        subcommand: args
            .get(i)
            .context("usage: compose [global flags] up|down [arguments]")?
            .clone(),
        rest: args[i + 1..].to_vec(),
    })
}
fn global_value(globals: &[String], flags: &[&str]) -> Option<String> {
    let mut out = None;
    let mut i = 0;
    while i < globals.len() {
        if let Some((f, v)) = globals[i].split_once('=') {
            if flags.contains(&f) {
                out = Some(v.into());
            }
        } else if flags.contains(&globals[i].as_str()) {
            i += 1;
            out = globals.get(i).cloned();
        }
        i += 1;
    }
    out
}
fn compose_command(globals: &[String], rest: &[&str]) -> Vec<String> {
    let mut a = vec!["compose".into()];
    a.extend_from_slice(globals);
    a.extend(rest.iter().map(|s| s.to_string()));
    a
}
fn compose_containers(globals: &[String]) -> Result<(BTreeSet<String>, BTreeMap<String, u16>)> {
    let output = command(
        "docker",
        &compose_command(globals, &["ps", "--format", "json"]),
    )?;
    let rows: Vec<Value> = if output.trim().is_empty() {
        Vec::new()
    } else if output.trim_start().starts_with('[') {
        serde_json::from_str(&output)?
    } else {
        output
            .lines()
            .filter(|s| !s.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?
    };
    let mut projects = BTreeSet::new();
    let mut ports = BTreeMap::new();
    for r in rows {
        if let Some(p) = r["Project"].as_str() {
            projects.insert(p.into());
        }
        let Some(service) = r["Service"].as_str() else {
            continue;
        };
        if r["State"].as_str().is_some_and(|s| s != "running") {
            continue;
        }
        for p in r["Publishers"].as_array().into_iter().flatten() {
            if p["Protocol"].as_str().is_some_and(|s| s != "tcp") {
                continue;
            }
            if let (Some(target), Some(published)) =
                (p["TargetPort"].as_u64(), p["PublishedPort"].as_u64())
            {
                if (1..=65535).contains(&target) && (1..=65535).contains(&published) {
                    ports
                        .entry(format!("{service}:{target}"))
                        .or_insert(published as u16);
                }
            }
        }
    }
    Ok((projects, ports))
}
fn non_http(port: u16) -> bool {
    [
        1433, 1521, 2181, 3306, 4222, 5432, 5672, 6379, 9042, 9092, 11211, 26257, 27017,
    ]
    .contains(&port)
}
fn compose_plan(
    mut model: Value,
    settings: &crate::state::ProxySettings,
) -> Result<(Value, Vec<ComposeRoute>)> {
    let project = crate::config::sanitize_for_hostname(
        model["name"]
            .as_str()
            .context("Compose resolved model has no project name")?,
    );
    if project.is_empty() {
        bail!("invalid Compose project name");
    }
    let mut routes = Vec::new();
    let mut seen = BTreeSet::new();
    let services = model["services"]
        .as_object_mut()
        .context("Compose model has no services")?;
    for (service, svc) in services {
        if svc["network_mode"] == "host" {
            continue;
        }
        let mut labels = svc.get("labels").cloned().unwrap_or(json!({}));
        // `porch.*` labels win over the Portless names they mirror.
        for key in ["enable", "name", "ports"] {
            if let Some(v) = labels.get(format!("porch.{key}")).cloned() {
                labels[format!("portless.{key}")] = v;
            }
        }
        if labels["portless.enable"]
            .as_str()
            .is_some_and(|s| matches!(s.trim(), "0" | "false"))
        {
            continue;
        }
        let label = crate::config::sanitize_for_hostname(
            labels["portless.name"].as_str().unwrap_or(service),
        );
        if label.is_empty() {
            bail!("{service} has invalid portless.name");
        }
        let selected = labels["portless.ports"]
            .as_str()
            .map(|s| {
                s.split(',')
                    .map(|p| {
                        p.trim()
                            .parse::<u16>()
                            .context("invalid portless.ports label")
                    })
                    .collect::<Result<BTreeSet<_>>>()
            })
            .transpose()?;
        if selected.as_ref().is_some_and(|p| p.contains(&0)) {
            bail!("portless.ports cannot contain zero");
        }
        let ports = svc["ports"].as_array().cloned().unwrap_or_default();
        if let Some(selected) = &selected {
            for target in selected {
                if !ports
                    .iter()
                    .any(|p| p["target"].as_u64() == Some(*target as u64))
                {
                    bail!("{service}: portless.ports lists unpublished port {target}");
                }
            }
        }
        let mut next = Vec::new();
        let mut targets = Vec::new();
        for mut p in ports {
            let target = p["target"]
                .as_u64()
                .filter(|p| (1..=65535).contains(p))
                .context("Compose published target is invalid")? as u16;
            if p["protocol"].as_str().is_some_and(|p| p != "tcp")
                || selected
                    .as_ref()
                    .map_or(non_http(target), |s| !s.contains(&target))
            {
                next.push(p);
                continue;
            }
            if targets.contains(&target) {
                continue;
            }
            targets.push(target);
            p.as_object_mut()
                .context("invalid Compose port entry")?
                .remove("published");
            p["host_ip"] = json!("127.0.0.1");
            next.push(p);
        }
        for (i, target) in targets.iter().enumerate() {
            let mut names = if targets.len() > 1 {
                vec![format!("{target}.{label}.{project}")]
            } else {
                Vec::new()
            };
            if i == 0 {
                names.insert(0, format!("{label}.{project}"));
            }
            for name in names {
                valid_hostname(&name)?;
                if !seen.insert(name.clone()) {
                    bail!("Compose services collide on {name}");
                }
                routes.push(ComposeRoute {
                    name,
                    service: service.clone(),
                    target: *target,
                });
            }
        }
        svc["ports"] = json!(next);
        if !targets.is_empty() {
            if !svc["environment"].is_object() {
                svc["environment"] = json!({});
            }
            if svc["environment"]
                .get("__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS")
                .is_none()
            {
                svc["environment"]["__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS"] = json!(format!(
                    ".{}",
                    if settings.lan {
                        "local"
                    } else {
                        settings
                            .tlds
                            .first()
                            .map(String::as_str)
                            .unwrap_or("localhost")
                    }
                ));
            }
        }
    }
    Ok((model, routes))
}
fn compose_paths(project: &str) -> (PathBuf, PathBuf) {
    use sha2::{Digest, Sha256};
    let key = format!("{:x}", Sha256::digest(project.as_bytes()));
    (
        crate::state::dir().join(format!("compose-{}.json", &key[..16])),
        crate::state::dir().join(format!("compose-{}.routes.json", &key[..16])),
    )
}
fn compose_conflict(
    state: &crate::state::State,
    project: &str,
    names: &[String],
    settings: &crate::state::ProxySettings,
) -> Option<String> {
    let old = state
        .compose_projects
        .get(project)
        .cloned()
        .unwrap_or_default();
    let incoming = names
        .iter()
        .flat_map(|name| route_hosts(name, settings).unwrap_or_default())
        .collect::<BTreeSet<_>>();
    state
        .routes
        .iter()
        .find(|existing| {
            let overlap = route_hosts(&existing.name, settings)
                .unwrap_or_default()
                .iter()
                .any(|h| incoming.contains(h));
            overlap
                && (!old.contains(&existing.name)
                    || existing.pid.is_some()
                    || state.owners.contains_key(&existing.name))
        })
        .map(|r| r.name.clone())
}
fn reconcile_compose(project: &str, globals: &[String], plan: &[ComposeRoute]) -> Result<()> {
    let (_, ports) = compose_containers(globals)?;
    let mut routes = Vec::new();
    for r in plan {
        if let Some(port) = ports.get(&format!("{}:{}", r.service, r.target)) {
            routes.push(crate::state::Route {
                name: r.name.clone(),
                target: format!("http://127.0.0.1:{port}"),
                pid: None,
            });
        }
    }
    // Polling mostly finds nothing new: don't rewrite (and fsync) the state.
    let current = crate::state::load()?;
    let old = current
        .compose_projects
        .get(project)
        .cloned()
        .unwrap_or_default();
    let names: Vec<String> = routes.iter().map(|r| r.name.clone()).collect();
    let mut existing: Vec<(&str, &str)> = current
        .routes
        .iter()
        .filter(|r| old.contains(&r.name))
        .map(|r| (r.name.as_str(), r.target.as_str()))
        .collect();
    let mut wanted: Vec<(&str, &str)> = routes
        .iter()
        .map(|r| (r.name.as_str(), r.target.as_str()))
        .collect();
    existing.sort();
    wanted.sort();
    if old == names && existing == wanted {
        return Ok(());
    }
    let settings = crate::state::proxy_settings()?;
    crate::state::transaction(|s| {
        if let Some(name) = compose_conflict(
            s,
            project,
            &routes.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
            &settings,
        ) {
            bail!("Compose route {name} is owned by another app or project");
        }
        let old = s.compose_projects.get(project).cloned().unwrap_or_default();
        s.routes.retain(|r| !old.contains(&r.name));
        s.compose_projects.insert(
            project.into(),
            routes.iter().map(|r| r.name.clone()).collect(),
        );
        s.routes.extend(routes);
        Ok(())
    })?;
    Ok(())
}
struct StdinSource(PathBuf);
impl Drop for StdinSource {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn compose_stdin(globals: &mut Vec<String>) -> Result<Option<StdinSource>> {
    let mut positions = Vec::new();
    let mut inline = Vec::new();
    let mut i = 0;
    while i < globals.len() {
        if matches!(globals[i].as_str(), "-f" | "--file") {
            if globals.get(i + 1).is_some_and(|s| s == "-") {
                positions.push(i + 1);
            }
            i += 1;
        } else if matches!(globals[i].as_str(), "--file=-" | "-f=-") {
            inline.push(i);
        }
        i += 1;
    }
    if positions.is_empty() && inline.is_empty() {
        return Ok(None);
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    if input.trim().is_empty() {
        bail!("Compose stdin is empty");
    }
    let path = crate::state::dir().join(format!("compose-stdin-{}.yaml", std::process::id()));
    write_state(&path, input)?;
    for i in positions {
        globals[i] = path.to_string_lossy().into_owned();
    }
    for i in inline {
        globals[i] = format!("--file={}", path.display());
    }
    if global_value(globals, &["--project-directory"]).is_none() {
        globals.extend([
            "--project-directory".into(),
            std::env::current_dir()?.to_string_lossy().into_owned(),
        ]);
    }
    Ok(Some(StdinSource(path)))
}
async fn passthrough_compose(args: &[String]) -> Result<()> {
    let mut command = tokio::process::Command::new("docker");
    command.arg("compose").args(args);
    crate::process::configure(&mut command);
    let mut child = command.spawn().context("starting docker compose")?;
    let _tree = crate::process::TreeGuard::new(&child)?;
    tokio::select! {status=child.wait()=>require_status(status?),_=tokio::signal::ctrl_c()=>{stop_compose(&mut child,2).await;Err(ExternalExit(130).into())},_=terminate_signal()=>{stop_compose(&mut child,15).await;Err(ExternalExit(143).into())}}
}
async fn compose(args: &[String]) -> Result<()> {
    let mut parsed = compose_args(args)?;
    if !matches!(parsed.subcommand.as_str(), "up" | "down") {
        return passthrough_compose(args).await;
    }
    let _stdin_source = compose_stdin(&mut parsed.globals)?;
    let source: Value = serde_json::from_str(&command(
        "docker",
        &compose_command(&parsed.globals, &["config", "--format", "json"]),
    )?)?;
    let project = source["name"]
        .as_str()
        .context("Compose project missing")?
        .to_owned();
    let settings = crate::state::proxy_settings()?;
    let (mut model, plan) = compose_plan(source, &settings)?;
    let (model_path, plan_path) = compose_paths(&project);
    let globals = vec!["-p".into(), project.clone()];
    if parsed.subcommand == "down" {
        let mut a = vec!["compose".into()];
        a.extend_from_slice(&parsed.globals);
        a.push(parsed.subcommand.clone());
        a.extend_from_slice(&parsed.rest);
        let status = Command::new("docker").args(a).status()?;
        require_status(status)?;
        let plan = fs::read(&plan_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<ComposeRoute>>(&b).ok())
            .unwrap_or(plan);
        reconcile_compose(&project, &globals, &plan)?;
        // The project is down: release its ports and delete the resolved
        // model, which can hold secrets from .env files.
        forget_compose_project(&project)?;
        let _ = request_hosts_sync().await;
        return Ok(());
    }
    if let Some(name) = compose_conflict(
        &crate::state::load()?,
        &project,
        &plan.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
        &settings,
    ) {
        bail!("Compose route {name} is owned by another app or project");
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(crate::state::dir().join("compose.lock"))?;
    lock.lock_exclusive()?;
    prune_compose_projects();
    let ports_path = crate::state::dir().join("compose-ports.json");
    let mut stored: BTreeMap<String, BTreeMap<String, u16>> = fs::read(&ports_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let previous = stored.get(&project).cloned().unwrap_or_default();
    let reserved = stored
        .iter()
        .filter(|(p, _)| *p != &project)
        .flat_map(|(_, m)| m.values().copied())
        .collect::<BTreeSet<_>>();
    let ours = compose_containers(&globals)?
        .1
        .into_values()
        .collect::<BTreeSet<_>>();
    let mut assigned = BTreeMap::new();
    let mut reservations = Vec::new();
    for r in &plan {
        let key = format!("{}:{}", r.service, r.target);
        if assigned.contains_key(&key) {
            continue;
        }
        let old = previous
            .get(&key)
            .copied()
            .filter(|p| !reserved.contains(p));
        let mut selected = None;
        for p in old.into_iter().chain(5000..=5999) {
            if reserved.contains(&p) || assigned.values().any(|a| *a == p) {
                continue;
            }
            if ours.contains(&p) && old == Some(p) {
                selected = Some(p);
                break;
            }
            if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", p)) {
                selected = Some(p);
                reservations.push(listener);
                break;
            }
        }
        assigned.insert(
            key,
            selected.context("no free Compose host port in 5000-5999")?,
        );
    }
    for (service, svc) in model["services"].as_object_mut().unwrap() {
        for p in svc["ports"].as_array_mut().into_iter().flatten() {
            if let Some(port) = p["target"]
                .as_u64()
                .and_then(|t| assigned.get(&format!("{service}:{t}")))
            {
                p["published"] = json!(port.to_string());
            }
        }
    }
    stored.insert(project.clone(), assigned);
    write_state(&ports_path, serde_json::to_vec_pretty(&stored)?)?;
    write_state(&model_path, serde_json::to_vec_pretty(&model)?)?;
    write_state(&plan_path, serde_json::to_vec_pretty(&plan)?)?;
    let cwd = std::env::current_dir()?;
    let dir = global_value(&parsed.globals, &["--project-directory"])
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            global_value(&parsed.globals, &["-f", "--file"])
                .or_else(|| {
                    std::env::var("COMPOSE_FILE").ok().and_then(|s| {
                        s.split(if cfg!(windows) { ';' } else { ':' })
                            .next()
                            .map(str::to_owned)
                    })
                })
                .map(|f| cwd.join(f).parent().unwrap_or(&cwd).to_owned())
                .unwrap_or(cwd)
        });
    let mut argv = vec![
        "compose".into(),
        "-p".into(),
        project.clone(),
        "--project-directory".into(),
        dir.to_string_lossy().into_owned(),
        "-f".into(),
        model_path.to_string_lossy().into_owned(),
    ];
    let mut i = 0;
    while i < parsed.globals.len() {
        let flag = parsed.globals[i].split('=').next().unwrap();
        if [
            "-p",
            "--project-name",
            "-f",
            "--file",
            "--project-directory",
        ]
        .contains(&flag)
        {
            if !parsed.globals[i].contains('=') {
                i += 1;
            }
        } else {
            argv.push(parsed.globals[i].clone());
        }
        i += 1;
    }
    argv.push("up".into());
    argv.extend_from_slice(&parsed.rest);
    remember_compose_dir(&project, &dir);
    drop(reservations);
    drop(lock);
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(argv).kill_on_drop(true);
    crate::process::configure(&mut cmd);
    let mut child = cmd.spawn()?;
    let _tree_guard = crate::process::TreeGuard::new(&child)?;
    let detached = parsed
        .rest
        .iter()
        .any(|a| matches!(a.as_str(), "-d" | "--detach" | "--wait" | "--wait=true"));
    if detached {
        let status = child.wait().await?;
        require_status(status)?;
        reconcile_compose(&project, &globals, &plan)?;
        let _ = request_hosts_sync().await;
        for r in &plan {
            println!("{}", settings.url(&r.name));
        }
        return Ok(());
    }

    // Follow containers as they start and stop. `docker compose ps` runs on
    // a blocking thread, so a slow Docker never delays Ctrl-C.
    let reconcile = {
        let (project, globals, plan) = (project.clone(), globals.clone(), plan.clone());
        move || {
            let (project, globals, plan) = (project.clone(), globals.clone(), plan.clone());
            tokio::task::spawn_blocking(move || reconcile_compose(&project, &globals, &plan))
        }
    };
    let finish = |reconcile: tokio::task::JoinHandle<Result<()>>| async move {
        reconcile.await.map_err(anyhow::Error::from).and_then(|r| r)
    };
    let mut poll = tokio::time::interval(Duration::from_secs(2));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending: Option<tokio::task::JoinHandle<Result<()>>> = None;
    loop {
        tokio::select! {
            status=child.wait()=>{
                let status=status?;
                if let Some(p)=pending.take(){let _=p.await;}
                finish(reconcile()).await?;
                let _=request_hosts_sync().await;
                require_status(status)?;
                return Ok(());
            },
            _=poll.tick(), if pending.is_none()=>{pending=Some(reconcile());},
            result=async{pending.as_mut().expect("guarded by is_some").await}, if pending.is_some()=>{
                pending=None;
                match result.map_err(anyhow::Error::from).and_then(|r| r){
                    Ok(())=>{},
                    Err(e)=>eprintln!("Compose route update: {e}"),
                }
            },
            _=tokio::signal::ctrl_c()=>{stop_compose(&mut child,2).await;if let Some(p)=pending.take(){p.abort();}let _=finish(reconcile()).await;return Err(ExternalExit(130).into());},
            _=terminate_signal()=>{stop_compose(&mut child,15).await;if let Some(p)=pending.take(){p.abort();}let _=finish(reconcile()).await;return Err(ExternalExit(143).into());},
        }
    }
}
/// Project directories by Compose project, so bookkeeping for a project whose
/// folder is gone can be dropped.
fn compose_dirs_path() -> PathBuf {
    crate::state::dir().join("compose-projects.json")
}
fn read_json_map<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
fn remember_compose_dir(project: &str, dir: &Path) {
    let path = compose_dirs_path();
    let mut dirs: BTreeMap<String, PathBuf> = read_json_map(&path);
    dirs.insert(project.into(), dir.to_path_buf());
    if let Ok(bytes) = serde_json::to_vec_pretty(&dirs) {
        let _ = write_state(&path, bytes);
    }
}
/// Drop a project's reserved ports, resolved model, plan and directory record.
fn forget_compose_project(project: &str) -> Result<()> {
    let ports_path = crate::state::dir().join("compose-ports.json");
    let mut stored: BTreeMap<String, BTreeMap<String, u16>> = read_json_map(&ports_path);
    if stored.remove(project).is_some() {
        write_state(&ports_path, serde_json::to_vec_pretty(&stored)?)?;
    }
    let dirs_path = compose_dirs_path();
    let mut dirs: BTreeMap<String, PathBuf> = read_json_map(&dirs_path);
    if dirs.remove(project).is_some() {
        write_state(&dirs_path, serde_json::to_vec_pretty(&dirs)?)?;
    }
    let (model, plan) = compose_paths(project);
    for path in [model, plan] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
/// Forget projects whose directory no longer exists and that have no routes.
fn prune_compose_projects() {
    let dirs: BTreeMap<String, PathBuf> = read_json_map(&compose_dirs_path());
    let state = crate::state::load().unwrap_or_default();
    for (project, dir) in dirs {
        let routed = state
            .compose_projects
            .get(&project)
            .is_some_and(|names| !names.is_empty());
        if !dir.exists() && !routed {
            let _ = forget_compose_project(&project);
        }
    }
}

#[derive(Debug)]
pub struct ExternalExit(pub i32);
impl std::fmt::Display for ExternalExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "external command exited with {}", self.0)
    }
}
impl std::error::Error for ExternalExit {}
fn require_status(status: std::process::ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    #[cfg(unix)]
    let code = {
        use std::os::unix::process::ExitStatusExt;
        status.code().unwrap_or(128 + status.signal().unwrap_or(1))
    };
    #[cfg(not(unix))]
    let code = status.code().unwrap_or(1);
    Err(ExternalExit(code).into())
}
async fn stop_compose(child: &mut tokio::process::Child, signal: i32) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), signal);
        }
    }
    // Compose runs in its own console process group under this console.
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        if signal == 9 {
            crate::shutdown::kill_tree(pid);
        } else {
            crate::shutdown::interrupt_group(pid);
        }
    }
    // Compose stops each container gracefully, waiting up to its stop timeout
    // (10 s by default) before killing it: give it longer than that, and pass
    // a second Ctrl-C on, which Compose takes as "force".
    let waited = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                _ = child.wait() => break,
                _ = tokio::signal::ctrl_c() => {
                    #[cfg(unix)]
                    if let Some(pid) = child.id() {
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGINT);
                        }
                    }
                    #[cfg(windows)]
                    if let Some(pid) = child.id() {
                        crate::shutdown::interrupt_group(pid);
                    }
                }
            }
        }
    })
    .await;
    if waited.is_err() {
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = child.kill().await;
    }
}
async fn terminate_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    }
    #[cfg(windows)]
    {
        if crate::shutdown::listen().is_ok() {
            crate::shutdown::requested().await;
        } else {
            std::future::pending::<()>().await;
        }
    }
    #[cfg(not(any(unix, windows)))]
    std::future::pending::<()>().await;
}

pub async fn run(args: Vec<String>) -> Result<()> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    if rest.iter().any(|s| s == "--help" || s == "-h") {
        println!("{}",match sub {
        "hosts"=>"porch hosts sync|clean\nManage only the Porch block in the system hosts file.",
        "service"=>"porch service install|status|uninstall\nInstall options: --port/-p N --https --no-tls --unprivileged --lan --ip IP --tld DOMAIN (repeatable) --wildcard --cert FILE --key FILE --state-dir DIR",
        "compose"=>"porch compose [Docker Compose global flags] up|down [arguments]\nHTTP publications use stable loopback ports; database and UDP ports are retained.",
        "share"=>"porch share tailscale|funnel|ngrok|cloudflare PORT\nOwn the sharing session until Ctrl-C.",_=>"porch doctor | trust | lan"
    });
        return Ok(());
    }
    match sub {
        "hosts" => hosts(rest.first().map(String::as_str).unwrap_or("sync")),
        "service" => {
            service_command(
                rest.first().map(String::as_str).unwrap_or("status"),
                &rest[rest.len().min(1)..],
            )
            .await
        }
        "trust" => trust(rest.iter().any(|a| a == "--system")),
        "compose" => compose(rest).await,
        "share" => {
            let provider = rest
                .first()
                .context("usage: share tailscale|funnel|ngrok|cloudflare PORT")?
                .clone();
            let port = rest.get(1).context("share port missing")?.parse()?;
            let session = start_shares(port, &[provider]).await?;
            for (kind, url) in &session.urls {
                println!("{kind}={url}");
            }
            tokio::signal::ctrl_c().await?;
            drop(session);
            Ok(())
        }
        "lan" => {
            let mut settings = crate::state::proxy_settings()?;
            settings.lan = true;
            tokio::select! {result=lan_watch(settings)=>result,_=tokio::signal::ctrl_c()=>Ok(())}
        }
        "doctor" => doctor().await,
        _ => bail!("expected hosts, service, trust, share, lan, compose, or doctor"),
    }
}
/// Problems the last `doctor` report marked with ✗, for its exit code.
pub static DOCTOR_FAILURES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
async fn doctor() -> Result<()> {
    use crate::style::{self, Stream::Stdout};
    let good = |label: &str, detail: &str| {
        println!(
            "  {} {label}  {}",
            style::green(Stdout, "✓"),
            style::dim(Stdout, detail)
        )
    };
    let bad = |label: &str, detail: &str| {
        DOCTOR_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        println!("  {} {label}  {detail}", style::red(Stdout, "✗"))
    };
    let info = |label: &str, detail: &str| {
        println!(
            "  {} {label}  {}",
            style::dim(Stdout, "·"),
            style::dim(Stdout, detail)
        )
    };
    let section = |title: &str| println!("\n{}", style::bold(Stdout, title));
    let settings = crate::state::proxy_settings()?;
    let state = crate::state::load()?;
    println!(
        "{}",
        style::bold(Stdout, &format!("Porch {}", env!("CARGO_PKG_VERSION")))
    );
    section("Settings");
    info("state", &crate::state::dir().display().to_string());
    info(
        "proxy",
        &format!(
            "{} · {} · port {}{}",
            settings.url("<app>"),
            if settings.tls { "HTTPS" } else { "HTTP" },
            settings.port,
            if settings.unprivileged {
                " · no sudo"
            } else {
                " · uses sudo"
            }
        ),
    );
    info("domains", &settings.tlds.join(", "));
    if settings.lan {
        info(
            "LAN mode",
            settings.lan_ip.as_deref().unwrap_or("automatic address"),
        );
    }
    section("Proxy");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?;
    let identity = fs::read(crate::state::dir().join("proxy.identity"))
        .ok()
        .and_then(|v| serde_json::from_slice::<crate::state::ProcessIdentity>(&v).ok());
    let ready = fs::read_to_string(crate::state::dir().join("proxy.ready")).unwrap_or_default();
    let owned_ready = identity.as_ref().is_some_and(|i| {
        crate::state::matches(i) && ready.trim().parse::<u32>().ok() == Some(i.pid)
    });
    let token =
        fs::read_to_string(crate::state::dir().join("proxy.hosts-sync-token")).unwrap_or_default();
    let challenge = "ab".repeat(32);
    let mut stopped = 0;
    let mut foreign = false;
    for address in ["127.0.0.1", "[::1]"] {
        let label = format!("listener {address}");
        let response = client
            .head(format!(
                "http://{address}:{}/_porch/hosts-sync",
                settings.port
            ))
            .header("host", "localhost")
            .header("x-porch-hosts-sync-challenge", &challenge)
            .send()
            .await;
        match response {
            Ok(response) => {
                let proof = response
                    .headers()
                    .get("x-porch-hosts-sync-proof")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let authenticated = verify_hosts_sync(token.trim(), &challenge, proof);
                let disabled = settings.unprivileged
                    && response.status() == reqwest::StatusCode::CONFLICT
                    && response.headers().contains_key("x-porch");
                if owned_ready && (authenticated || disabled) {
                    good(&label, "Porch is ready (identity verified)");
                } else {
                    let who = crate::ports::describe_owner(settings.port);
                    let detail = format!(
                        "foreign/unverified listener ({who}); port collision or stale daemon state"
                    );
                    if owned_ready {
                        bad(&label, &detail);
                    } else {
                        // Porch's proxy is stopped and another program holds
                        // its port: the busy-port check below reports (and
                        // with --fix resolves) that, so don't count it twice.
                        println!("  {} {label}  {detail}", style::yellow(Stdout, "!"));
                    }
                }
            }
            Err(e) if e.is_connect() && !owned_ready => stopped += 1,
            Err(e) if e.is_connect() => bad(
                &label,
                "the proxy should be here but nothing answers; run `porch proxy start`",
            ),
            Err(e) if owned_ready => bad(
                &label,
                &format!("unavailable ({e}); run `porch proxy start`"),
            ),
            Err(_) => {
                foreign = true;
                println!(
                    "  {} {label}  another program answers on this port (see Issues)",
                    style::yellow(Stdout, "!")
                );
            }
        }
    }
    if foreign {
    } else if stopped == 2
        && state
            .routes
            .iter()
            .any(|r| r.pid.is_some() && crate::state::route_alive(&state, r))
    {
        bad(
            "stopped",
            "apps are running but their URLs won't answer until the proxy starts; run `porch proxy start`",
        );
    } else if stopped == 2 {
        info(
            "stopped",
            "the proxy starts with your next app, or run `porch proxy start`",
        );
    } else if stopped == 1 {
        info(
            "one listener",
            "only one loopback address answers (IPv6 may be off)",
        );
    }
    section("Routes");
    if state.routes.is_empty() {
        info("none", "start an app with `porch run <command>`");
    }
    for r in state.routes {
        let owner = state.owners.get(&r.name).map(crate::state::matches);
        let host = settings.hostname(&r.name);
        let resolves = tokio::net::lookup_host((host.as_str(), settings.port))
            .await
            .is_ok_and(|mut a| a.next().is_some());
        let label = settings.url(&r.name);
        let detail = format!(
            "-> {}{}",
            r.target,
            if owner == Some(false) {
                " (stale launcher)"
            } else {
                ""
            }
        );
        let target = r.target.clone();
        let listening =
            tokio::task::spawn_blocking(move || crate::health::target_listening(&target))
                .await
                .unwrap_or(false);
        if !resolves && (host == "localhost" || host.ends_with(".localhost")) {
            // Browsers resolve *.localhost themselves, so the URL works there;
            // only tools that ask the system resolver (curl, older macOS) miss it.
            println!(
                "  {} {label}  {}",
                style::yellow(Stdout, "!"),
                style::dim(
                    Stdout,
                    &format!("{detail} · browsers open it; for curl and other tools on this computer, run `sudo porch hosts sync`")
                )
            );
        } else if !resolves {
            bad(
                &label,
                &format!("{host} does not resolve on this computer; run `sudo porch hosts sync`"),
            );
        } else if !listening {
            println!(
                "  {} {label}  {} {}",
                style::yellow(Stdout, "!"),
                style::dim(Stdout, &detail),
                style::yellow(Stdout, "(not listening)")
            );
        } else if owner == Some(false) {
            bad(&label, &detail);
        } else {
            good(&label, &detail);
        }
    }
    section("HTTPS");
    if !settings.tls {
        info(
            "off",
            "turn it on with `porch setup` or `porch proxy start --https`",
        );
    } else if settings.cert.is_some() {
        info("your certificate", "using --cert/--key; trust is up to you");
    } else {
        match ca_trust() {
            CaTrust::NoCa => bad(
                "local CA",
                "missing; it is created when the proxy starts with --https",
            ),
            CaTrust::Trusted(scope) => good(
                "local CA",
                if scope == "user" {
                    "trusted for this user (checked in the trust store)"
                } else {
                    "trusted system-wide (checked in the trust store)"
                },
            ),
            CaTrust::Unverified(_) => info(
                "local CA",
                "trust was installed, but the trust store couldn't be checked here",
            ),
            CaTrust::NotInstalled => bad("local CA", "not trusted yet; run `porch trust`"),
            CaTrust::Stale => bad(
                "local CA",
                "the trusted CA is an older one; run `porch trust` to trust the current CA",
            ),
            CaTrust::Removed => bad(
                "local CA",
                "its trust was removed from the trust store; run `porch trust` again",
            ),
        }
        if let Ok(pem) = fs::read_to_string(crate::state::dir().join("ca.pem")) {
            if !crate::tls::ca_server_auth_only(&pem) {
                info("CA purpose", crate::tls::LEGACY_CA_NOTICE);
            }
        }
        if trust_record()
            .ok()
            .flatten()
            .is_some_and(|r| r.platform == "macos" && !r.tls_only)
        {
            info(
                "trust policy",
                "macOS trusts the CA for every purpose (set by Porch 0.5.2 or earlier); run `porch trust` again (`porch trust --system` for system-wide trust) to limit it to TLS",
            );
        }
    }
    if let Some(warning) = service_warning() {
        section("Start at login");
        bad("startup service", &warning);
    }
    section("Tools");
    for (tool, flag, why) in [
        ("node", "--version", "Turborepo and Node apps"),
        ("docker", "--version", "porch compose"),
        ("tailscale", "version", "--tailscale and --funnel"),
        ("ngrok", "version", "--ngrok"),
        ("cloudflared", "--version", "--cloudflare"),
    ] {
        match exec(tool, &[flag]) {
            Ok(out) => good(tool, out.lines().next().unwrap_or("").trim()),
            Err(_) => info(tool, &format!("not installed (only needed for {why})")),
        }
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn oversized_command_output_is_an_error() {
        let n = (COMMAND_OUTPUT_LIMIT + 1).to_string();
        let err = exec("head", &["-c", &n, "/dev/zero"]).unwrap_err();
        assert!(err.to_string().contains("more than"), "{err:#}");
        // Far past the limit the writer is cut off by SIGPIPE, which must not
        // turn the oversize report into an exit-status failure.
        let err = exec(
            "head",
            &["-c", &(4 * COMMAND_OUTPUT_LIMIT).to_string(), "/dev/zero"],
        )
        .unwrap_err();
        assert!(err.to_string().contains("more than"), "{err:#}");
        assert_eq!(exec("head", &["-c", "3", "/dev/zero"]).unwrap().len(), 3);
    }
    #[test]
    fn ca_fingerprint_matches_openssl_format() {
        // Pinned against `openssl x509 -noout -fingerprint -sha1` (colons stripped), the
        // format trust.json records used through 0.3.x.
        let ca = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/porch-ca.pem"
        ));
        assert_eq!(
            ca_fingerprint(ca).unwrap(),
            "8B7699335C1F0EDA581E025A5F45400A8BEF9354"
        );
        verify_porch_ca(ca).unwrap();
        let other = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/other-ca.pem"
        ));
        assert!(verify_porch_ca(other).is_err());
        let dir = std::env::temp_dir().join(format!("porch-ca-garbage-{}", std::process::id()));
        fs::write(&dir, "not a certificate").unwrap();
        assert!(ca_fingerprint(&dir).is_err());
        assert!(verify_porch_ca(&dir).is_err());
        let _ = fs::remove_file(&dir);
    }
    #[test]
    fn nss_listing_matches_exact_nickname() {
        let listing = "\nCertificate Nickname                                         Trust Attributes\n                                                             SSL,S/MIME,JAR/XPI\n\nPorch Local CA 8B7699335C1F                                  C,,\nPorch Local CA 8B7699335C1F0 other                           CT,C,C\n";
        assert!(nss_listing_has(listing, "Porch Local CA 8B7699335C1F"));
        assert!(!nss_listing_has(listing, "Porch Local CA 8B7699335C"));
        assert!(!nss_listing_has(listing, "Porch Local CA ABCDEF012345"));
        assert!(!nss_listing_has(
            "\nCertificate Nickname                                         Trust Attributes\n                                                             SSL,S/MIME,JAR/XPI\n\n",
            "Porch Local CA 8B7699335C1F"
        ));
    }
    #[test]
    fn hosts_preserve_unmanaged() {
        assert_eq!(
            hosts_content(
                "127.0.0.1 localhost\n# porch-start\nold\n# porch-end\n# custom\n",
                &[]
            )
            .unwrap(),
            "127.0.0.1 localhost\n# custom\n"
        );
        assert!(hosts_content("# porch-start\n", &[]).is_err());
        assert!(valid_hostname("evil\n127.0.0.1 injected").is_err());
    }
    #[test]
    fn proof_is_authenticated() {
        let t = "a".repeat(64);
        let c = "b".repeat(64);
        let p = hosts_sync_proof(&t, &c).unwrap();
        assert_eq!(
            p,
            "f6cbb37b326d36f2f27d294ac3bb46a6aac29c1c9936b985576041bfb338ae70"
        );
        assert!(verify_hosts_sync(&t, &c, &p));
        assert!(!verify_hosts_sync(&"c".repeat(64), &c, &p));
        assert!(hosts_sync_proof("invalid", &c).is_none());
    }
    #[test]
    fn compose_global_flags() {
        let p = compose_args(
            &[
                "-f",
                "dev.yml",
                "--profile=dev",
                "-p",
                "shop",
                "up",
                "-d",
                "api",
            ]
            .map(str::to_string),
        )
        .unwrap();
        assert_eq!(p.subcommand, "up");
        assert_eq!(global_value(&p.globals, &["-p"]), Some("shop".into()));
        assert_eq!(p.rest, ["-d", "api"]);
    }
    #[test]
    fn compose_plan_routes_http_and_respects_labels() {
        let source = json!({"name":"my-shop","services":{"web":{"ports":[{"target":3000,"published":"3000"},{"target":3000,"published":"3001"},{"target":8080}]},"db":{"ports":[{"target":5432,"published":"5432"}]},"other":{"labels":{"portless.enable":"false"},"ports":[{"target":80,"published":"80"}]}}});
        let (model, routes) =
            compose_plan(source, &crate::state::ProxySettings::default()).unwrap();
        assert_eq!(routes.len(), 3);
        assert_eq!(routes[0].name, "web.my-shop");
        assert_eq!(
            model["services"]["web"]["ports"].as_array().unwrap().len(),
            2
        );
        assert!(model["services"]["web"]["ports"][0]
            .get("published")
            .is_none());
        assert_eq!(model["services"]["db"]["ports"][0]["published"], "5432");
        // porch.* labels work like (and win over) portless.* ones.
        let source = json!({"name":"shop","services":{"web":{"labels":{"porch.name":"store","portless.name":"old"},"ports":[{"target":3000}]},"db":{"labels":{"porch.enable":"false"},"ports":[{"target":80}]}}});
        let (_, routes) = compose_plan(source, &crate::state::ProxySettings::default()).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].name, "store.shop");
    }
    #[test]
    fn tunnel_output_waits_for_cloudflare_connection() {
        assert!(extract_tunnel_url("cloudflare", "https://abc.trycloudflare.com").is_none());
        assert_eq!(
            extract_tunnel_url(
                "cloudflare",
                "https://abc.trycloudflare.com\nRegistered tunnel connection"
            ),
            Some("https://abc.trycloudflare.com".into())
        );
        assert_eq!(
            extract_tunnel_url("ngrok", "started tunnel url=https://abc.ngrok.app"),
            Some("https://abc.ngrok.app".into())
        );
        assert!(extract_tunnel_url("ngrok", "documentation https://ngrok.com").is_none());
    }
    #[test]
    fn linux_trust_layouts() {
        assert_eq!(
            linux_ca_layout("ID=arch"),
            (
                "/etc/ca-certificates/trust-source/anchors",
                "update-ca-trust"
            )
        );
        assert_eq!(
            linux_ca_layout("ID=ubuntu"),
            ("/usr/local/share/ca-certificates", "update-ca-certificates")
        );
        assert_eq!(
            linux_ca_layout("ID=fedora"),
            ("/etc/pki/ca-trust/source/anchors", "update-ca-trust")
        );
        assert_eq!(
            linux_ca_layout("ID=opensuse"),
            ("/etc/pki/trust/anchors", "update-ca-certificates")
        );
    }
    #[test]
    fn keychain_listings_match_exact_fingerprints() {
        let listing = "SHA-256 hash: 00\nSHA-1 hash: AAAA\n-----BEGIN CERTIFICATE-----\nb25l\n-----END CERTIFICATE-----\nSHA-1 hash: BBBB\n-----BEGIN CERTIFICATE-----\ndHdv\n-----END CERTIFICATE-----\n";
        assert!(listing_has_sha1(listing, "aaaa"));
        assert!(!listing_has_sha1(listing, "AAA"));
        assert_eq!(
            pem_with_sha1(listing, "BBBB").unwrap(),
            "-----BEGIN CERTIFICATE-----\ndHdv\n-----END CERTIFICATE-----\n"
        );
        assert!(pem_with_sha1(listing, "CCCC").is_none());
    }
    #[test]
    fn hosts_rewrite_goes_through_the_locked_handle_and_never_truncates_first() {
        let path = std::env::temp_dir().join(format!("porch-hosts-rewrite-{}", std::process::id()));
        let long =
            "127.0.0.1 localhost\n# porch-start\n127.0.0.1 a.test\n::1 a.test\n# porch-end\n";
        fs::write(&path, long).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        rewrite_locked(&mut file, |old| hosts_content(old, &[])).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "127.0.0.1 localhost\n");
        // Unchanged content is never written: a read-only handle is enough.
        let mut read_only = fs::File::open(&path).unwrap();
        rewrite_locked(&mut read_only, |old| hosts_content(old, &[])).unwrap();
        // A grown file is written whole.
        rewrite_locked(&mut file, |old| hosts_content(old, &["b.test".into()])).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "127.0.0.1 localhost\n# porch-start\n127.0.0.1 b.test\n::1 b.test\n# porch-end\n"
        );
        let _ = fs::remove_file(path);
    }
    #[test]
    fn startup_service_runs_as_the_user_wherever_it_can() {
        let settings = |port: u16| crate::state::ProxySettings {
            port,
            ..Default::default()
        };
        let yes = |_: u16| true;
        let no = |_: u16| false;
        use ServiceScope::*;
        // High ports never need privileges, anywhere.
        for platform in ["macos", "linux", "windows"] {
            assert_eq!(
                service_scope(platform, &settings(7462), false, false, no).unwrap(),
                User
            );
        }
        // Low ports this user may bind: a login item too.
        assert_eq!(
            service_scope("macos", &settings(80), false, false, yes).unwrap(),
            User
        );
        assert_eq!(
            service_scope("linux", &settings(80), false, false, yes).unwrap(),
            User
        );
        assert_eq!(
            service_scope("windows", &settings(443), false, false, no).unwrap(),
            User
        );
        // Only Linux without unprivileged low ports needs a system unit.
        assert_eq!(
            service_scope("linux", &settings(80), false, false, no).unwrap(),
            System
        );
        assert_eq!(
            service_scope("linux", &settings(80), true, true, no).unwrap(),
            System
        );
        let mut https = settings(8443);
        https.tls = true;
        https.redirect_port = Some(80);
        assert_eq!(
            service_scope("linux", &https, false, false, no).unwrap(),
            System
        );
        assert!(service_scope("macos", &settings(80), false, false, no).is_err());
        let mut bound = settings(80);
        bound.bind = Some("127.0.0.1".into());
        assert!(service_scope("macos", &bound, false, false, yes).is_err());
        assert!(service_scope("macos", &settings(80), false, true, yes).is_err());
        assert!(service_scope("linux", &settings(7462), false, true, no).is_err());
        // Under sudo, nothing but the Linux system unit is installed.
        assert!(service_scope("macos", &settings(7462), true, false, yes).is_err());
        assert!(service_scope("linux", &settings(7462), true, false, yes).is_err());
    }
    #[test]
    fn linux_system_unit_never_runs_as_root() {
        let body = render_service(
            "linux",
            ServiceScope::System,
            Path::new("/usr/local/lib/porch/porch"),
            Path::new("/home/me/.porch"),
            &settings_args(&crate::state::ProxySettings {
                port: 80,
                unprivileged: false,
                ..Default::default()
            }),
            (1000, 1000),
            "",
        )
        .unwrap();
        for line in [
            "User=1000",
            "Group=1000",
            "AmbientCapabilities=CAP_NET_BIND_SERVICE",
            "CapabilityBoundingSet=CAP_NET_BIND_SERVICE",
            "NoNewPrivileges=yes",
            "ExecStart=\"/usr/local/lib/porch/porch\" \"proxy\"",
            "WantedBy=multi-user.target",
        ] {
            assert!(body.contains(line), "{line}: {body}");
        }
        let user = render_service(
            "linux",
            ServiceScope::User,
            Path::new("/home/me/bin/porch"),
            Path::new("/home/me/.porch"),
            &[],
            (1000, 1000),
            "",
        )
        .unwrap();
        assert!(!user.contains("User=") && user.contains("WantedBy=default.target"));
    }
    #[test]
    fn service_rendering_options_and_escaping() {
        let s = crate::state::ProxySettings {
            port: 8443,
            tlds: vec!["test".into(), "localhost".into()],
            wildcard: true,
            lan: true,
            ..Default::default()
        };
        let a = settings_args(&s);
        assert!(a.contains(&"--wildcard".into()));
        assert!(a.contains(&"--skip-trust".into()));
        assert_eq!(a.iter().filter(|a| *a == "--tld").count(), 2);
        assert_eq!(xml("a&<b>"), "a&amp;&lt;b&gt;");
        assert!(systemd_quote("bad\n").is_err());
    }
}
