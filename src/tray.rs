//! Porch's optional native menu. All network/lifecycle work goes through the CLI
//! on a Tokio worker; native UI objects never leave the OS main thread.
use crate::discovery::types::{App, Health, Snapshot, Source};
use anyhow::{bail, Context, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
};
use tokio::io::AsyncReadExt;

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
    /// Show a pairing code (host side of `porch pair`) in the menu.
    Pair,
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
            Self::Pair => vec!["pair".into(), "--no-wait".into(), "--json".into()],
            Self::Refresh | Self::Quit => return None,
        };
        Some(args)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub label: String,
    pub enabled: bool,
    pub action: Option<Action>,
    pub children: Vec<Entry>,
    pub separator: bool,
    /// Draw this entry as an on/off switch in the given state. macOS shows a
    /// native switch beside the label; Linux shows a check item.
    pub toggle: Option<bool>,
}
impl Entry {
    fn text(label: impl Into<String>) -> Self {
        Self {
            label: clean_label(&label.into()),
            enabled: false,
            action: None,
            children: vec![],
            separator: false,
            toggle: None,
        }
    }
    fn action(label: impl Into<String>, action: Action, enabled: bool) -> Self {
        Self {
            label: label.into(),
            enabled,
            action: Some(action),
            children: vec![],
            separator: false,
            toggle: None,
        }
    }
    fn group(label: impl Into<String>, children: Vec<Entry>) -> Self {
        Self {
            label: clean_label(&label.into()),
            enabled: true,
            action: None,
            children,
            separator: false,
            toggle: None,
        }
    }
    fn separator() -> Self {
        Self {
            label: String::new(),
            enabled: false,
            action: None,
            children: vec![],
            separator: true,
            toggle: None,
        }
    }
    /// The header row: "Porch" with a switch that starts or stops the proxy.
    /// Until the first refresh the state is unknown, so the switch is off and
    /// disabled rather than guessing.
    fn proxy_toggle(running: Option<bool>) -> Self {
        let on = running == Some(true);
        Self {
            label: "Porch".into(),
            enabled: running.is_some(),
            action: Some(if on {
                Action::StopProxy
            } else {
                Action::StartProxy
            }),
            children: vec![],
            separator: false,
            toggle: Some(on),
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
        // The macOS template art is a dark mark: right for light Windows
        // taskbars too. Dark taskbars and Linux panels get the light mark.
        if cfg!(target_os = "macos") || (cfg!(windows) && light_taskbar()) {
            (art!("macos", "36"), 36)
        } else {
            (art!("linux", "32"), 32)
        }
    }
}

/// Whether the Windows taskbar uses the light theme. Read once: a theme
/// change takes effect when the menu restarts.
fn light_taskbar() -> bool {
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        static LIGHT: OnceLock<bool> = OnceLock::new();
        *LIGHT.get_or_init(|| {
            use windows_sys::Win32::System::Registry::{
                RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
            };
            let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
            let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
            let value = wide("SystemUsesLightTheme");
            let mut data = 0u32;
            let mut size = std::mem::size_of::<u32>() as u32;
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    value.as_ptr(),
                    RRF_RT_REG_DWORD,
                    std::ptr::null_mut(),
                    (&mut data as *mut u32).cast(),
                    &mut size,
                )
            };
            // Missing value: Windows 10 before 1903, whose taskbar is dark.
            status == 0 && data == 1
        })
    }
    #[cfg(not(windows))]
    false
}

pub fn menu_model(
    snapshot: Option<&Snapshot>,
    error: Option<&str>,
    notice: Option<&str>,
) -> Vec<Entry> {
    let mut entries = vec![Entry::proxy_toggle(
        snapshot.map(|snapshot| snapshot.local.proxy_running),
    )];
    if let Some(notice) = notice {
        entries.push(Entry::text(notice));
    }
    if let Some(error) = error {
        entries.push(Entry::text(format!("Could not refresh: {error}")));
    }
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
            entries.push(Entry::text("No paired machines yet; choose Pair a device…"));
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
            entries.push(Entry::group(peer.machine.name.clone(), apps));
        }
        if !snapshot.candidates.is_empty() {
            let mut found = vec![Entry::text(
                "Choose Pair a device… on one machine, then run porch pair CODE on the other",
            )];
            for candidate in snapshot.candidates.iter().take(30) {
                found.push(Entry::text(format!(
                    "{}: {}",
                    candidate.name, candidate.endpoint
                )));
            }
            entries.push(Entry::group("Found nearby (not paired)", found));
        }
        for warning in snapshot.warnings.iter().take(3) {
            entries.push(Entry::text(warning));
        }
        entries.push(Entry::separator());
    } else {
        entries.push(Entry::text(if error.is_some() {
            "Check the porch CLI and state directory"
        } else {
            "Loading apps…"
        }));
    }
    entries.extend([
        Entry::action("Pair a device…", Action::Pair, true),
        Entry::action("Refresh apps", Action::Refresh, true),
        Entry::action("Reconnect discovery", Action::Reconnect, true),
        Entry::separator(),
        Entry::action("Quit Porch menu", Action::Quit, true),
    ]);
    entries
}

pub fn cli_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PORCH_BIN") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            bail!("PORCH_BIN does not name a Porch CLI executable");
        }
        return Ok(path);
    }
    let cli = format!("porch{}", std::env::consts::EXE_SUFFIX);
    let current = std::env::current_exe()?;
    if let Some(parent) = current.parent() {
        let sibling = parent.join(&cli);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    let installed = if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA")
            .context("LOCALAPPDATA is unavailable; set PORCH_BIN to the CLI")?;
        PathBuf::from(base).join(r"Programs\Porch\bin").join(&cli)
    } else {
        let home =
            std::env::var_os("HOME").context("HOME is unavailable; set PORCH_BIN to the CLI")?;
        PathBuf::from(home).join(".local/bin/porch")
    };
    if installed.is_file() {
        return Ok(installed);
    }
    bail!("Porch CLI not found. Install porch beside porch-tray or set PORCH_BIN")
}
async fn invoke(cli: &Path, args: &[String]) -> Result<Vec<u8>> {
    let mut command = tokio::process::Command::new(cli);
    // The menu has no console, so each CLI call would open its own window.
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let mut child = command
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
    let result = tokio::time::timeout(Duration::from_secs(35), async {
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
        if !status.success() {
            let message = String::from_utf8_lossy(&err)
                .lines()
                .next()
                .unwrap_or("Porch command failed")
                .to_owned();
            bail!("{}", message);
        }
        Ok(out)
    })
    .await
    .context("Porch command timed out; try it in a terminal")?;
    result
}

/// The code stays valid in the agent after the CLI returns; show it in the menu.
fn pairing_notice(out: &[u8]) -> String {
    let code = serde_json::from_slice::<serde_json::Value>(out)
        .ok()
        .and_then(|v| v["code"].as_str().map(str::to_owned))
        .filter(|c| c.len() == 6 && c.bytes().all(|b| b.is_ascii_digit()));
    match code {
        Some(code) => format!(
            "Pairing code {code} — on the other machine run: porch pair {code} (valid 5 minutes)"
        ),
        None => "Porch did not return a pairing code".into(),
    }
}
enum Update {
    Snapshot(Result<Snapshot, String>),
    Notice(String),
    /// The proxy reached (or failed to reach) the state the switch asked for,
    /// settling `flips` clicks at once.
    ProxyDone {
        flips: usize,
        error: Option<String>,
    },
}
fn worker(cli: PathBuf) -> (Sender<Action>, Receiver<Update>) {
    worker_with_intervals(cli, Duration::from_secs(15), Duration::from_secs(60))
}
fn worker_with_intervals(
    cli: PathBuf,
    poll: Duration,
    discovery: Duration,
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
        let fetch = |discover: bool| {
            runtime
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
                .map_err(|error| format!("{error:#}"))
        };
        let refresh = |updates: &Sender<Update>, discover: bool| {
            let _ = updates.send(Update::Snapshot(fetch(discover)));
        };
        // Commands read while gathering queued switch flips wait here.
        let mut backlog = std::collections::VecDeque::new();
        // Take every queued flip, keeping the last wanted state and how many
        // flips it settles; anything else waits its turn in the backlog.
        let gather = |backlog: &mut std::collections::VecDeque<Action>| {
            let mut latest = None;
            let mut flips = 0;
            for queued in command_rx.try_iter() {
                match queued {
                    Action::StartProxy => (latest, flips) = (Some(true), flips + 1),
                    Action::StopProxy => (latest, flips) = (Some(false), flips + 1),
                    other => backlog.push_back(other),
                }
            }
            (latest, flips)
        };
        refresh(&updates, true);
        let mut last_discovery = std::time::Instant::now();
        loop {
            let next = match backlog.pop_front() {
                Some(action) => Ok(action),
                None => command_rx.recv_timeout(poll),
            };
            match next {
                Ok(Action::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(action @ (Action::StartProxy | Action::StopProxy)) => {
                    // The switch says what state the user wants, not a list of
                    // commands. Fold rapid flips into the last one, skip work
                    // when the proxy is already there, and pick up flips that
                    // arrive while a start or stop runs.
                    let mut want = action == Action::StartProxy;
                    let mut flips = 1;
                    let mut error;
                    loop {
                        let (latest, more) = gather(&mut backlog);
                        if let Some(latest) = latest {
                            want = latest;
                        }
                        flips += more;
                        let running = fetch(false).ok().map(|s| s.local.proxy_running);
                        error = if running == Some(want) {
                            None
                        } else {
                            let action = if want {
                                Action::StartProxy
                            } else {
                                Action::StopProxy
                            };
                            let args = action.cli_args().expect("proxy actions run the CLI");
                            runtime
                                .block_on(invoke(&cli, &args))
                                .err()
                                .map(|error| format!("{error:#}"))
                        };
                        let (latest, more) = gather(&mut backlog);
                        if more == 0 {
                            break;
                        }
                        want = latest.unwrap_or(want);
                        flips += more;
                    }
                    // Send the fresh state first, then settle the switch, so it
                    // never redraws from a catalog taken before the change.
                    refresh(&updates, false);
                    let _ = updates.send(Update::ProxyDone { flips, error });
                }
                Ok(action) => {
                    let reconnect = action == Action::Reconnect;
                    if let Some(args) = action.cli_args() {
                        let result = runtime.block_on(invoke(&cli, &args));
                        let message = match result {
                            Ok(out) if action == Action::Pair => pairing_notice(&out),
                            Ok(_) => match action {
                                Action::Copy { .. } => "URL copied".to_owned(),
                                Action::Open { .. } => "Opened in browser".to_owned(),
                                Action::Reconnect => "Discovery refreshed".to_owned(),
                                _ => "Updated".to_owned(),
                            },
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
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let discover = last_discovery.elapsed() >= discovery;
                    refresh(&updates, discover);
                    if discover {
                        last_discovery = std::time::Instant::now();
                    }
                }
            }
        }
    });
    (commands, update_rx)
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
mod native {
    use super::*;
    #[cfg(not(target_os = "macos"))]
    use tray_icon::menu::CheckMenuItem;
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
        Icon, TrayIcon, TrayIconBuilder,
    };

    /// The live control behind a toggle row, kept so its state can be read at
    /// click time and updated in place without rebuilding the menu.
    #[cfg(target_os = "macos")]
    type Toggle = mac::SwitchRow;
    #[cfg(not(target_os = "macos"))]
    type Toggle = CheckMenuItem;

    fn toggle_is_on(toggle: &Toggle) -> bool {
        #[cfg(target_os = "macos")]
        return toggle.is_on();
        #[cfg(not(target_os = "macos"))]
        return toggle.is_checked();
    }
    fn set_toggle(toggle: &Toggle, on: bool, enabled: bool) {
        #[cfg(target_os = "macos")]
        toggle.set(on, enabled);
        #[cfg(not(target_os = "macos"))]
        {
            if toggle.is_checked() != on {
                toggle.set_checked(on);
            }
            if toggle.is_enabled() != enabled {
                toggle.set_enabled(enabled);
            }
        }
    }
    /// Show that a start or stop is in flight. macOS spins beside the switch;
    /// the Linux check item has no room for it.
    fn set_busy(toggle: &Toggle, busy: bool) {
        #[cfg(target_os = "macos")]
        toggle.set_busy(busy);
        #[cfg(not(target_os = "macos"))]
        let _ = (toggle, busy);
    }
    /// True while a menu is open: AppKit runs the main loop in event-tracking
    /// mode then. Rebuilding rows under the pointer at that moment makes the
    /// menu jump, so wait for it to close.
    fn menu_is_open() -> bool {
        #[cfg(target_os = "macos")]
        return mac::menu_is_open();
        #[cfg(not(target_os = "macos"))]
        return false;
    }

    /// True when two models draw the same menu apart from toggle state, so the
    /// switch can be updated in place instead of tearing the menu down.
    pub(super) fn same_layout(a: &[Entry], b: &[Entry]) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(a, b)| match (a.toggle, b.toggle) {
                (Some(_), Some(_)) => a.label == b.label,
                _ => a == b,
            })
    }

    struct Built {
        actions: HashMap<String, Action>,
        toggles: HashMap<String, Toggle>,
        /// macOS: ids of toggle rows in menu order, given switches afterwards.
        #[cfg(target_os = "macos")]
        toggle_ids: Vec<String>,
    }

    pub struct Ui {
        tray: TrayIcon,
        icon_state: IconState,
        menu: Menu,
        actions: HashMap<String, Action>,
        toggles: HashMap<String, Toggle>,
        model: Vec<Entry>,
        snapshot: Option<Snapshot>,
        error: Option<String>,
        notice: Option<String>,
        /// Proxy starts/stops sent but not yet finished, and the state the user
        /// last asked for. While any are in flight the switch keeps showing
        /// that state instead of whatever an older catalog says.
        pending: usize,
        wanted: bool,
        /// Rows below the header changed while the menu was open; redraw them
        /// once it closes.
        stale: bool,
    }
    fn icon(state: IconState) -> Result<Icon> {
        let (pixels, size) = state.artwork();
        Ok(Icon::from_rgba(pixels.to_vec(), size, size)?)
    }
    /// Append `entries` to `menu`, whose first `offset` items are already there.
    fn fill_menu(menu: &Menu, entries: &[Entry], offset: usize) -> Result<Built> {
        fn append_entries(
            entries: &[Entry],
            append: &dyn Fn(&dyn tray_icon::menu::IsMenuItem) -> tray_icon::menu::Result<()>,
            built: &mut Built,
        ) -> Result<()> {
            for entry in entries {
                if entry.separator {
                    append(&PredefinedMenuItem::separator())?;
                } else if !entry.children.is_empty() {
                    let submenu = Submenu::new(&entry.label, entry.enabled);
                    append(&submenu)?;
                    append_entries(&entry.children, &|item| submenu.append(item), built)?;
                } else if entry.toggle.is_some() {
                    // macOS replaces this row with a switch (see `attach_switches`).
                    // Use a plain item there: a checked item makes NSMenu indent
                    // every row for a checkmark column. It stays enabled so the
                    // switch can fire its action.
                    #[cfg(target_os = "macos")]
                    let item = MenuItem::new(&entry.label, true, None);
                    #[cfg(not(target_os = "macos"))]
                    let item = CheckMenuItem::new(
                        format!("{} proxy", entry.label),
                        entry.enabled,
                        entry.toggle == Some(true),
                        None,
                    );
                    if let Some(action) = &entry.action {
                        built.actions.insert(item.id().0.clone(), action.clone());
                    }
                    #[cfg(target_os = "macos")]
                    built.toggle_ids.push(item.id().0.clone());
                    #[cfg(not(target_os = "macos"))]
                    built.toggles.insert(item.id().0.clone(), item.clone());
                    append(&item)?;
                } else {
                    let item = MenuItem::new(&entry.label, entry.enabled, None);
                    if let Some(action) = &entry.action {
                        built.actions.insert(item.id().0.clone(), action.clone());
                    }
                    append(&item)?;
                }
            }
            Ok(())
        }
        let mut built = Built {
            actions: HashMap::new(),
            toggles: HashMap::new(),
            #[cfg(target_os = "macos")]
            toggle_ids: Vec::new(),
        };
        append_entries(entries, &|item| menu.append(item), &mut built)?;
        #[cfg(target_os = "macos")]
        mac::attach_switches(menu, entries, offset, &mut built);
        #[cfg(not(target_os = "macos"))]
        let _ = offset;
        Ok(built)
    }

    #[cfg(target_os = "macos")]
    mod mac {
        use super::{Built, Entry, Menu};
        use objc2::{
            define_class, msg_send, rc::Retained, DefinedClass as _, MainThreadMarker,
            MainThreadOnly,
        };
        use objc2_app_kit::{
            NSAutoresizingMaskOptions, NSControlSize, NSControlStateValueOff,
            NSControlStateValueOn, NSEvent, NSFont, NSMenu, NSProgressIndicator,
            NSProgressIndicatorStyle, NSResponder, NSSwitch, NSTextField, NSView,
        };
        use objc2_foundation::{
            NSDefaultRunLoopMode, NSObject, NSPoint, NSRect, NSRunLoop, NSSize, NSString,
        };
        use std::cell::OnceCell;
        use tray_icon::menu::ContextMenu;

        const WIDTH: f64 = 240.0;
        const HEIGHT: f64 = 32.0;
        const INSET: f64 = 14.0;
        const GAP: f64 = 8.0;

        define_class!(
            /// A menu row that owns its clicks. Left to itself, NSSwitch runs
            /// its own drag-tracking loop inside the menu's, and when the
            /// menu redraws mid-click it can be left stuck half pressed. The
            /// row swallows the mouse instead and flips the switch with
            /// `performClick:`, which animates and fires its action. The whole
            /// row is clickable, as in Control Center.
            #[unsafe(super(NSView, NSResponder, NSObject))]
            #[thread_kind = MainThreadOnly]
            #[name = "PorchSwitchRowView"]
            #[ivars = OnceCell<Retained<NSSwitch>>]
            struct RowView;

            impl RowView {
                #[unsafe(method(hitTest:))]
                fn hit_test(&self, point: NSPoint) -> *mut NSView {
                    let hit: *mut NSView = unsafe { msg_send![super(self), hitTest: point] };
                    if hit.is_null() {
                        hit
                    } else {
                        self as *const Self as *mut NSView
                    }
                }
                #[unsafe(method(mouseDown:))]
                fn mouse_down(&self, _event: &NSEvent) {}
                #[unsafe(method(mouseUp:))]
                fn mouse_up(&self, event: &NSEvent) {
                    let point = self.convertPoint_fromView(event.locationInWindow(), None);
                    let bounds = self.bounds();
                    let inside = point.x >= 0.0
                        && point.y >= 0.0
                        && point.x <= bounds.size.width
                        && point.y <= bounds.size.height;
                    if let Some(switch) = self.ivars().get() {
                        if inside && switch.isEnabled() {
                            unsafe { switch.performClick(None) };
                        }
                    }
                }
            }
        );

        /// The switch and spinner of one toggle row.
        pub struct SwitchRow {
            switch: Retained<NSSwitch>,
            spinner: Retained<NSProgressIndicator>,
            busy: std::cell::Cell<bool>,
        }
        impl SwitchRow {
            pub fn is_on(&self) -> bool {
                self.switch.state() == NSControlStateValueOn
            }
            pub fn set(&self, on: bool, enabled: bool) {
                let state = if on {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                };
                if self.switch.state() != state {
                    self.switch.setState(state);
                }
                if self.switch.isEnabled() != enabled {
                    self.switch.setEnabled(enabled);
                }
            }
            pub fn set_busy(&self, busy: bool) {
                if self.busy.replace(busy) == busy {
                    return;
                }
                unsafe {
                    if busy {
                        self.spinner.startAnimation(None);
                    } else {
                        self.spinner.stopAnimation(None);
                    }
                }
            }
        }

        pub fn menu_is_open() -> bool {
            let run_loop = NSRunLoop::currentRunLoop();
            run_loop
                .currentMode()
                .is_some_and(|mode| &*mode != unsafe { NSDefaultRunLoopMode })
        }

        /// Give each top-level toggle row a custom view: its label on the left
        /// and a native switch on the right, with a spinner beside the switch
        /// while a change is in flight. The switch borrows the menu item's own
        /// target and action, so flipping it raises the same `MenuEvent` a
        /// click on the item would, and the menu stays open. Only top-level
        /// toggle rows are supported; the model puts the one switch in the
        /// header.
        pub fn attach_switches(menu: &Menu, entries: &[Entry], offset: usize, built: &mut Built) {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            // SAFETY: muda owns this NSMenu for the lifetime of `menu`, and we
            // are on the main thread.
            let ns_menu = unsafe { &*(menu.ns_menu() as *const NSMenu) };
            let mut ids = std::mem::take(&mut built.toggle_ids).into_iter();
            for (index, entry) in entries.iter().enumerate() {
                let Some(on) = entry.toggle else { continue };
                let Some(id) = ids.next() else { return };
                let Some(item) = ns_menu.itemAtIndex((offset + index) as isize) else {
                    continue;
                };
                let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT));
                let view = RowView::alloc(mtm).set_ivars(OnceCell::new());
                let view: Retained<RowView> =
                    unsafe { msg_send![super(view), initWithFrame: frame] };
                view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);

                let label = NSTextField::labelWithString(
                    &NSString::from_str(&entry.label.replace("&&", "&")),
                    mtm,
                );
                label.setFont(Some(&NSFont::boldSystemFontOfSize(14.0)));
                label.sizeToFit();
                let size = label.frame().size;
                label.setFrameOrigin(NSPoint::new(INSET, ((HEIGHT - size.height) / 2.0).round()));
                view.addSubview(&label);

                let switch = NSSwitch::new(mtm);
                switch.setControlSize(NSControlSize::Small);
                switch.sizeToFit();
                let size = switch.frame().size;
                let switch_x = WIDTH - INSET - size.width;
                switch.setFrameOrigin(NSPoint::new(
                    switch_x,
                    ((HEIGHT - size.height) / 2.0).round(),
                ));
                switch.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
                switch.setState(if on {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                });
                switch.setEnabled(entry.enabled);
                // SAFETY: the target is the retained menu item, which outlives
                // the switch it hosts; the selector is the one muda installed.
                unsafe {
                    switch.setTarget(item.target().as_deref());
                    switch.setAction(item.action());
                }
                view.addSubview(&switch);

                let spinner = NSProgressIndicator::new(mtm);
                spinner.setStyle(NSProgressIndicatorStyle::Spinning);
                spinner.setControlSize(NSControlSize::Small);
                spinner.setDisplayedWhenStopped(false);
                spinner.sizeToFit();
                let size = spinner.frame().size;
                spinner.setFrameOrigin(NSPoint::new(
                    (switch_x - GAP - size.width).round(),
                    ((HEIGHT - size.height) / 2.0).round(),
                ));
                spinner.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
                view.addSubview(&spinner);

                let _ = view.ivars().set(switch.clone());
                item.setView(Some(&view));
                built.toggles.insert(
                    id,
                    SwitchRow {
                        switch,
                        spinner,
                        busy: std::cell::Cell::new(false),
                    },
                );
            }
        }
    }
    impl Ui {
        pub fn new() -> Result<Self> {
            let model = menu_model(None, None, None);
            let menu = Menu::new();
            let Built {
                actions, toggles, ..
            } = fill_menu(&menu, &model, 0)?;
            let builder = TrayIconBuilder::new()
                .with_tooltip("Porch — your dev apps")
                .with_menu(Box::new(menu.clone()));
            let icon_state = IconState::from_snapshot(None);
            #[cfg(target_os = "macos")]
            let builder = builder.with_icon_templated(icon(icon_state)?);
            #[cfg(not(target_os = "macos"))]
            let builder = builder.with_icon(icon(icon_state)?);
            let tray = builder.build().context("create native Porch menu")?;
            Ok(Self {
                tray,
                icon_state,
                menu,
                actions,
                toggles,
                model,
                snapshot: None,
                error: None,
                notice: None,
                pending: 0,
                wanted: false,
                stale: false,
            })
        }
        pub fn tick(&mut self, updates: &Receiver<Update>, commands: &Sender<Action>) -> bool {
            let mut changed = false;
            for update in updates.try_iter() {
                match update {
                    Update::Snapshot(Ok(snapshot)) => {
                        self.snapshot = Some(snapshot);
                        self.error = None;
                    }
                    Update::Snapshot(Err(error)) => self.error = Some(error),
                    Update::Notice(notice) => self.notice = Some(notice),
                    Update::ProxyDone { flips, error } => {
                        self.pending = self.pending.saturating_sub(flips);
                        // Success needs no message: the switch is the message.
                        self.notice = error;
                    }
                }
                changed = true;
            }
            let open = menu_is_open();
            if self.stale && !open {
                changed = true;
            }
            if changed {
                let icon_state = IconState::from_snapshot(self.snapshot.as_ref());
                if icon_state != self.icon_state {
                    #[cfg(target_os = "macos")]
                    let result = icon(icon_state)
                        .and_then(|icon| Ok(self.tray.set_icon_templated(Some(icon))?));
                    #[cfg(not(target_os = "macos"))]
                    let result =
                        icon(icon_state).and_then(|icon| Ok(self.tray.set_icon(Some(icon))?));
                    match result {
                        Ok(()) => self.icon_state = icon_state,
                        Err(error) => eprintln!("Porch: update icon: {error:#}"),
                    }
                }
                let mut model = menu_model(
                    self.snapshot.as_ref(),
                    self.error.as_deref(),
                    self.notice.as_deref(),
                );
                if self.pending > 0 {
                    for entry in model.iter_mut().filter(|entry| entry.toggle.is_some()) {
                        entry.toggle = Some(self.wanted);
                    }
                }
                // Leading toggle rows (the header) survive every redraw: update
                // their switches in place so a flip animates once and is never
                // replaced mid-animation by a fresh control.
                let keep = model
                    .iter()
                    .zip(&self.model)
                    .take_while(|(new, old)| {
                        new.toggle.is_some() && old.toggle.is_some() && new.label == old.label
                    })
                    .count();
                for entry in model[..keep].iter() {
                    for toggle in self.toggles.values() {
                        set_toggle(toggle, entry.toggle == Some(true), entry.enabled);
                    }
                }
                if same_layout(&model[keep..], &self.model[keep.min(self.model.len())..]) {
                    self.model = model;
                    self.stale = false;
                } else if open && keep > 0 {
                    self.stale = true;
                } else {
                    // AppIndicator keeps the initial menu object. Mutate its contents
                    // instead of replacing it so refresh works on Linux as on macOS.
                    while self.menu.remove_at(keep).is_some() {}
                    match fill_menu(&self.menu, &model[keep..], keep) {
                        Ok(built) => {
                            self.actions = built.actions;
                            if keep == 0 {
                                self.toggles = built.toggles;
                            } else {
                                self.toggles.extend(built.toggles);
                            }
                            self.model = model;
                            self.stale = false;
                        }
                        Err(error) => eprintln!("Porch: update menu: {error:#}"),
                    }
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if let Some(toggle) = self.toggles.get(&event.id.0) {
                    // The control has already flipped itself. Ask for the state it
                    // now shows, not the action captured when the menu was drawn,
                    // so quick on-off-on flips each send the right command.
                    let on = toggle_is_on(toggle);
                    self.wanted = on;
                    self.pending += 1;
                    let _ = commands.send(if on {
                        Action::StartProxy
                    } else {
                        Action::StopProxy
                    });
                } else if let Some(action) = self.actions.get(&event.id.0) {
                    if *action == Action::Quit {
                        let _ = commands.send(Action::Quit);
                        return false;
                    }
                    let _ = commands.send(action.clone());
                }
            }
            for toggle in self.toggles.values() {
                set_busy(toggle, self.pending > 0);
            }
            true
        }
    }
    #[cfg(any(target_os = "macos", windows))]
    pub fn run(cli: PathBuf) -> Result<()> {
        use tao::{
            event::{Event, StartCause},
            event_loop::{ControlFlow, EventLoopBuilder},
        };
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut event_loop = EventLoopBuilder::new().build();
        #[cfg(target_os = "macos")]
        {
            use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
            event_loop.set_activation_policy(ActivationPolicy::Accessory);
        }
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
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    return native::run(cli);
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = cli;
        bail!("Porch tray supports macOS, Linux and Windows; use the headless Porch CLI on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::types::{Catalog, Machine, PeerCatalog, PROTOCOL_VERSION};
    /// A per-process counter for fixture directories: the clock (microseconds
    /// on macOS) can give parallel tests the same name.
    #[cfg(unix)]
    fn unique() -> usize {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
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
            }],
            candidates: vec![],
            warnings: vec![],
        }
    }
    #[test]
    fn remote_actions_target_stable_machine_and_app_not_remote_localhost() {
        let entries = menu_model(Some(&snapshot()), None, None);
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
        let entries = menu_model(Some(&state), None, None);
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
        let entries = menu_model(Some(&state), Some("peer timed out"), None);
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
        use std::{fs, os::unix::fs::PermissionsExt};
        let directory = std::env::temp_dir().join(format!(
            "porch-tray-worker-{}-{}",
            std::process::id(),
            unique()
        ));
        fs::create_dir(&directory).unwrap();
        let cli = directory.join("porch");
        let log = directory.join("calls");
        let quoted_log = format!("'{}'", log.display().to_string().replace('\'', "'\"'\"'"));
        let json = serde_json::to_string(&snapshot()).unwrap();
        fs::write(&cli, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {quoted_log}\ncat <<'PORCH_SNAPSHOT'\n{json}\nPORCH_SNAPSHOT\n")).unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        // Discovery is due far less often than the catalog poll, so catalog
        // refreshes happen in between even when each CLI call is slow.
        let (commands, updates) =
            worker_with_intervals(cli, Duration::from_millis(30), Duration::from_millis(400));
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
        // Wait for the observable outcome, not a number of updates: a catalog
        // refresh and a periodic discovery, both after the open.
        let (mut found_catalog, mut found_scan) = (false, false);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !(found_catalog && found_scan) && std::time::Instant::now() < deadline {
            let _ = updates.recv_timeout(Duration::from_secs(5)).unwrap();
            let calls = fs::read_to_string(&log).unwrap();
            let after_open = calls
                .split_once("open app-stable-id --peer peer-stable-id\n")
                .map_or("", |(_, after)| after);
            found_catalog = after_open.lines().any(|line| line == "list --all --json");
            found_scan = after_open
                .lines()
                .any(|line| line == "peers refresh --json");
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
        assert!(found_catalog, "catalog refresh: {calls}");
        assert!(found_scan, "periodic discovery: {calls}");
        assert!(!calls.contains("proxy stop"), "quit must not stop proxy");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pair_a_device_runs_host_side_and_shows_the_code() {
        let entries = menu_model(Some(&snapshot()), None, None);
        let pair = entries
            .iter()
            .find(|e| e.action == Some(Action::Pair))
            .expect("Pair a device… menu item");
        assert_eq!(pair.label, "Pair a device…");
        assert!(pair.enabled);
        assert_eq!(
            Action::Pair.cli_args().unwrap(),
            vec!["pair", "--no-wait", "--json"]
        );
        let notice = pairing_notice(br#"{"version":1,"code":"482913","expires":1}"#);
        assert!(notice.contains("482913") && notice.contains("porch pair 482913"));
        assert!(!pairing_notice(b"{}").contains("porch pair "));
        let shown = menu_model(Some(&snapshot()), None, Some(&notice));
        assert!(shown.iter().any(|e| e.label.contains("482913")));
    }
    #[test]
    fn header_switch_tracks_and_flips_the_proxy() {
        let loading = &menu_model(None, None, None)[0];
        assert_eq!(loading.label, "Porch");
        assert_eq!(loading.toggle, Some(false));
        assert!(!loading.enabled, "unknown state cannot be toggled");

        let mut state = snapshot();
        let on = &menu_model(Some(&state), None, None)[0];
        assert_eq!((on.toggle, on.enabled), (Some(true), true));
        assert_eq!(on.action, Some(Action::StopProxy));

        state.local.proxy_running = false;
        let entries = menu_model(Some(&state), None, None);
        assert_eq!(entries[0].toggle, Some(false));
        assert_eq!(entries[0].action, Some(Action::StartProxy));
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(e.action, Some(Action::StartProxy | Action::StopProxy)))
                .count(),
            1,
            "the switch replaces the old Start/Stop proxy item"
        );
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn flipping_the_switch_updates_in_place_but_other_changes_rebuild() {
        let mut state = snapshot();
        let on = menu_model(Some(&state), None, None);
        state.local.proxy_running = false;
        let off = menu_model(Some(&state), None, None);
        // Only the header differs at the top level, but app rows inside
        // "This machine" also change when the proxy stops, so that rebuilds.
        assert!(!native::same_layout(&on, &off));
        let mut flipped = on.clone();
        flipped[0].toggle = Some(false);
        flipped[0].action = Some(Action::StartProxy);
        assert!(native::same_layout(&on, &flipped));
        let noticed = menu_model(Some(&snapshot()), None, Some("URL copied"));
        assert!(!native::same_layout(&on, &noticed));
    }
    #[cfg(unix)]
    #[test]
    fn proxy_switch_gets_fresh_state_before_it_settles() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let directory = std::env::temp_dir().join(format!(
            "porch-tray-toggle-{}-{}",
            std::process::id(),
            unique()
        ));
        fs::create_dir(&directory).unwrap();
        let cli = directory.join("porch");
        let json = serde_json::to_string(&snapshot()).unwrap();
        fs::write(
            &cli,
            format!("#!/bin/sh\ncase \"$1\" in proxy) exit 0;; esac\ncat <<'PORCH_SNAPSHOT'\n{json}\nPORCH_SNAPSHOT\n"),
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let (commands, updates) =
            worker_with_intervals(cli, Duration::from_secs(60), Duration::from_secs(60));
        assert!(matches!(
            updates.recv_timeout(Duration::from_secs(5)).unwrap(),
            Update::Snapshot(Ok(_))
        ));
        commands.send(Action::StartProxy).unwrap();
        assert!(matches!(
            updates.recv_timeout(Duration::from_secs(5)).unwrap(),
            Update::Snapshot(Ok(_))
        ));
        assert!(matches!(
            updates.recv_timeout(Duration::from_secs(5)).unwrap(),
            Update::ProxyDone {
                flips: 1,
                error: None
            }
        ));
        commands.send(Action::Quit).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
    /// Flip the switch `flips` times while the worker is busy, starting from a
    /// running proxy, and return the CLI calls and the settled update.
    #[cfg(unix)]
    fn rapid_flips(flips: usize) -> (String, Update) {
        use std::{fs, os::unix::fs::PermissionsExt};
        let directory = std::env::temp_dir().join(format!(
            "porch-tray-flips-{flips}-{}-{}",
            std::process::id(),
            unique()
        ));
        fs::create_dir(&directory).unwrap();
        let cli = directory.join("porch");
        let log = directory.join("calls");
        let quoted_log = format!("'{}'", log.display());
        let json = serde_json::to_string(&snapshot()).unwrap();
        // The first discovery is slow, so every flip queues up behind it.
        fs::write(
            &cli,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {quoted_log}\ncase \"$1\" in peers) sleep 0.5;; proxy) exit 0;; esac\ncat <<'PORCH_SNAPSHOT'\n{json}\nPORCH_SNAPSHOT\n"),
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let (commands, updates) =
            worker_with_intervals(cli, Duration::from_secs(60), Duration::from_secs(60));
        for flip in 0..flips {
            commands
                .send(if flip % 2 == 0 {
                    Action::StopProxy
                } else {
                    Action::StartProxy
                })
                .unwrap();
        }
        let settled = loop {
            match updates.recv_timeout(Duration::from_secs(5)).unwrap() {
                done @ Update::ProxyDone { .. } => break done,
                _ => continue,
            }
        };
        commands.send(Action::Quit).unwrap();
        let calls = fs::read_to_string(&log).unwrap();
        fs::remove_dir_all(directory).unwrap();
        (calls, settled)
    }
    #[cfg(unix)]
    #[test]
    fn ten_quick_flips_back_to_on_run_nothing() {
        let (calls, settled) = rapid_flips(10);
        assert!(!calls.contains("proxy"), "no start or stop: {calls}");
        assert!(matches!(
            settled,
            Update::ProxyDone {
                flips: 10,
                error: None
            }
        ));
    }
    #[cfg(unix)]
    #[test]
    fn nine_quick_flips_ending_off_stop_once() {
        let (calls, settled) = rapid_flips(9);
        assert_eq!(calls.matches("proxy stop").count(), 1, "{calls}");
        assert!(!calls.contains("proxy start"), "{calls}");
        assert!(matches!(settled, Update::ProxyDone { flips: 9, .. }));
    }
    #[test]
    fn labels_do_not_inject_shortcuts_or_control_characters() {
        assert_eq!(clean_label("A&B\nC\t"), "A&&BC");
        assert!(clean_label(&"x".repeat(500)).len() <= 140);
    }
}
