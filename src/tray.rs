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
    Open { app: String, peer: Option<String> },
    Copy { app: String, peer: Option<String> },
    StartProxy,
    StopProxy,
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
pub fn menu_model(
    snapshot: Option<&Snapshot>,
    error: Option<&str>,
    notice: Option<&str>,
) -> Vec<Entry> {
    let mut entries = vec![Entry::text("Porch")];
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
            entries.push(Entry::text("No paired machines; use porch peers add"));
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
            let mut found = vec![Entry::text("Pair in a terminal with porch peers add")];
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
    }
    entries.extend([
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
async fn invoke(cli: &Path, args: &[String]) -> Result<Vec<u8>> {
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

enum Update {
    Snapshot(Result<Snapshot, String>),
    Notice(String),
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
        refresh(&updates, true);
        let mut last_discovery = std::time::Instant::now();
        loop {
            match command_rx.recv_timeout(poll) {
                Ok(Action::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(action) => {
                    let reconnect = action == Action::Reconnect;
                    if let Some(args) = action.cli_args() {
                        let message = match runtime.block_on(invoke(&cli, &args)) {
                            Ok(_) => match action {
                                Action::Copy { .. } => "URL copied".to_owned(),
                                Action::Open { .. } => "Opened in browser".to_owned(),
                                Action::StartProxy => "Proxy started".to_owned(),
                                Action::StopProxy => "Proxy stopped; apps keep running".to_owned(),
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native {
    use super::*;
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
        Icon, TrayIcon, TrayIconBuilder,
    };

    pub struct Ui {
        _tray: TrayIcon,
        menu: Menu,
        actions: HashMap<String, Action>,
        model: Vec<Entry>,
        snapshot: Option<Snapshot>,
        error: Option<String>,
        notice: Option<String>,
    }
    fn icon() -> Result<Icon> {
        // A 2x template mark: porch roof, two posts and a welcoming open doorway.
        let mut pixels = vec![0u8; 32 * 32 * 4];
        for y in 0..32usize {
            for x in 0..32usize {
                let roof_y = 6 + x.abs_diff(16) / 2;
                let roof = (5..=27).contains(&x) && y.abs_diff(roof_y) <= 1;
                let posts =
                    ((8..=10).contains(&x) || (22..=24).contains(&x)) && (15..=26).contains(&y);
                let step = (6..=26).contains(&x) && (26..=28).contains(&y);
                let door = ((14..=15).contains(&x) || (19..=20).contains(&x))
                    && (18..=25).contains(&y)
                    || (14..=20).contains(&x) && (17..=18).contains(&y);
                if roof || posts || step || door {
                    let i = (y * 32 + x) * 4;
                    #[cfg(target_os = "linux")]
                    {
                        pixels[i] = 53;
                        pixels[i + 1] = 139;
                        pixels[i + 2] = 190;
                    }
                    pixels[i + 3] = 255;
                }
            }
        }
        Ok(Icon::from_rgba(pixels, 32, 32)?)
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
            let model = menu_model(None, None, None);
            let menu = Menu::new();
            let actions = fill_menu(&menu, &model)?;
            let builder = TrayIconBuilder::new()
                .with_tooltip("Porch — your dev apps")
                .with_menu(Box::new(menu.clone()));
            #[cfg(target_os = "macos")]
            let builder = builder.with_icon_templated(icon()?);
            #[cfg(target_os = "linux")]
            let builder = builder.with_icon(icon()?);
            let tray = builder.build().context("create native Porch menu")?;
            Ok(Self {
                _tray: tray,
                menu,
                actions,
                model,
                snapshot: None,
                error: None,
                notice: None,
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
                }
                changed = true;
            }
            if changed {
                let model = menu_model(
                    self.snapshot.as_ref(),
                    self.error.as_deref(),
                    self.notice.as_deref(),
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
                if let Some(action) = self.actions.get(&event.id.0) {
                    if *action == Action::Quit {
                        let _ = commands.send(Action::Quit);
                        return false;
                    }
                    let _ = commands.send(action.clone());
                }
            }
            true
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
    use crate::discovery::types::{Catalog, Machine, PeerCatalog, PROTOCOL_VERSION};
    fn app(remote: bool) -> App {
        App {
            id: "app-stable-id".into(),
            name: "web".into(),
            local_url: "http://web.localhost:1355".into(),
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
                endpoint: "https://192.168.1.9:1356".into(),
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
        let (commands, updates) =
            worker_with_intervals(cli, Duration::from_millis(30), Duration::from_millis(50));
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
    fn labels_do_not_inject_shortcuts_or_control_characters() {
        assert_eq!(clean_label("A&B\nC\t"), "A&&BC");
        assert!(clean_label(&"x".repeat(500)).len() <= 140);
    }
}
