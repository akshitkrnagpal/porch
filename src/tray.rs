//! Porch's optional native menu. All network/lifecycle work goes through the CLI
//! on a Tokio worker; native UI objects never leave the OS main thread.
use crate::discovery::types::{
    App, Health, JoinFailure, Machine, PairShown, PairStatusReport, Snapshot, Source,
};
use anyhow::{bail, Context, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;

#[path = "tray_pairing.rs"]
pub mod pairing;
use pairing::{JoinAttempt, PairingUpdate, PairingView};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Open {
        app: String,
        peer: Option<String>,
    },
    Copy {
        app: String,
        peer: Option<String>,
    },
    StartProxy,
    StopProxy,
    /// Show a pairing code (host side of `porch pair`), first cancelling `replace`.
    ShowCode {
        replace: Option<String>,
    },
    /// `porch pair cancel ID`.
    CancelCode {
        id: String,
    },
    /// Copy a pairing code or command; `what` names it in the notice.
    CopyText {
        text: String,
        what: String,
    },
    /// Ask for a code (dialog), optionally for one machine, then join.
    EnterCode {
        host: Option<String>,
        name: Option<String>,
        prefill: String,
    },
    /// Ask for an address, then a code.
    EnterCodeWithAddress {
        prefill: String,
    },
    /// `porch pair CODE [--host H] --json` in the background.
    Join(JoinAttempt),
    CancelJoin,
    DismissHost,
    DismissJoin,
    /// Ask for confirmation, then unpair.
    Unpair {
        peer: String,
        name: String,
    },
    UnpairConfirmed {
        peer: String,
        name: String,
    },
    CopyFingerprint {
        fingerprint: String,
    },
    StopSharing,
    StartAtLogin,
    StopAtLogin,
    /// A dialog could not be shown.
    DialogFailed(String),
    Refresh,
    Reconnect,
    Quit,
}
impl Action {
    /// Pass argument vectors to the existing CLI, never to a shell.
    pub fn cli_args(&self) -> Option<Vec<String>> {
        let args = match self {
            Self::Open { app, peer } | Self::Copy { app, peer } => {
                let mut args = vec![
                    if matches!(self, Self::Open { .. }) {
                        "open"
                    } else {
                        "copy"
                    }
                    .into(),
                    app.clone(),
                ];
                if let Some(peer) = peer {
                    args.extend(["--peer".into(), peer.clone()]);
                }
                args
            }
            Self::StartProxy => vec!["proxy".into(), "start".into()],
            Self::StopProxy => vec!["proxy".into(), "stop".into()],
            Self::Reconnect => vec!["peers".into(), "refresh".into()],
            Self::ShowCode { .. } => vec!["pair".into(), "--no-wait".into(), "--json".into()],
            Self::CancelCode { id } => {
                vec!["pair".into(), "cancel".into(), id.clone(), "--json".into()]
            }
            Self::Join(attempt) => {
                let mut args = vec!["pair".into(), attempt.code.clone()];
                if let Some(host) = &attempt.host {
                    args.extend(["--host".into(), host.clone()]);
                }
                args.push("--json".into());
                args
            }
            Self::UnpairConfirmed { peer, .. } => {
                vec!["unpair".into(), peer.clone(), "--json".into()]
            }
            Self::StopSharing => vec!["discovery".into(), "disable".into()],
            Self::StartAtLogin => vec!["discovery".into(), "autostart".into(), "enable".into()],
            Self::StopAtLogin => vec!["discovery".into(), "autostart".into(), "disable".into()],
            _ => return None,
        };
        Some(args)
    }
    /// Handled on the UI thread (dialogs and local state), not by the worker.
    fn is_ui(&self) -> bool {
        matches!(
            self,
            Self::EnterCode { .. }
                | Self::EnterCodeWithAddress { .. }
                | Self::Unpair { .. }
                | Self::DismissHost
                | Self::DismissJoin
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub label: String,
    pub enabled: bool,
    pub action: Option<Action>,
    pub children: Vec<Entry>,
    pub separator: bool,
}
impl Entry {
    fn text(label: impl Into<String>) -> Self {
        Self {
            label: clean_label(&label.into()),
            enabled: false,
            action: None,
            children: vec![],
            separator: false,
        }
    }
    fn action(label: impl Into<String>, action: Action, enabled: bool) -> Self {
        Self {
            label: label.into(),
            enabled,
            action: Some(action),
            children: vec![],
            separator: false,
        }
    }
    fn group(label: impl Into<String>, children: Vec<Entry>) -> Self {
        Self {
            label: clean_label(&label.into()),
            enabled: true,
            action: None,
            children,
            separator: false,
        }
    }
    fn separator() -> Self {
        Self {
            label: String::new(),
            enabled: false,
            action: None,
            children: vec![],
            separator: true,
        }
    }
}
fn clean_label(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(140)
        .collect::<String>()
        .replace('&', "&&")
}
fn health_label(health: &Health) -> &'static str {
    match health {
        Health::Healthy => "running",
        Health::Stopped => "stopped",
        Health::Unreachable => "unreachable",
        Health::Unknown => "status unknown",
    }
}
fn app_entry(app: &App, peer: Option<&str>, proxy_running: bool) -> Entry {
    let remote = peer.is_some();
    let enabled = if remote {
        app.remote_access
    } else {
        proxy_running
    } && !matches!(app.health, Health::Stopped | Health::Unreachable);
    let mut children = vec![
        Entry::action(
            "Open in browser",
            Action::Open {
                app: app.id.clone(),
                peer: peer.map(str::to_owned),
            },
            enabled,
        ),
        Entry::action(
            "Copy URL",
            Action::Copy {
                app: app.id.clone(),
                peer: peer.map(str::to_owned),
            },
            enabled,
        ),
        Entry::separator(),
        Entry::text(health_label(&app.health)),
    ];
    if remote && !app.remote_access {
        children.push(Entry::text(
            app.unavailable_reason
                .as_deref()
                .unwrap_or("App sharing is off on this machine"),
        ));
    } else if !remote && !proxy_running {
        children.push(Entry::text("Start the proxy to open this app"));
    } else if !remote && !app.remote_access {
        children.push(Entry::text("Local only; app sharing is off"));
    }
    Entry::group(
        format!("{} ({})", app.name, health_label(&app.health)),
        children,
    )
}
/// A pure model makes enabled states and action routing testable without a display.
/// What the tray mark shows: solid or dimmed apps under the roof, plus a
/// badge when a paired machine is reachable. Artwork lives in `assets/icon`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IconState {
    pub proxy_running: bool,
    pub peers: bool,
}

impl IconState {
    /// Before the first refresh nothing is known, so show the stopped mark
    /// rather than claiming the proxy is up.
    pub fn from_snapshot(snapshot: Option<&Snapshot>) -> Self {
        snapshot.map_or(
            Self {
                proxy_running: false,
                peers: false,
            },
            |snapshot| Self {
                proxy_running: snapshot.local.proxy_running,
                peers: snapshot
                    .peers
                    .iter()
                    .any(|peer| peer.catalog.is_some() && peer.error.is_none()),
            },
        )
    }

    /// Raw 8-bit RGBA artwork and its square edge in pixels. macOS gets an 18pt
    /// @2x black template; Linux gets the light grayscale mark at 32px.
    pub fn artwork(self) -> (&'static [u8], u32) {
        macro_rules! art {
            ($platform:literal, $size:literal) => {
                match (self.proxy_running, self.peers) {
                    (true, false) => include_bytes!(concat!(
                        "../assets/icon/rgba/",
                        $platform,
                        "-running-",
                        $size,
                        ".rgba"
                    ))
                    .as_slice(),
                    (false, false) => include_bytes!(concat!(
                        "../assets/icon/rgba/",
                        $platform,
                        "-stopped-",
                        $size,
                        ".rgba"
                    ))
                    .as_slice(),
                    (true, true) => include_bytes!(concat!(
                        "../assets/icon/rgba/",
                        $platform,
                        "-peers-",
                        $size,
                        ".rgba"
                    ))
                    .as_slice(),
                    (false, true) => include_bytes!(concat!(
                        "../assets/icon/rgba/",
                        $platform,
                        "-peers-stopped-",
                        $size,
                        ".rgba"
                    ))
                    .as_slice(),
                }
            };
        }
        if cfg!(target_os = "macos") {
            (art!("macos", "36"), 36)
        } else {
            (art!("linux", "32"), 32)
        }
    }
}

pub fn menu_model(
    snapshot: Option<&Snapshot>,
    error: Option<&str>,
    notice: Option<&str>,
    pairing: &PairingView,
) -> Vec<Entry> {
    let mut entries = vec![Entry::text("Porch")];
    if let Some(notice) = notice {
        entries.push(Entry::text(notice));
    }
    if let Some(error) = error {
        entries.push(Entry::text(format!("Could not refresh: {error}")));
    }
    entries.extend(pairing::status_entries(pairing));
    entries.push(Entry::separator());
    if let Some(snapshot) = snapshot {
        let mut local = vec![Entry::text(if snapshot.local.proxy_running {
            "Proxy is running"
        } else {
            "Proxy is stopped"
        })];
        if snapshot.local.apps.is_empty() {
            local.push(Entry::text("No apps yet; run porch in a project"));
        }
        local.extend(
            snapshot
                .local
                .apps
                .iter()
                .take(100)
                .map(|app| app_entry(app, None, snapshot.local.proxy_running)),
        );
        entries.push(Entry::group(
            format!("This machine ({})", snapshot.local.machine.name),
            local,
        ));
        if snapshot.peers.is_empty() {
            entries.push(Entry::text("No paired machines yet; choose Pair a device"));
        }
        for peer in snapshot.peers.iter().take(50) {
            let sources = peer
                .sources
                .iter()
                .map(|source| match source {
                    Source::Local => "local",
                    Source::Lan => "LAN",
                    Source::Tailscale => "Tailscale",
                    Source::Explicit => "paired",
                })
                .collect::<Vec<_>>()
                .join(", ");
            let mut apps = vec![Entry::text(format!("Via {sources}"))];
            if let Some(error) = &peer.error {
                apps.push(Entry::text(format!("Unavailable: {error}")));
            }
            if let Some(catalog) = &peer.catalog {
                if catalog.apps.is_empty() {
                    apps.push(Entry::text("No apps on this machine"));
                }
                apps.extend(
                    catalog
                        .apps
                        .iter()
                        .take(100)
                        .map(|app| app_entry(app, Some(&peer.machine.id), catalog.proxy_running)),
                );
            } else {
                apps.push(Entry::text("Reconnect to check this machine"));
            }
            apps.push(Entry::separator());
            if !peer.fingerprint.is_empty() {
                apps.push(Entry::text(format!(
                    "Fingerprint {}",
                    pairing::short_fingerprint(&peer.fingerprint)
                )));
                apps.push(Entry::action(
                    "Copy fingerprint",
                    Action::CopyFingerprint {
                        fingerprint: peer.fingerprint.clone(),
                    },
                    true,
                ));
            }
            apps.push(Entry::action(
                "Unpair…",
                Action::Unpair {
                    peer: peer.machine.id.clone(),
                    name: peer.machine.name.clone(),
                },
                true,
            ));
            entries.push(Entry::group(peer.machine.name.clone(), apps));
        }
        let joining = matches!(pairing.join, pairing::JoinView::Joining { .. });
        entries.extend(pairing::nearby_menu(&snapshot.candidates, joining));
        for warning in snapshot.warnings.iter().take(3) {
            entries.push(Entry::text(warning));
        }
        entries.push(Entry::separator());
        entries.push(pairing::pair_menu(pairing));
        if let Some(sharing) = sharing_menu(snapshot) {
            entries.push(sharing);
        }
        entries.push(if snapshot.local.proxy_running {
            Entry::action("Stop proxy", Action::StopProxy, true)
        } else {
            Entry::action("Start proxy", Action::StartProxy, true)
        });
    } else {
        entries.push(Entry::text(if error.is_some() {
            "Check the porch CLI and state directory"
        } else {
            "Loading apps…"
        }));
        entries.push(pairing::pair_menu(pairing));
    }
    entries.extend([
        Entry::action("Refresh apps", Action::Refresh, true),
        Entry::action("Reconnect discovery", Action::Reconnect, true),
        Entry::separator(),
        Entry::action("Quit Porch menu", Action::Quit, true),
    ]);
    entries
}
/// "Sharing": shown once this machine has shared (pairing turns it on).
fn sharing_menu(snapshot: &Snapshot) -> Option<Entry> {
    let sharing = snapshot.sharing.as_ref()?;
    if !sharing.running && !sharing.share_apps {
        return None;
    }
    let mut children = vec![Entry::text(if sharing.running && sharing.share_apps {
        "Paired machines can open your apps"
    } else if sharing.running {
        "Discovery is on; apps are not shared"
    } else {
        "Sharing is set up but not running"
    })];
    children.push(Entry::action("Stop sharing", Action::StopSharing, true));
    children.push(if sharing.autostart {
        Entry::action("Don't start sharing at login", Action::StopAtLogin, true)
    } else {
        Entry::action(
            "Start sharing at login",
            Action::StartAtLogin,
            sharing.running,
        )
    });
    Some(Entry::group("Sharing", children))
}

pub fn cli_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PORCH_BIN") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            bail!("PORCH_BIN does not name a Porch CLI executable");
        }
        return Ok(path);
    }
    let current = std::env::current_exe()?;
    if let Some(parent) = current.parent() {
        let sibling = parent.join("porch");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    let home = std::env::var_os("HOME").context("HOME is unavailable; set PORCH_BIN to the CLI")?;
    let installed = PathBuf::from(home).join(".local/bin/porch");
    if installed.is_file() {
        return Ok(installed);
    }
    bail!("Porch CLI not found. Install porch beside porch-tray or set PORCH_BIN")
}
/// Exit status, stdout and the first stderr line of one CLI call.
struct Output {
    success: bool,
    stdout: Vec<u8>,
    message: String,
}
async fn run_cli(cli: &Path, args: &[String], limit: Duration) -> Result<Output> {
    let mut child = tokio::process::Command::new(cli)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("launch Porch CLI")?;
    let mut stdout = child
        .stdout
        .take()
        .context("CLI stdout")?
        .take(4 * 1024 * 1024 + 1);
    let mut stderr = child
        .stderr
        .take()
        .context("CLI stderr")?
        .take(64 * 1024 + 1);
    let result = tokio::time::timeout(limit, async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (status, _, _) = tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        )?;
        if out.len() > 4 * 1024 * 1024 || err.len() > 64 * 1024 {
            bail!("Porch CLI response is too large");
        }
        Ok(Output {
            success: status.success(),
            stdout: out,
            message: String::from_utf8_lossy(&err)
                .lines()
                .next()
                .unwrap_or("Porch command failed")
                .trim_start_matches("porch: ")
                .to_owned(),
        })
    })
    .await
    .context("Porch command timed out; try it in a terminal")?;
    result
}
async fn invoke(cli: &Path, args: &[String]) -> Result<Vec<u8>> {
    let output = run_cli(cli, args, Duration::from_secs(35)).await?;
    if !output.success {
        bail!("{}", output.message);
    }
    Ok(output.stdout)
}
/// A join probes, may start the agent and waits on the other machine.
const JOIN_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the code being shown is checked while it is active.
const STATUS_POLL: Duration = Duration::from_secs(2);

/// Result of `porch pair CODE --json`: the peer, or the structured failure.
fn join_result(output: &Output) -> Result<Machine, JoinFailure> {
    let value = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
    if output.success {
        if let Some(machine) = value
            .as_ref()
            .and_then(|v| serde_json::from_value::<Machine>(v["machine"].clone()).ok())
        {
            return Ok(machine);
        }
    }
    Err(value
        .and_then(|v| serde_json::from_value::<JoinFailure>(v["error"].clone()).ok())
        .unwrap_or_else(|| JoinFailure {
            detail: Some(output.message.clone()),
            ..crate::discovery::pairing::failure("local")
        }))
}

/// The tray remembers which code it showed (never the code itself), so after a
/// restart it can say a code is active and offer to cancel it.
#[derive(serde::Serialize, serde::Deserialize)]
struct ShownRecord {
    id: String,
    expires: u64,
}
fn remember(file: Option<&Path>, record: Option<&ShownRecord>) {
    let Some(file) = file else { return };
    match record.and_then(|r| serde_json::to_vec(r).ok()) {
        Some(bytes) => {
            let _ = std::fs::write(file, bytes);
        }
        None => {
            let _ = std::fs::remove_file(file);
        }
    }
}
fn recall(file: Option<&Path>) -> Option<ShownRecord> {
    let record: ShownRecord = serde_json::from_slice(&std::fs::read(file?).ok()?).ok()?;
    (record.expires > crate::discovery::server::now()).then_some(record)
}

enum Update {
    Snapshot(Result<Snapshot, String>),
    Notice(String),
    Pairing(PairingUpdate),
}
fn worker(cli: PathBuf) -> (Sender<Action>, Receiver<Update>) {
    worker_with_intervals(
        cli,
        Duration::from_secs(15),
        Duration::from_secs(60),
        Some(crate::state::dir().join("tray-pairing.json")),
    )
}
fn copy(updates: &Sender<Update>, text: &str, what: &str) {
    let message = match crate::discovery::copy_url(text) {
        Ok(()) => format!("{what} copied"),
        Err(error) => format!("{error:#}"),
    };
    let _ = updates.send(Update::Notice(message));
}
/// Watch one shown code: report each state change, flag a peer that accepted
/// but never connected, and stop once the code is finished.
async fn poll_code(cli: PathBuf, id: String, updates: Sender<Update>, file: Option<PathBuf>) {
    let args = vec!["pair".into(), "status".into(), id.clone(), "--json".into()];
    let mut last = None;
    let mut accepted: Option<Instant> = None;
    let mut overdue = false;
    loop {
        if let Ok(report) = invoke(&cli, &args)
            .await
            .and_then(|b| Ok(serde_json::from_slice::<PairStatusReport>(&b)?))
        {
            let key = (report.state, report.peer.clone());
            if last.as_ref() != Some(&key) {
                last = Some(key);
                let _ = updates.send(Update::Pairing(PairingUpdate::Status(report.clone())));
            }
            use crate::discovery::types::PairState;
            if report.state == PairState::Accepted {
                let since = *accepted.get_or_insert_with(Instant::now);
                if !overdue && since.elapsed() >= crate::discovery::pairing::CONFIRM_GRACE {
                    overdue = true;
                    let _ = updates.send(Update::Pairing(PairingUpdate::Overdue(id.clone())));
                }
            }
            if report.state.is_final() {
                remember(file.as_deref(), None);
                if report.state == PairState::Paired {
                    refresh_snapshot(&cli, &updates).await;
                }
                return;
            }
        }
        tokio::time::sleep(if overdue {
            STATUS_POLL * 5
        } else {
            STATUS_POLL
        })
        .await;
    }
}
async fn refresh_snapshot(cli: &Path, updates: &Sender<Update>) {
    let args = vec!["list".into(), "--all".into(), "--json".into()];
    let result = async {
        let bytes = invoke(cli, &args).await?;
        serde_json::from_slice::<Snapshot>(&bytes).context("Porch returned an incompatible catalog")
    }
    .await
    .map_err(|error| format!("{error:#}"));
    let _ = updates.send(Update::Snapshot(result));
}
fn worker_with_intervals(
    cli: PathBuf,
    poll: Duration,
    discovery: Duration,
    pairing_file: Option<PathBuf>,
) -> (Sender<Action>, Receiver<Update>) {
    let (commands, command_rx) = mpsc::channel();
    let (updates, update_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = updates.send(Update::Snapshot(Err(error.to_string())));
                return;
            }
        };
        let refresh = |updates: &Sender<Update>, discover: bool| {
            let result = runtime
                .block_on(async {
                    let args = if discover {
                        vec!["peers".into(), "refresh".into(), "--json".into()]
                    } else {
                        vec!["list".into(), "--all".into(), "--json".into()]
                    };
                    let bytes = invoke(&cli, &args).await?;
                    serde_json::from_slice::<Snapshot>(&bytes)
                        .context("Porch returned an incompatible catalog")
                })
                .map_err(|error| format!("{error:#}"));
            let _ = updates.send(Update::Snapshot(result));
        };
        let mut poller: Option<tokio::task::AbortHandle> = None;
        let mut join: Option<tokio::task::AbortHandle> = None;
        let watch = |id: String, poller: &mut Option<tokio::task::AbortHandle>| {
            if let Some(old) = poller.take() {
                old.abort();
            }
            *poller = Some(
                runtime
                    .spawn(poll_code(
                        cli.clone(),
                        id,
                        updates.clone(),
                        pairing_file.clone(),
                    ))
                    .abort_handle(),
            );
        };
        if let Some(record) = recall(pairing_file.as_deref()) {
            let _ = updates.send(Update::Pairing(PairingUpdate::Restored {
                id: record.id.clone(),
                expires: record.expires,
            }));
            watch(record.id, &mut poller);
        }
        refresh(&updates, true);
        let mut last_discovery = std::time::Instant::now();
        loop {
            let action = match command_rx.recv_timeout(poll) {
                Ok(Action::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(action) => action,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let discover = last_discovery.elapsed() >= discovery;
                    refresh(&updates, discover);
                    if discover {
                        last_discovery = std::time::Instant::now();
                    }
                    continue;
                }
            };
            let pairing = |update| {
                let _ = updates.send(Update::Pairing(update));
            };
            match &action {
                Action::ShowCode { replace } => {
                    if let Some(id) = replace {
                        let cancel = Action::CancelCode { id: id.clone() };
                        let _ =
                            runtime.block_on(invoke(&cli, &cancel.cli_args().unwrap_or_default()));
                    }
                    let shown = runtime.block_on(async {
                        let bytes = invoke(&cli, &action.cli_args().unwrap_or_default()).await?;
                        Ok::<_, anyhow::Error>(serde_json::from_slice::<PairShown>(&bytes)?)
                    });
                    match shown {
                        Ok(shown) => {
                            remember(
                                pairing_file.as_deref(),
                                Some(&ShownRecord {
                                    id: shown.id.clone(),
                                    expires: shown.expires,
                                }),
                            );
                            watch(shown.id.clone(), &mut poller);
                            pairing(PairingUpdate::Shown(shown));
                        }
                        Err(error) => pairing(PairingUpdate::ShowFailed(format!(
                            "Could not show a code: {error:#}"
                        ))),
                    }
                    refresh(&updates, false);
                    continue;
                }
                Action::CancelCode { .. } => {
                    // The poller reports the cancelled state and stops.
                    if let Err(error) =
                        runtime.block_on(invoke(&cli, &action.cli_args().unwrap_or_default()))
                    {
                        let _ = updates.send(Update::Notice(format!("{error:#}")));
                    }
                    continue;
                }
                Action::Join(attempt) => {
                    if let Some(old) = join.take() {
                        old.abort();
                    }
                    pairing(PairingUpdate::Joining(attempt.clone()));
                    let (cli, updates, attempt) = (cli.clone(), updates.clone(), attempt.clone());
                    let args = action.cli_args().unwrap_or_default();
                    join = Some(
                        runtime
                            .spawn(async move {
                                let update = match run_cli(&cli, &args, JOIN_TIMEOUT).await {
                                    Ok(output) => match join_result(&output) {
                                        Ok(machine) => PairingUpdate::Joined(machine),
                                        Err(failure) => PairingUpdate::JoinFailed(failure, attempt),
                                    },
                                    Err(error) => PairingUpdate::JoinFailed(
                                        JoinFailure {
                                            detail: Some(format!("{error:#}")),
                                            ..crate::discovery::pairing::failure("unreachable")
                                        },
                                        attempt,
                                    ),
                                };
                                let joined = matches!(update, PairingUpdate::Joined(_));
                                let _ = updates.send(Update::Pairing(update));
                                if joined {
                                    refresh_snapshot(&cli, &updates).await;
                                }
                            })
                            .abort_handle(),
                    );
                    continue;
                }
                Action::CancelJoin => {
                    // Dropping the task kills the CLI (kill_on_drop).
                    if let Some(old) = join.take() {
                        old.abort();
                    }
                    pairing(PairingUpdate::JoinCancelled);
                    continue;
                }
                Action::CopyText { text, what } => {
                    copy(&updates, text, what);
                    continue;
                }
                Action::CopyFingerprint { fingerprint } => {
                    copy(&updates, fingerprint, "Fingerprint");
                    continue;
                }
                Action::DialogFailed(error) => {
                    pairing(PairingUpdate::DialogUnavailable);
                    let _ =
                        updates.send(Update::Notice(format!("Could not open a dialog: {error}")));
                    continue;
                }
                _ => {}
            }
            let reconnect = action == Action::Reconnect;
            if let Some(args) = action.cli_args() {
                let message = match runtime.block_on(invoke(&cli, &args)) {
                    Ok(out) => {
                        match &action {
                            Action::Copy { .. } => "URL copied".to_owned(),
                            Action::Open { .. } => "Opened in browser".to_owned(),
                            Action::StartProxy => "Proxy started".to_owned(),
                            Action::StopProxy => "Proxy stopped; apps keep running".to_owned(),
                            Action::Reconnect => "Discovery refreshed".to_owned(),
                            Action::UnpairConfirmed { name, .. } => {
                                let reached = serde_json::from_slice::<serde_json::Value>(&out)
                                    .ok()
                                    .and_then(|v| v["reached"].as_bool())
                                    .unwrap_or(true);
                                if reached {
                                    format!("Unpaired {name}")
                                } else {
                                    format!("Unpaired {name} here; it was unreachable, so unpair there too")
                                }
                            }
                            Action::StopSharing => "Sharing stopped".to_owned(),
                            Action::StartAtLogin => "Sharing will start at login".to_owned(),
                            Action::StopAtLogin => "Sharing will not start at login".to_owned(),
                            _ => "Updated".to_owned(),
                        }
                    }
                    Err(error) => format!("{error:#}"),
                };
                let _ = updates.send(Update::Notice(message));
            }
            let discover = !reconnect && last_discovery.elapsed() >= discovery;
            refresh(&updates, discover);
            if reconnect || discover {
                last_discovery = std::time::Instant::now();
            }
        }
    });
    (commands, update_rx)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native {
    use super::*;
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
        Icon, TrayIcon, TrayIconBuilder,
    };

    pub struct Ui {
        tray: TrayIcon,
        icon_state: IconState,
        menu: Menu,
        actions: HashMap<String, Action>,
        model: Vec<Entry>,
        snapshot: Option<Snapshot>,
        error: Option<String>,
        notice: Option<String>,
        pairing: PairingView,
        /// Local state changed (e.g. Dismiss); rebuild on the next tick.
        dirty: bool,
    }
    fn icon(state: IconState) -> Result<Icon> {
        let (pixels, size) = state.artwork();
        Ok(Icon::from_rgba(pixels.to_vec(), size, size)?)
    }
    fn fill_menu(menu: &Menu, entries: &[Entry]) -> Result<HashMap<String, Action>> {
        fn append(
            entries: &[Entry],
            append: &dyn Fn(&dyn tray_icon::menu::IsMenuItem) -> tray_icon::menu::Result<()>,
            actions: &mut HashMap<String, Action>,
        ) -> Result<()> {
            for entry in entries {
                if entry.separator {
                    append(&PredefinedMenuItem::separator())?;
                } else if !entry.children.is_empty() {
                    let submenu = Submenu::new(&entry.label, entry.enabled);
                    append(&submenu)?;
                    append_children(&submenu, &entry.children, actions)?;
                } else {
                    let item = MenuItem::new(&entry.label, entry.enabled, None);
                    if let Some(action) = &entry.action {
                        actions.insert(item.id().0.clone(), action.clone());
                    }
                    append(&item)?;
                }
            }
            Ok(())
        }
        fn append_children(
            menu: &Submenu,
            entries: &[Entry],
            actions: &mut HashMap<String, Action>,
        ) -> Result<()> {
            append(entries, &|item| menu.append(item), actions)
        }
        let mut actions = HashMap::new();
        append(entries, &|item| menu.append(item), &mut actions)?;
        Ok(actions)
    }
    impl Ui {
        pub fn new() -> Result<Self> {
            let model = menu_model(None, None, None, &PairingView::default());
            let menu = Menu::new();
            let actions = fill_menu(&menu, &model)?;
            let builder = TrayIconBuilder::new()
                .with_tooltip("Porch — your dev apps")
                .with_menu(Box::new(menu.clone()));
            let icon_state = IconState::from_snapshot(None);
            #[cfg(target_os = "macos")]
            let builder = builder.with_icon_templated(icon(icon_state)?);
            #[cfg(target_os = "linux")]
            let builder = builder.with_icon(icon(icon_state)?);
            let tray = builder.build().context("create native Porch menu")?;
            Ok(Self {
                tray,
                icon_state,
                menu,
                actions,
                model,
                snapshot: None,
                error: None,
                notice: None,
                pairing: PairingView::default(),
                dirty: false,
            })
        }
        pub fn tick(&mut self, updates: &Receiver<Update>, commands: &Sender<Action>) -> bool {
            let mut changed = std::mem::take(&mut self.dirty);
            changed |= self.pairing.expire(Instant::now());
            for update in updates.try_iter() {
                match update {
                    Update::Snapshot(Ok(snapshot)) => {
                        self.snapshot = Some(snapshot);
                        self.error = None;
                    }
                    Update::Snapshot(Err(error)) => self.error = Some(error),
                    Update::Notice(notice) => self.notice = Some(notice),
                    Update::Pairing(update) => self.pairing.apply(update, Instant::now()),
                }
                changed = true;
            }
            if changed {
                let icon_state = IconState::from_snapshot(self.snapshot.as_ref());
                if icon_state != self.icon_state {
                    #[cfg(target_os = "macos")]
                    let result = icon(icon_state)
                        .and_then(|icon| Ok(self.tray.set_icon_templated(Some(icon))?));
                    #[cfg(target_os = "linux")]
                    let result =
                        icon(icon_state).and_then(|icon| Ok(self.tray.set_icon(Some(icon))?));
                    match result {
                        Ok(()) => self.icon_state = icon_state,
                        Err(error) => eprintln!("Porch: update icon: {error:#}"),
                    }
                }
                let model = menu_model(
                    self.snapshot.as_ref(),
                    self.error.as_deref(),
                    self.notice.as_deref(),
                    &self.pairing,
                );
                if model != self.model {
                    // AppIndicator keeps the initial menu object. Mutate its contents
                    // instead of replacing it so refresh works on Linux as on macOS.
                    while self.menu.remove_at(0).is_some() {}
                    match fill_menu(&self.menu, &model) {
                        Ok(actions) => {
                            self.actions = actions;
                            self.model = model;
                        }
                        Err(error) => eprintln!("Porch: update menu: {error:#}"),
                    }
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if let Some(action) = self.actions.get(&event.id.0).cloned() {
                    if action == Action::Quit {
                        let _ = commands.send(Action::Quit);
                        return false;
                    }
                    if action.is_ui() {
                        self.ui_action(action, commands);
                    } else {
                        let _ = commands.send(action);
                    }
                }
            }
            true
        }
        fn ui_action(&mut self, action: Action, commands: &Sender<Action>) {
            match action {
                Action::DismissHost => {
                    self.pairing
                        .apply(PairingUpdate::DismissHost, Instant::now());
                    self.dirty = true;
                }
                Action::DismissJoin => {
                    self.pairing
                        .apply(PairingUpdate::DismissJoin, Instant::now());
                    self.dirty = true;
                }
                Action::EnterCode {
                    host,
                    name,
                    prefill,
                } => dialogs::flow(commands, move || dialogs::code(host, name, prefill)),
                Action::EnterCodeWithAddress { prefill } => {
                    dialogs::flow(commands, move || dialogs::address(prefill))
                }
                Action::Unpair { peer, name } => {
                    dialogs::flow(commands, move || dialogs::unpair(peer, name))
                }
                other => {
                    let _ = commands.send(other);
                }
            }
        }
    }
    /// Code-entry and confirmation dialogs. macOS uses `osascript` (on a
    /// thread, so the menu stays live); Linux uses GTK on the main thread.
    mod dialogs {
        use super::super::{pairing::parse_code, Action, JoinAttempt};
        use anyhow::Result;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc::Sender,
        };
        const TITLE: &str = "Pair with another machine";
        static OPEN: AtomicBool = AtomicBool::new(false);

        /// Run a dialog flow and send the action it produces to the worker.
        pub fn flow(
            commands: &Sender<Action>,
            f: impl FnOnce() -> Result<Option<Action>> + Send + 'static,
        ) {
            if OPEN.swap(true, Ordering::SeqCst) {
                return;
            }
            let commands = commands.clone();
            let run = move || {
                let action = match f() {
                    Ok(action) => action,
                    Err(error) => Some(Action::DialogFailed(format!("{error:#}"))),
                };
                OPEN.store(false, Ordering::SeqCst);
                if let Some(action) = action {
                    let _ = commands.send(action);
                }
            };
            #[cfg(target_os = "macos")]
            std::thread::spawn(run);
            #[cfg(target_os = "linux")]
            run();
        }
        pub fn code(
            host: Option<String>,
            name: Option<String>,
            prefill: String,
        ) -> Result<Option<Action>> {
            let target = name.clone().or_else(|| host.clone());
            let base = format!(
                "Enter the 6-digit code shown on {}. Both machines will then be able to open each other's registered development apps.",
                target.as_deref().unwrap_or("the other machine")
            );
            let (mut message, mut text) = (base.clone(), prefill);
            loop {
                let Some(input) = platform::ask(TITLE, &message, &text, "Pair")? else {
                    return Ok(None);
                };
                if let Some(code) = parse_code(&input) {
                    return Ok(Some(Action::Join(JoinAttempt { code, host, name })));
                }
                message = format!("That isn't a 6-digit code. {base}");
                text = input;
            }
        }
        pub fn address(prefill: String) -> Result<Option<Action>> {
            let mut message = "Enter the other machine's address: its name, IP, or IP:port.";
            let mut text = String::new();
            loop {
                let Some(input) = platform::ask(TITLE, message, &text, "Next")? else {
                    return Ok(None);
                };
                let input = input.trim().to_owned();
                if !input.is_empty() && !input.contains(char::is_whitespace) {
                    return code(Some(input), None, prefill);
                }
                message = "Enter one address, like 192.168.1.20 or mini.local.";
                text = input;
            }
        }
        pub fn unpair(peer: String, name: String) -> Result<Option<Action>> {
            let confirmed = platform::confirm(
                &format!("Unpair {name}?"),
                &format!("{name} will no longer be able to open this machine's apps, and its apps leave this menu. Pair again any time."),
                "Unpair",
            )?;
            Ok(confirmed.then_some(Action::UnpairConfirmed { peer, name }))
        }

        #[cfg(target_os = "macos")]
        mod platform {
            use anyhow::{bail, Context, Result};
            /// Text goes in as arguments, never into the script source.
            fn osascript(lines: &[&str], args: &[&str]) -> Result<Option<String>> {
                let mut command = std::process::Command::new("/usr/bin/osascript");
                for line in lines {
                    command.arg("-e").arg(line);
                }
                let output = command
                    .args(args)
                    .output()
                    .context("osascript is unavailable")?;
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    return Ok(Some(
                        text.trim_end_matches(['\n', '\r'])
                            .chars()
                            .take(200)
                            .collect(),
                    ));
                }
                let error = String::from_utf8_lossy(&output.stderr);
                if error.contains("-128") {
                    return Ok(None); // Cancel
                }
                bail!("{}", error.trim())
            }
            pub fn ask(
                title: &str,
                message: &str,
                prefill: &str,
                button: &str,
            ) -> Result<Option<String>> {
                osascript(
                    &[
                        "on run argv",
                        "activate",
                        "set r to display dialog (item 2 of argv) default answer (item 3 of argv) with title (item 1 of argv) buttons {\"Cancel\", (item 4 of argv)} default button 2 cancel button 1",
                        "return text returned of r",
                        "end run",
                    ],
                    &[title, message, prefill, button],
                )
            }
            pub fn confirm(title: &str, message: &str, button: &str) -> Result<bool> {
                Ok(osascript(
                    &[
                        "on run argv",
                        "activate",
                        "display dialog (item 2 of argv) with title (item 1 of argv) buttons {\"Cancel\", (item 3 of argv)} default button 1 cancel button 1 with icon caution",
                        "return \"ok\"",
                        "end run",
                    ],
                    &[title, message, button],
                )?
                .is_some())
            }
        }
        #[cfg(target_os = "linux")]
        mod platform {
            use anyhow::Result;
            use gtk::prelude::*;
            fn dialog(
                title: &str,
                message: &str,
                button: &str,
                entry: Option<&str>,
            ) -> (bool, Option<String>) {
                let dialog = gtk::Dialog::with_buttons(
                    Some(title),
                    None::<&gtk::Window>,
                    gtk::DialogFlags::MODAL,
                    &[
                        ("Cancel", gtk::ResponseType::Cancel),
                        (button, gtk::ResponseType::Accept),
                    ],
                );
                dialog.set_default_response(if entry.is_some() {
                    gtk::ResponseType::Accept
                } else {
                    gtk::ResponseType::Cancel
                });
                dialog.set_keep_above(true);
                let area = dialog.content_area();
                area.set_spacing(10);
                area.set_border_width(12);
                let label = gtk::Label::new(Some(message));
                label.set_line_wrap(true);
                label.set_max_width_chars(48);
                label.set_xalign(0.0);
                area.pack_start(&label, false, false, 0);
                let field = entry.map(|prefill| {
                    let field = gtk::Entry::new();
                    field.set_text(prefill);
                    field.set_max_length(200);
                    field.set_activates_default(true);
                    area.pack_start(&field, false, false, 0);
                    field
                });
                dialog.show_all();
                let accepted = dialog.run() == gtk::ResponseType::Accept;
                let text = field.map(|f| f.text().to_string());
                dialog.hide();
                // SAFETY: the dialog and its children are not used after this.
                unsafe { dialog.destroy() };
                (accepted, text)
            }
            pub fn ask(
                title: &str,
                message: &str,
                prefill: &str,
                button: &str,
            ) -> Result<Option<String>> {
                let (accepted, text) = dialog(title, message, button, Some(prefill));
                Ok(if accepted { text } else { None })
            }
            pub fn confirm(title: &str, message: &str, button: &str) -> Result<bool> {
                Ok(dialog(title, message, button, None).0)
            }
        }
    }
    #[cfg(target_os = "macos")]
    pub fn run(cli: PathBuf) -> Result<()> {
        use tao::{
            event::{Event, StartCause},
            event_loop::{ControlFlow, EventLoopBuilder},
            platform::macos::{ActivationPolicy, EventLoopExtMacOS},
        };
        let mut event_loop = EventLoopBuilder::new().build();
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
        let (commands, updates) = worker(cli);
        let mut ui: Option<Ui> = None;
        event_loop.run(move |event, _, control_flow| {
            *control_flow =
                ControlFlow::WaitUntil(std::time::Instant::now() + Duration::from_millis(150));
            if matches!(event, Event::NewEvents(StartCause::Init)) {
                match Ui::new() {
                    Ok(native) => ui = Some(native),
                    Err(error) => {
                        eprintln!("Porch: {error:#}");
                        *control_flow = ControlFlow::Exit;
                    }
                }
            }
            if let Some(ui) = &mut ui {
                if !ui.tick(&updates, &commands) {
                    *control_flow = ControlFlow::Exit;
                }
            }
        });
    }
    #[cfg(target_os = "linux")]
    pub fn run(cli: PathBuf) -> Result<()> {
        use gtk::glib::{self, variant::ToVariant};
        gtk::init()
            .context("No graphical desktop is available. Use `porch list --all` in this session")?;
        let connection =
            gtk::gio::bus_get_sync(gtk::gio::BusType::Session, gtk::gio::Cancellable::NONE)
                .context("No desktop session bus is available. Use `porch list --all`")?;
        let mut watcher = false;
        for name in [
            "org.kde.StatusNotifierWatcher",
            "org.freedesktop.StatusNotifierWatcher",
        ] {
            if let Ok(reply) = connection.call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameHasOwner",
                Some(&(name,).to_variant()),
                None,
                gtk::gio::DBusCallFlags::NONE,
                2000,
                gtk::gio::Cancellable::NONE,
            ) {
                watcher |= reply.get::<(bool,)>().is_some_and(|(present,)| present);
            }
        }
        if !watcher {
            bail!("This desktop has no system tray host. On GNOME enable an AppIndicator extension, or use `porch list --all`");
        }
        // The AppIndicator crate loads its backend dynamically and panics when
        // missing. Probe it first so an optional desktop dependency fails with
        // the same actionable CLI fallback as an absent tray host.
        let indicator_library = [
            "libayatana-appindicator3.so.1",
            "libappindicator3.so.1",
            "libayatana-appindicator3.so",
            "libappindicator3.so",
        ]
        .iter()
        .any(|name| {
            let name = std::ffi::CString::new(*name).expect("static library name");
            // SAFETY: a valid NUL-terminated name, and we release only handles
            // returned successfully by dlopen. No library symbols are called.
            unsafe {
                let handle = libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
                if handle.is_null() {
                    false
                } else {
                    libc::dlclose(handle);
                    true
                }
            }
        });
        if !indicator_library {
            bail!("AppIndicator is unavailable. Install libayatana-appindicator3-1, or use `porch list --all`");
        }
        let (commands, updates) = worker(cli);
        let mut ui = Ui::new()?;
        glib::timeout_add_local(Duration::from_millis(150), move || {
            if ui.tick(&updates, &commands) {
                glib::ControlFlow::Continue
            } else {
                gtk::main_quit();
                glib::ControlFlow::Break
            }
        });
        gtk::main();
        Ok(())
    }
}

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("Porch menu bar / tray {}\n\nUsage: porch-tray [--state-dir DIR]\n\nLists local and paired dev apps. Open, copy, refresh, and control the proxy.\nQuit closes only the menu. Sharing and login startup are never enabled automatically.\nUse `porch-login enable|disable` for explicit login startup.", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--version") {
        println!("porch-tray {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    match args.as_slice() {
        [] => {}
        [flag, dir] if flag == "--state-dir" => std::env::set_var("PORCH_STATE_DIR", dir),
        _ => bail!("usage: porch-tray [--state-dir DIR]"),
    }
    let cli = cli_path()?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return native::run(cli);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = cli;
        bail!("Porch tray supports macOS and Linux; use the headless Porch CLI on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::types::{Catalog, PeerCatalog, PROTOCOL_VERSION};
    fn app(remote: bool) -> App {
        App {
            id: "app-stable-id".into(),
            name: "web".into(),
            local_url: "http://web.localhost:7462".into(),
            health: Health::Healthy,
            remote_access: remote,
            unavailable_reason: None,
        }
    }
    fn catalog(remote: bool) -> Catalog {
        Catalog {
            version: PROTOCOL_VERSION,
            machine: Machine {
                id: "local".into(),
                name: "Laptop".into(),
            },
            apps: vec![app(remote)],
            proxy_running: true,
            generated_at: 1,
        }
    }
    fn snapshot() -> Snapshot {
        Snapshot {
            version: PROTOCOL_VERSION,
            local: catalog(false),
            peers: vec![PeerCatalog {
                machine: Machine {
                    id: "peer-stable-id".into(),
                    name: "Workbench".into(),
                },
                slug: "workbench".into(),
                endpoint: "https://192.168.1.9:7463".into(),
                sources: vec![Source::Lan, Source::Tailscale],
                last_seen: 1,
                catalog: Some(catalog(true)),
                error: None,
                fingerprint: "ab12".repeat(16),
            }],
            candidates: vec![],
            warnings: vec![],
            sharing: None,
        }
    }
    #[test]
    fn remote_actions_target_stable_machine_and_app_not_remote_localhost() {
        let entries = menu_model(Some(&snapshot()), None, None, &PairingView::default());
        let remote = entries
            .iter()
            .find(|entry| entry.label == "Workbench")
            .unwrap();
        let action = remote.children[1].children[0].action.as_ref().unwrap();
        assert_eq!(
            action.cli_args().unwrap(),
            vec!["open", "app-stable-id", "--peer", "peer-stable-id"]
        );
        assert!(remote.children[1].children[0].enabled);
        let local = &entries[2].children[1];
        assert_eq!(
            local.children[0]
                .action
                .as_ref()
                .unwrap()
                .cli_args()
                .unwrap(),
            vec!["open", "app-stable-id"]
        );
        assert!(
            local.children[0].enabled,
            "local app stays usable when remote sharing is off"
        );
    }
    #[test]
    fn unavailable_remote_app_disables_open_and_copy() {
        let mut state = snapshot();
        state.peers[0].catalog.as_mut().unwrap().apps[0].remote_access = false;
        let entries = menu_model(Some(&state), None, None, &PairingView::default());
        let remote = entries
            .iter()
            .find(|entry| entry.label == "Workbench")
            .unwrap();
        assert!(!remote.children[1].children[0].enabled);
        assert!(!remote.children[1].children[1].enabled);
        assert!(remote.children[1]
            .children
            .iter()
            .any(|entry| entry.label.contains("sharing is off")));
    }
    #[test]
    fn icon_reflects_proxy_and_reachable_peers() {
        let state = |running, peers| IconState {
            proxy_running: running,
            peers,
        };
        assert_eq!(IconState::from_snapshot(None), state(false, false));
        let mut snap = snapshot();
        assert_eq!(IconState::from_snapshot(Some(&snap)), state(true, true));
        snap.local.proxy_running = false;
        assert_eq!(IconState::from_snapshot(Some(&snap)), state(false, true));
        // A paired machine that cannot be reached does not earn the badge.
        snap.peers[0].error = Some("timed out".into());
        assert_eq!(IconState::from_snapshot(Some(&snap)), state(false, false));
        snap.peers[0].error = None;
        snap.peers[0].catalog = None;
        assert_eq!(IconState::from_snapshot(Some(&snap)), state(false, false));
    }
    #[test]
    fn every_icon_state_has_distinct_square_rgba_artwork() {
        let mut seen = Vec::new();
        for running in [true, false] {
            for peers in [true, false] {
                let (pixels, size) = IconState {
                    proxy_running: running,
                    peers,
                }
                .artwork();
                assert_eq!(pixels.len(), (size * size * 4) as usize);
                assert!(
                    pixels.chunks(4).any(|px| px[3] == 255),
                    "mark is opaque somewhere"
                );
                assert!(!seen.contains(&pixels), "states must look different");
                seen.push(pixels);
            }
        }
    }
    #[test]
    fn stopped_proxy_empty_error_and_quit_are_clear() {
        let mut state = snapshot();
        state.local.proxy_running = false;
        state.local.apps.clear();
        state.peers.clear();
        let entries = menu_model(
            Some(&state),
            Some("peer timed out"),
            None,
            &PairingView::default(),
        );
        assert!(entries
            .iter()
            .any(|entry| entry.label.contains("peer timed out")));
        assert!(entries
            .iter()
            .any(|entry| entry.action == Some(Action::StartProxy)));
        assert!(entries[3]
            .children
            .iter()
            .any(|entry| entry.label.contains("No apps yet")));
        assert_eq!(entries.last().unwrap().action, Some(Action::Quit));
        assert_eq!(Action::Quit.cli_args(), None);
        assert_eq!(Action::StopProxy.cli_args().unwrap(), vec!["proxy", "stop"]);
    }
    #[cfg(unix)]
    #[test]
    fn worker_discovers_initially_refreshes_and_dispatches_real_cli_arguments() {
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            time::{SystemTime, UNIX_EPOCH},
        };
        let directory = std::env::temp_dir().join(format!(
            "porch-tray-worker-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let cli = directory.join("porch");
        let log = directory.join("calls");
        let quoted_log = format!("'{}'", log.display().to_string().replace('\'', "'\"'\"'"));
        let json = serde_json::to_string(&snapshot()).unwrap();
        fs::write(&cli, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {quoted_log}\ncat <<'PORCH_SNAPSHOT'\n{json}\nPORCH_SNAPSHOT\n")).unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let (commands, updates) = worker_with_intervals(
            cli,
            Duration::from_millis(30),
            Duration::from_millis(50),
            None,
        );
        assert!(matches!(
            updates.recv_timeout(Duration::from_secs(5)).unwrap(),
            Update::Snapshot(Ok(_))
        ));
        commands
            .send(Action::Open {
                app: "app-stable-id".into(),
                peer: Some("peer-stable-id".into()),
            })
            .unwrap();
        let mut found_scan = false;
        for _ in 0..10 {
            let _ = updates.recv_timeout(Duration::from_secs(5)).unwrap();
            let calls = fs::read_to_string(&log).unwrap();
            if calls
                .lines()
                .filter(|line| *line == "peers refresh --json")
                .count()
                >= 2
            {
                found_scan = true;
                break;
            }
        }
        commands.send(Action::Quit).unwrap();
        while updates.recv_timeout(Duration::from_secs(5)).is_ok() {}
        let calls = fs::read_to_string(&log).unwrap();
        assert!(
            calls.starts_with("peers refresh --json\n"),
            "initial discovery: {calls}"
        );
        assert!(
            calls.contains("open app-stable-id --peer peer-stable-id\n"),
            "remote open: {calls}"
        );
        assert!(
            calls.contains("list --all --json\n"),
            "catalog refresh: {calls}"
        );
        assert!(found_scan, "periodic discovery: {calls}");
        assert!(!calls.contains("proxy stop"), "quit must not stop proxy");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pair_a_device_is_a_submenu_and_peers_show_fingerprint_and_unpair() {
        let entries = menu_model(Some(&snapshot()), None, None, &PairingView::default());
        let pair = entries
            .iter()
            .find(|e| e.label == "Pair a device")
            .expect("Pair a device submenu");
        let labels: Vec<_> = pair.children.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Show a pairing code",
                "Enter a code…",
                "Enter a code with an address…"
            ]
        );
        assert!(pair.children.iter().all(|e| e.enabled));
        assert_eq!(
            Action::ShowCode { replace: None }.cli_args().unwrap(),
            vec!["pair", "--no-wait", "--json"]
        );
        assert_eq!(
            Action::Join(JoinAttempt {
                code: "012345".into(),
                host: Some("10.0.0.2:7463".into()),
                name: None
            })
            .cli_args()
            .unwrap(),
            vec!["pair", "012345", "--host", "10.0.0.2:7463", "--json"]
        );
        assert!(Action::EnterCode {
            host: None,
            name: None,
            prefill: String::new()
        }
        .is_ui());
        let remote = entries.iter().find(|e| e.label == "Workbench").unwrap();
        assert!(remote
            .children
            .iter()
            .any(|e| e.label == "Fingerprint ab12…12"));
        let unpair = remote.children.last().unwrap();
        assert_eq!(unpair.label, "Unpair…");
        assert_eq!(
            Action::UnpairConfirmed {
                peer: "peer-stable-id".into(),
                name: "Workbench".into()
            }
            .cli_args()
            .unwrap(),
            vec!["unpair", "peer-stable-id", "--json"]
        );
    }
    #[test]
    fn sharing_submenu_reflects_agent_and_login_startup() {
        let mut state = snapshot();
        let find = |state: &Snapshot| {
            menu_model(Some(state), None, None, &PairingView::default())
                .into_iter()
                .find(|e| e.label == "Sharing")
        };
        assert!(find(&state).is_none(), "older CLI: no sharing info");
        state.sharing = Some(crate::discovery::types::SharingState::default());
        assert!(find(&state).is_none(), "never shared: nothing to stop");
        state.sharing = Some(crate::discovery::types::SharingState {
            running: true,
            share_apps: true,
            autostart: false,
        });
        let sharing = find(&state).unwrap();
        assert_eq!(sharing.children[1].action, Some(Action::StopSharing));
        assert_eq!(sharing.children[2].action, Some(Action::StartAtLogin));
        assert_eq!(
            Action::StopSharing.cli_args().unwrap(),
            vec!["discovery", "disable"]
        );
    }
    #[test]
    fn code_lines_sit_at_the_top_of_the_menu() {
        let mut view = PairingView::default();
        view.apply(
            PairingUpdate::Shown(PairShown {
                version: PROTOCOL_VERSION,
                id: "one".into(),
                code: "482913".into(),
                expires: 1_900_000_000,
                expires_in: 300,
                machine: Machine {
                    id: "a".into(),
                    name: "A".into(),
                },
                endpoint: "https://192.168.1.20:7463/".into(),
            }),
            Instant::now(),
        );
        let entries = menu_model(Some(&snapshot()), None, None, &view);
        assert!(entries[1].label.starts_with("Pairing code 482 913"));
        let pair = entries.iter().find(|e| e.label == "Pair a device").unwrap();
        assert!(!pair.children[0].enabled, "one code at a time");
    }
    #[cfg(unix)]
    #[test]
    fn worker_shows_polls_restores_and_joins_in_the_background() {
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            time::{SystemTime, UNIX_EPOCH},
        };
        let directory = std::env::temp_dir().join(format!(
            "porch-tray-pairing-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let cli = directory.join("porch");
        let file = directory.join("tray-pairing.json");
        let expires = crate::discovery::server::now() + 300;
        // A code the previous tray process showed is still waiting.
        fs::write(&file, format!(r#"{{"id":"earlier","expires":{expires}}}"#)).unwrap();
        let snapshot = serde_json::to_string(&snapshot()).unwrap();
        fs::write(
            &cli,
            format!(
                r#"#!/bin/sh
case "$*" in
  "pair --no-wait --json") echo '{{"version":1,"id":"new","code":"482913","expires":{expires},"expires_in":300,"machine":{{"id":"a","name":"A"}},"endpoint":"https://192.168.1.20:7463/"}}' ;;
  "pair cancel earlier --json") echo '{{"version":1,"id":"earlier","state":"cancelled","reason":"user"}}' ;;
  "pair status earlier --json") echo '{{"version":1,"id":"earlier","state":"waiting","expires":{expires},"expires_in":200}}' ;;
  "pair status new --json") echo '{{"version":1,"id":"new","state":"paired","peer":{{"id":"m","name":"Mini"}}}}' ;;
  "pair 012345 --host 10.0.0.2:7463 --json") sleep 0.3; echo '{{"version":1,"error":{{"code":"wrong_code","message":"That code didn'"'"'t work. Check the code on the other machine.","retry":true,"candidates":[]}}}}'; echo "porch: wrong" >&2; exit 1 ;;
  *) cat <<'PORCH_SNAPSHOT'
{snapshot}
PORCH_SNAPSHOT
  ;;
esac
"#
            ),
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let (commands, updates) = worker_with_intervals(
            cli,
            Duration::from_secs(60),
            Duration::from_secs(60),
            Some(file.clone()),
        );
        let mut view = PairingView::default();
        let mut until = |done: &dyn Fn(&PairingView) -> bool| {
            while !done(&view) {
                if let Update::Pairing(update) =
                    updates.recv_timeout(Duration::from_secs(10)).unwrap()
                {
                    view.apply(update, Instant::now());
                }
            }
            view.clone()
        };
        let restored = until(&|v| v.active_id().is_some());
        assert_eq!(restored.active_id(), Some("earlier"));
        assert!(matches!(
            restored.host,
            pairing::HostView::Showing { code: None, .. }
        ));
        // Join runs on its own task: the worker shows a code while it waits.
        commands
            .send(Action::Join(JoinAttempt {
                code: "012345".into(),
                host: Some("10.0.0.2:7463".into()),
                name: Some("Mini".into()),
            }))
            .unwrap();
        commands
            .send(Action::ShowCode {
                replace: Some("earlier".into()),
            })
            .unwrap();
        let done = until(&|v| {
            matches!(v.host, pairing::HostView::Paired { .. })
                && matches!(v.join, pairing::JoinView::Failed { .. })
        });
        match &done.join {
            pairing::JoinView::Failed { failure, attempt } => {
                assert_eq!(failure.code, "wrong_code");
                assert_eq!(attempt.code, "012345");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&done.host, pairing::HostView::Paired { peer, .. } if peer == "Mini"),
            "{:?}",
            done.host
        );
        commands.send(Action::Quit).unwrap();
        // The finished code is forgotten.
        for _ in 0..50 {
            if !file.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!file.exists());
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn labels_do_not_inject_shortcuts_or_control_characters() {
        assert_eq!(clean_label("A&B\nC\t"), "A&&BC");
        assert!(clean_label(&"x".repeat(500)).len() <= 140);
    }
}
