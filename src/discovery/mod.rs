//! Opt-in discovery agent lifecycle and shared browser actions.
pub mod autostart;
pub mod client;
pub mod commands;
pub mod gateway;
pub mod pairing;
mod pake;
pub mod server;
pub mod types;

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
use types::*;

fn process_identity(kind: &str) -> Option<crate::state::ProcessIdentity> {
    serde_json::from_slice(&fs::read(crate::state::dir().join(format!("{kind}.identity"))).ok()?)
        .ok()
}
pub fn running() -> bool {
    process_identity("discovery").is_some_and(|p| {
        crate::state::matches(&p)
            && fs::read_to_string(crate::state::dir().join("discovery.ready"))
                .is_ok_and(|s| s.trim() == p.pid.to_string())
    })
}
fn private_file(path: PathBuf) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let f = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(f)
}
fn lock(name: &str) -> Result<fs::File> {
    crate::state::load()?;
    // Not private_file: that opens append-only, and Windows refuses to lock a
    // handle without read or write access.
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(crate::state::dir().join(name))?;
    file.try_lock_exclusive()
        .context("another Porch operation is in progress; retry shortly")?;
    Ok(file)
}
pub async fn enable(config: ServerConfig) -> Result<()> {
    let _lock = lock("discovery.start.lock")?;
    if running() {
        bail!("discovery already running; disable it before changing its exposure")
    }
    server::configure(config)?;
    spawn_agent().await
}
/// Start the configured agent as a detached process and wait until it is ready.
async fn spawn_agent() -> Result<()> {
    let _ = fs::remove_file(crate::state::dir().join("discovery.ready"));
    let log = private_file(crate::state::dir().join("discovery.log"))?;
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .args(["discovery", "serve"])
        .env("PORCH_STATE_DIR", crate::state::dir())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    detach(&mut command);
    let mut child = command.spawn().context("starting discovery agent")?;
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if running() {
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                bail!(
                    "discovery exited {status}; see {}",
                    crate::state::dir().join("discovery.log").display()
                )
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await;
    match result {
        Ok(r) => r,
        Err(_) => {
            let _ = child.kill().await;
            bail!("discovery startup timed out; see discovery.log")
        }
    }
}
fn detach(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    // Outlive the launching console: no console of its own, and Ctrl+C in the
    // terminal that ran `porch pair` must not reach it.
    #[cfg(windows)]
    command.creation_flags(
        windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP
            | windows_sys::Win32::System::Threading::DETACHED_PROCESS,
    );
}
struct AgentGuard;
impl Drop for AgentGuard {
    fn drop(&mut self) {
        if process_identity("discovery").is_some_and(|i| i.pid == std::process::id()) {
            for name in ["discovery.identity", "discovery.ready"] {
                let _ = fs::remove_file(crate::state::dir().join(name));
            }
        }
    }
}
pub async fn serve(autostart: bool) -> Result<()> {
    if autostart {
        #[cfg(windows)]
        log_without_console()?;
        // Login startup must not fail-loop: an agent that is already running, or
        // sharing that was turned off, is a clean exit for launchd/systemd.
        if running() {
            eprintln!("Porch discovery is already running");
            return Ok(());
        }
        if server::config()?.is_none() {
            eprintln!("Porch discovery is disabled; nothing to start");
            return Ok(());
        }
    }
    let _lock = lock("discovery.agent.lock")?;
    crate::shutdown::listen()?;
    let identity = crate::state::identity(std::process::id()).context("cannot identify agent")?;
    crate::state::atomic_write("discovery.identity", &serde_json::to_vec(&identity)?)?;
    let _guard = AgentGuard;
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { r=server::serve()=>r, _=term.recv()=>Ok(()), _=tokio::signal::ctrl_c()=>Ok(()) }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            r=server::serve()=>r,
            _=tokio::signal::ctrl_c()=>Ok(()),
            _=crate::shutdown::requested()=>Ok(()),
        }
    }
}
/// Task Scheduler starts the agent with a console window and no output
/// redirection: send output to discovery.log, then let the window close.
#[cfg(windows)]
fn log_without_console() -> Result<()> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{
        FreeConsole, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };
    let log = private_file(crate::state::dir().join("discovery.log"))?;
    let out = log.try_clone()?.into_raw_handle();
    let err = log.into_raw_handle();
    // Rust's stdout/stderr look the standard handles up on every write.
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, out);
        SetStdHandle(STD_ERROR_HANDLE, err);
        FreeConsole();
    }
    Ok(())
}
async fn stop_identity(identity: &crate::state::ProcessIdentity) -> Result<()> {
    if !crate::state::matches(identity) {
        return Ok(());
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(identity.pid as i32, libc::SIGTERM);
    }
    #[cfg(windows)]
    if !crate::shutdown::request(identity.pid) {
        crate::shutdown::kill_tree(identity.pid);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while crate::state::matches(identity) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    // The agent may be wedged; on Windows there is no later chance to force it.
    #[cfg(windows)]
    if crate::state::matches(identity) {
        crate::shutdown::kill_tree(identity.pid);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while crate::state::matches(identity) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
    if crate::state::matches(identity) {
        bail!("agent did not stop; no other process was signaled")
    }
    Ok(())
}
/// Stop the running agent, leaving its configuration and login startup alone.
pub(super) async fn stop_agent() -> Result<()> {
    let _lock = lock("discovery.start.lock")?;
    if let Some(i) = process_identity("discovery") {
        stop_identity(&i).await?;
    }
    Ok(())
}
pub async fn disable() -> Result<()> {
    let _lock = lock("discovery.start.lock")?;
    // Sharing off also means not starting it again at the next login.
    autostart::remove()?;
    if let Some(i) = process_identity("discovery") {
        stop_identity(&i).await?;
    }
    server::disable()?;
    Ok(())
}
#[derive(Serialize)]
pub struct Status {
    pub version: u32,
    pub running: bool,
    pub config: Option<ServerConfig>,
    pub machine: Machine,
    pub fingerprint: Option<String>,
    /// Discovery starts at login (`porch discovery autostart`).
    pub autostart: bool,
}
pub fn status() -> Result<Status> {
    let config = server::config()?;
    Ok(Status {
        version: PROTOCOL_VERSION,
        running: running(),
        fingerprint: config.as_ref().map(|_| server::fingerprint()).transpose()?,
        config,
        machine: server::identity()?,
        autostart: autostart::installed(),
    })
}

#[derive(Serialize, Deserialize)]
struct RelayRecord {
    #[serde(default)]
    peer_id: String,
    #[serde(default)]
    enrollment: String,
    process: crate::state::ProcessIdentity,
    url: String,
}
fn relay_key(peer: &str, app: &str) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "discovery-relay-{:x}.json",
        Sha256::digest(format!("{peer}\0{app}"))
    )
}
/// Start a loopback browser transport. The agent URL is never opened directly.
pub async fn app_url(app: &str, peer: Option<&str>) -> Result<String> {
    if let Some(peer_id) = peer {
        let selected = client::find_peer(peer_id)?;
        let peer_id = &selected.machine.id;
        let all = client::snapshot(false).await?;
        let remote = all
            .peers
            .iter()
            .find(|p| &p.machine.id == peer_id)
            .context("paired peer unavailable")?;
        let catalog = remote.catalog.as_ref().with_context(|| {
            remote
                .error
                .clone()
                .unwrap_or_else(|| "peer unavailable".into())
        })?;
        let found: Vec<_> = catalog
            .apps
            .iter()
            .filter(|a| a.id == app || a.name == app)
            .collect();
        if found.len() != 1 {
            bail!("app must identify exactly one app on the selected peer")
        }
        let found = found[0];
        if !found.remote_access {
            bail!(
                "{}",
                found
                    .unavailable_reason
                    .as_deref()
                    .unwrap_or("remote app access is disabled on this machine")
            )
        }
        // Readable, stable URL served by the local proxy: APP.MACHINE.localhost.
        if let Some(name) = gateway::hostname_app(&found.name) {
            if !all.local.proxy_running {
                bail!("the local proxy is stopped; start it with `porch proxy start` to open apps on {}", selected.machine.name)
            }
            let settings = crate::state::proxy_settings()?;
            return Ok(settings.url(&format!("{name}.{}", selected.slug)));
        }
        // Path-routed apps cannot be named in a hostname; use a dedicated relay.
        let _lock = lock("discovery.relay.lock")?;
        // A reconnect or renewed enrollment must not reuse a relay holding an old
        // endpoint or revoked bearer. Store only a digest, never another token copy.
        let fresh = client::peers()?
            .into_iter()
            .find(|p| &p.machine.id == peer_id)
            .context("peer was removed during refresh")?;
        use sha2::{Digest, Sha256};
        let enrollment = format!(
            "{:x}",
            Sha256::digest(format!(
                "{}\0{}\0{}",
                fresh.endpoint, fresh.fingerprint, fresh.token
            ))
        );
        let key = relay_key(peer_id, &found.id);
        if let Ok(bytes) = fs::read(crate::state::dir().join(&key)) {
            if let Ok(record) = serde_json::from_slice::<RelayRecord>(&bytes) {
                if crate::state::matches(&record.process) {
                    if record.enrollment == enrollment {
                        return Ok(record.url);
                    }
                    stop_identity(&record.process).await?;
                }
            }
        }
        let log = private_file(crate::state::dir().join("discovery-relay.log"))?;
        let mut cmd = tokio::process::Command::new(std::env::current_exe()?);
        cmd.args(["discovery", "relay", "--peer", peer_id, "--app", &found.id])
            .env("PORCH_STATE_DIR", crate::state::dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log);
        detach(&mut cmd);
        let mut child = cmd.spawn()?;
        let mut lines = BufReader::new(
            child
                .stdout
                .take()
                .context("relay startup pipe unavailable")?,
        )
        .lines();
        let ready = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
        let value = match ready {
            Ok(Ok(Some(line))) => serde_json::from_str::<serde_json::Value>(&line),
            _ => {
                let _ = child.kill().await;
                bail!("relay startup failed; see discovery-relay.log")
            }
        };
        let value = match value {
            Ok(v) => v,
            Err(e) => {
                let _ = child.kill().await;
                return Err(e.into());
            }
        };
        let url = value["url"]
            .as_str()
            .context("relay did not provide a URL")?
            .to_string();
        let parsed = url::Url::parse(&url)?;
        let local_host = parsed.host_str().is_some_and(|h| {
            h.ends_with(".localhost")
                && h.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        });
        if parsed.scheme() != "http"
            || !local_host
            || parsed.port().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            let _ = child.kill().await;
            bail!("relay returned an invalid local URL")
        }
        let identity = child
            .id()
            .and_then(crate::state::identity)
            .context("cannot identify relay")?;
        crate::state::atomic_write(
            &key,
            &serde_json::to_vec(&RelayRecord {
                peer_id: peer_id.clone(),
                enrollment,
                process: identity,
                url: url.clone(),
            })?,
        )?;
        Ok(url)
    } else {
        let local = server::local_catalog()?;
        let found: Vec<_> = local
            .apps
            .iter()
            .filter(|a| a.id == app || a.name == app)
            .collect();
        if found.len() != 1 {
            bail!("app must identify exactly one local app")
        }
        if !local.proxy_running {
            bail!("local proxy is stopped; start it with porch proxy start")
        }
        Ok(found[0].local_url.clone())
    }
}
pub async fn disconnect() -> Result<()> {
    disconnect_matching(None).await
}
pub async fn disconnect_peer(id: &str) -> Result<()> {
    disconnect_matching(Some(id)).await
}
async fn disconnect_matching(peer: Option<&str>) -> Result<()> {
    crate::state::load()?;
    for entry in fs::read_dir(crate::state::dir())? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("discovery-relay-") && name.ends_with(".json") {
            if let Ok(record) = serde_json::from_slice::<RelayRecord>(&fs::read(entry.path())?) {
                if peer.is_some_and(|id| id != record.peer_id) {
                    continue;
                }
                stop_identity(&record.process).await?;
            } else if peer.is_some() {
                continue;
            }
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}
pub fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    // `cmd /c start` would reinterpret `&` and `^` in the URL; the URL protocol
    // handler takes it as a single argument.
    #[cfg(windows)]
    let mut command = {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    bail!("opening apps supports macOS, Linux and Windows");
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        command.arg(url);
        if !command
            .status()
            .context("opening browser; use --url-only in a headless session")?
            .success()
        {
            bail!("browser could not open {url}; use --url-only")
        }
        Ok(())
    }
}
pub fn copy_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let candidates: Vec<(&str, Vec<&str>)> = vec![("pbcopy", vec![])];
    #[cfg(target_os = "linux")]
    let candidates = vec![
        ("wl-copy", vec![]),
        ("xclip", vec!["-selection", "clipboard"]),
        ("xsel", vec!["--clipboard", "--input"]),
    ];
    #[cfg(windows)]
    let candidates: Vec<(&str, Vec<&str>)> = vec![("clip", vec![])];
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let candidates: Vec<(&str, Vec<&str>)> = vec![];
    for (bin, args) in candidates {
        if let Ok(mut child) = std::process::Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut input) = child.stdin.take() {
                input.write_all(url.as_bytes())?;
            }
            if child.wait()?.success() {
                return Ok(());
            }
        }
    }
    bail!("clipboard unavailable; use `porch open APP --url-only` to print the URL")
}
