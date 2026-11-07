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
fn write_state(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes.as_ref())?;
    file.sync_all()?;
    crate::state::inherit_owner(path)?;
    Ok(())
}
fn command(program: &str, args: &[String]) -> Result<String> {
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
                .take(32 * 1024 * 1024)
                .read_to_end(&mut data)
                .map(|_| data)
        })
    };
    let stdout = read(Box::new(stdout));
    let stderr = read(Box::new(stderr));
    let timeout = env("COMMAND_TIMEOUT_MS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000u64);
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout);
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
            bail!("{program} timed out after {timeout}ms");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let out = stdout
        .join()
        .map_err(|_| anyhow::anyhow!("command stdout reader failed"))??;
    let err = stderr
        .join()
        .map_err(|_| anyhow::anyhow!("command stderr reader failed"))??;
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
    let original = fs::read_to_string(&path)?;
    let updated = hosts_content(&original, &names)?;
    if original != updated {
        file.set_len(0)?;
        file.write_all(updated.as_bytes())?;
        file.sync_all()?;
    }
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
fn constant_eq(a: &str, b: &str) -> bool {
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
    let mut builder = reqwest::Client::builder()
        .no_proxy()
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
#[derive(Serialize, Deserialize)]
struct ServiceRecord {
    platform: String,
    path: PathBuf,
    state_dir: PathBuf,
    settings: crate::state::ProxySettings,
}
fn service_platform() -> String {
    std::env::var("PORCH_SERVICE_PLATFORM").unwrap_or_else(|_| std::env::consts::OS.into())
}
fn service_path(platform: &str) -> PathBuf {
    if let Some(p) = std::env::var_os("PORCH_SERVICE_FILE") {
        return p.into();
    }
    match platform {
        "macos" => "/Library/LaunchDaemons/dev.porch.proxy.plist".into(),
        "linux" => "/etc/systemd/system/porch.service".into(),
        _ => crate::state::dir().join("porch-service.xml"),
    }
}
fn service(action: &str, args: &[String]) -> Result<()> {
    let record_path = crate::state::dir().join("service.json");
    let platform = service_platform();
    let path = service_path(&platform);
    let exe = std::env::current_exe()?;
    let p = path.to_string_lossy();
    match action {
        "status" => {
            if let Ok(raw) = fs::read_to_string(&record_path) {
                let record: ServiceRecord = serde_json::from_str(&raw)?;
                println!("installed: {}\nstate: {}\nHTTPS: {}\nport: {}\nTLDs: {}\nLAN: {}\nwildcard: {}",record.path.display(),record.state_dir.display(),record.settings.tls,record.settings.port,record.settings.tlds.join(","),record.settings.lan,record.settings.wildcard);
            } else {
                println!("not installed");
            }
            return Ok(());
        }
        "install" => {}
        "uninstall" => {
            if !path.exists() && !record_path.exists() {
                return Ok(());
            }
            match platform.as_str() {
                "macos" => {
                    exec("launchctl", &["bootout", "system", &p])?;
                }
                "linux" => {
                    exec("systemctl", &["disable", "--now", "porch.service"])?;
                }
                "windows" => {
                    exec("schtasks", &["/Delete", "/TN", "PorchProxy", "/F"])?;
                }
                _ => bail!("unsupported startup service platform"),
            };
            if path.exists() {
                fs::remove_file(&path)?;
            }
            if platform == "linux" {
                exec("systemctl", &["daemon-reload"])?;
            }
            if record_path.exists() {
                fs::remove_file(record_path)?;
            }
            return Ok(());
        }
        _ => bail!("usage: service install|status|uninstall"),
    }
    let settings = service_settings(args)?;
    crate::state::save_proxy_settings(&settings)?;
    let state = crate::state::dir();
    let argv = settings_args(&settings);
    let body=match platform.as_str(){
        "macos"=>format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Label</key><string>dev.porch.proxy</string><key>ProgramArguments</key><array><string>{}</string>{}</array><key>EnvironmentVariables</key><dict><key>PORCH_STATE_DIR</key><string>{}</string></dict><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>",xml(&exe.to_string_lossy()),argv.iter().map(|a|format!("<string>{}</string>",xml(a))).collect::<String>(),xml(&state.to_string_lossy()),xml(&state.join("proxy.log").to_string_lossy()),xml(&state.join("proxy.log").to_string_lossy())),
        "linux"=>format!("[Unit]\nDescription=Porch proxy\nAfter=network.target\n[Service]\nExecStart={} {}\nEnvironment={}\nRestart=on-failure\n[Install]\nWantedBy=multi-user.target\n",systemd_quote(&exe.to_string_lossy())?,argv.iter().map(|a|systemd_quote(a)).collect::<Result<Vec<_>>>()?.join(" "),systemd_quote(&format!("PORCH_STATE_DIR={}",state.display()))?),
        "windows"=>{let line=argv.iter().map(|a|windows_quote(a)).collect::<Vec<_>>().join(" ");format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\"><Triggers><BootTrigger><Enabled>true</Enabled></BootTrigger></Triggers><Principals><Principal id=\"System\"><UserId>S-1-5-18</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure></Settings><Actions Context=\"System\"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions></Task>",xml(&exe.to_string_lossy()),xml(&format!("{line} --state-dir {}",windows_quote(&state.to_string_lossy()))))},_=>bail!("unsupported startup service platform")};
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, body)
        .context("startup service installation requires administrator privileges")?;
    match platform.as_str() {
        "macos" => {
            exec("launchctl", &["bootstrap", "system", &p])?;
        }
        "linux" => {
            exec("systemctl", &["daemon-reload"])?;
            exec("systemctl", &["enable", "--now", "porch.service"])?;
        }
        "windows" => {
            exec(
                "schtasks",
                &["/Create", "/TN", "PorchProxy", "/XML", &p, "/F"],
            )?;
            exec("schtasks", &["/Run", "/TN", "PorchProxy"])?;
        }
        _ => unreachable!(),
    }
    write_state(
        &crate::state::dir().join("service.json"),
        serde_json::to_vec_pretty(&ServiceRecord {
            platform,
            path,
            state_dir: state,
            settings,
        })?,
    )?;
    Ok(())
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
fn ca_fingerprint(ca: &Path) -> Result<String> {
    let out = exec(
        "openssl",
        &[
            "x509",
            "-in",
            &ca.to_string_lossy(),
            "-noout",
            "-fingerprint",
            "-sha1",
        ],
    )?;
    let fingerprint = out
        .trim()
        .split('=')
        .next_back()
        .unwrap_or("")
        .replace(':', "");
    if fingerprint.len() != 40 || !fingerprint.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("could not determine CA fingerprint");
    }
    Ok(fingerprint)
}
fn verify_porch_ca(path: &Path) -> Result<()> {
    let subject = exec(
        "openssl",
        &[
            "x509",
            "-in",
            &path.to_string_lossy(),
            "-noout",
            "-subject",
            "-nameopt",
            "RFC2253",
        ],
    )?;
    if !subject
        .split(',')
        .any(|part| part.trim().trim_start_matches("subject=").trim() == "CN=Porch Local CA")
    {
        bail!("refusing to alter trust for a certificate not issued as Porch Local CA");
    }
    Ok(())
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
    };
    let ca_arg = ca.to_string_lossy();
    match platform.as_str() {
        "macos" if !system => {
            // The login keychain needs no sudo; macOS confirms with a password or Touch ID dialog.
            let keychain = macos_login_keychain(&user_home()?);
            exec(
                "security",
                &[
                    "add-trusted-cert",
                    "-r",
                    "trustRoot",
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
            exec(
                "security",
                &[
                    "add-trusted-cert",
                    "-d",
                    "-r",
                    "trustRoot",
                    "-k",
                    "/Library/Keychains/System.keychain",
                    &ca_arg,
                ],
            )?;
        }
        "windows" => {
            exec("certutil", &["-user", "-addstore", "Root", &ca_arg])?;
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
            fs::copy(&ca, &target)?;
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
/// WSL: browsers run on Windows, so also trust the CA in the Windows user store.
fn trust_wsl_windows(ca_arg: &str, record: &mut TrustRecord) -> Result<()> {
    let wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
        || fs::read_to_string("/proc/version")
            .is_ok_and(|s| s.to_lowercase().contains("microsoft"));
    if wsl && find_program("certutil.exe") {
        let windows_path = exec("wslpath", &["-w", ca_arg])?;
        exec(
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
    let ca = crate::state::dir().join("ca.pem");
    verify_porch_ca(&ca)?;
    if r.fingerprint.len() != 40
        || !r.fingerprint.bytes().all(|c| c.is_ascii_hexdigit())
        || ca_fingerprint(&ca)? != r.fingerprint
    {
        bail!("recorded trust fingerprint does not match the Porch CA; certificate retained");
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
            // Drop the user trust setting first; a missing setting is not an error.
            let _ = exec("security", &["remove-trusted-cert", &ca.to_string_lossy()]);
            exec(
                "security",
                &[
                    "delete-certificate",
                    "-Z",
                    &r.fingerprint,
                    &keychain.to_string_lossy(),
                ],
            )?;
        }
        "linux" if r.scope == "user" => {
            let home = user_home()?;
            let name = nss_name(&r.fingerprint);
            for db in &r.nss_dbs {
                if !db.is_absolute() || !db.starts_with(&home) {
                    bail!("invalid recorded NSS database path; refusing removal");
                }
                if !db.is_dir() {
                    continue;
                }
                let sql = format!("sql:{}", db.display());
                // Only delete the nickname this fingerprint installed, and only where present.
                if exec("certutil", &["-L", "-d", &sql, "-n", &name]).is_ok() {
                    exec("certutil", &["-D", "-d", &sql, "-n", &name])?;
                }
            }
        }
        "macos" => {
            exec(
                "security",
                &[
                    "delete-certificate",
                    "-Z",
                    &r.fingerprint,
                    "/Library/Keychains/System.keychain",
                ],
            )?;
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
        exec(
            if r.platform == "linux" {
                "certutil.exe"
            } else {
                "certutil"
            },
            &["-user", "-delstore", "Root", &r.fingerprint],
        )?;
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
        service("uninstall", &[])?;
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
impl Drop for ShareSession {
    fn drop(&mut self) {
        for child in &mut self.children {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
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
fn tailscale_lock() -> Result<fs::File> {
    #[cfg(unix)]
    let user = unsafe { libc::geteuid() }.to_string();
    #[cfg(not(unix))]
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let key = format!("{:x}", Sha256::digest(user.as_bytes()));
    let path = std::env::temp_dir().join(format!("porch-tailscale-{}.lock", &key[..16]));
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
pub fn prune_shares(force: bool) -> Result<()> {
    for entry in fs::read_dir(crate::state::dir())? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("sharing-")
            || !name.ends_with(".json")
            || !entry.file_type()?.is_file()
        {
            continue;
        }
        let record: SharingRecord = serde_json::from_slice(&fs::read(entry.path())?)?;
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
        for (mode, port) in record.tailscale {
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
                    &[&mode, "--yes", &format!("--https={port}"), "off"],
                )?;
            }
        }
        fs::remove_file(entry.path())?;
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
pub async fn lan_watch(settings: crate::state::ProxySettings) -> Result<()> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let mut guard = Announcements {
        daemon,
        names: Vec::new(),
    };
    let mut previous = Vec::new();
    let mut previous_ip = None;
    loop {
        let ip = match lan_ip(&settings) {
            Ok(ip) => ip,
            Err(e) => {
                eprintln!("LAN address detection: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let state = crate::state::load()?;
        let mut names = state
            .routes
            .iter()
            .filter(|r| crate::state::route_alive(&state, r))
            .map(|r| route_hosts(&r.name, &settings))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        if previous_ip != Some(ip) || names != previous {
            for name in guard.names.drain(..) {
                let _ = guard.daemon.unregister(&name);
            }
            for host in &names {
                let instance = host.trim_end_matches(".local");
                let info = mdns_sd::ServiceInfo::new(
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
                )?;
                guard.names.push(info.get_fullname().to_owned());
                guard.daemon.register(info)?;
            }
            previous = names;
            previous_ip = Some(ip);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
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
        let labels = svc.get("labels").cloned().unwrap_or(json!({}));
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

    loop {
        tokio::select! {
            status=child.wait()=>{let status=status?;reconcile_compose(&project,&globals,&plan)?;let _=request_hosts_sync().await;require_status(status)?;return Ok(());},
            _=tokio::time::sleep(Duration::from_millis(500))=>{if let Err(e)=reconcile_compose(&project,&globals,&plan){eprintln!("Compose route update: {e}");}},
            _=tokio::signal::ctrl_c()=>{stop_compose(&mut child,2).await;let _=reconcile_compose(&project,&globals,&plan);return Err(ExternalExit(130).into());},
            _=terminate_signal()=>{stop_compose(&mut child,15).await;let _=reconcile_compose(&project,&globals,&plan);return Err(ExternalExit(143).into());},
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
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
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
        "service" => service(
            rest.first().map(String::as_str).unwrap_or("status"),
            &rest[rest.len().min(1)..],
        ),
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
async fn doctor() -> Result<()> {
    let settings = crate::state::proxy_settings()?;
    let state = crate::state::load()?;
    println!(
        "state: {}\nproxy: {}\nHTTPS: {}\nTLDs: {}\nLAN: {}\nunprivileged: {}",
        crate::state::dir().display(),
        settings.port,
        settings.tls,
        settings.tlds.join(","),
        settings.lan,
        settings.unprivileged
    );
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
    for address in ["127.0.0.1", "[::1]"] {
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
                    println!("proxy listener {address}: Porch ready (identity verified)");
                } else {
                    println!("proxy listener {address}: foreign/unverified listener; port collision or stale daemon state");
                }
            }
            Err(e) => {
                println!("proxy listener {address}: unavailable ({e}); run porch proxy start")
            }
        }
    }
    #[cfg(unix)]
    if let Ok(output) = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{}", settings.port), "-sTCP:LISTEN"])
        .output()
    {
        if output.status.success() {
            println!(
                "listeners on proxy port:\n{}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
    }
    for r in state.routes {
        let owner = state.owners.get(&r.name).map(crate::state::matches);
        println!(
            "route: {} -> {}{}",
            settings.url(&r.name),
            r.target,
            if owner == Some(false) {
                " (stale launcher)"
            } else {
                ""
            }
        );
        let host = settings.hostname(&r.name);
        match tokio::net::lookup_host((host.as_str(), settings.port)).await {
            Ok(mut addresses) => {
                if addresses.next().is_some() {
                    println!("DNS {host}: resolves");
                } else {
                    println!("DNS {host}: unavailable; run porch hosts sync");
                }
            }
            _ => println!("DNS {host}: unavailable; run porch hosts sync"),
        };
    }
    println!(
        "CA: {}",
        if crate::state::dir().join("ca.pem").is_file() {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "CA trust: {}",
        match trust_scope().as_deref() {
            Some("user") => "installed for this user by Porch (recorded)",
            Some(_) => "installed system-wide by Porch (recorded)",
            None => "not recorded; run porch trust",
        }
    );
    for (tool, flag) in [
        ("node", "--version"),
        ("openssl", "version"),
        ("docker", "--version"),
        ("tailscale", "version"),
        ("ngrok", "version"),
        ("cloudflared", "--version"),
    ] {
        match exec(tool, &[flag]) {
            Ok(out) => println!("{tool}: {}", out.trim()),
            Err(e) => println!("{tool}: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
