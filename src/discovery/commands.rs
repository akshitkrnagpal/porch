//! CLI dispatch for the opt-in discovery protocol.
use super::{client, server, types::*, *};
use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Expose an authenticated catalog on one explicitly selected private interface.
    Enable {
        #[arg(long)]
        bind: String,
        #[arg(long, default_value_t=DEFAULT_PORT, value_parser=clap::value_parser!(u16).range(1..))]
        port: u16,
        /// Grant paired peers browser access to registered development apps.
        #[arg(long)]
        share_apps: bool,
        #[arg(long)]
        no_advertise: bool,
    },
    /// Run the configured agent in the foreground (no startup service is installed).
    Serve {
        /// Used by login startup: exit cleanly if already running or disabled.
        #[arg(long, hide = true)]
        autostart: bool,
    },
    /// Start sharing automatically at login (per-user; no sudo).
    Autostart {
        #[arg(value_enum)]
        action: AutostartAction,
    },
    /// Stop only the discovery agent and remove its exposure configuration.
    Disable,
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Print a short-lived, single-use pairing secret for an explicitly trusted peer.
    PairToken,
    /// Print the certificate fingerprint to compare through a trusted channel.
    Fingerprint,
    /// Revoke every enrolled client. Peers must pair again.
    Revoke,
    /// Stop this machine's remote browser relays; leave apps and proxy running.
    Disconnect,
    #[command(hide = true)]
    Relay {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        app: String,
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
}
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum AutostartAction {
    Enable,
    Disable,
    Status,
}
/// Pair two machines. Run `porch pair` on one to show a code, then
/// `porch pair CODE` on the other. Both can then open each other's apps.
#[derive(Args, Debug)]
pub struct PairArgs {
    /// The 6-digit code shown by `porch pair` on the other machine.
    pub code: Option<String>,
    /// The other machine's name or IP, when it is not discovered automatically.
    #[arg(long)]
    pub host: Option<String>,
    /// Advanced: share on this private IP instead of choosing automatically.
    #[arg(long)]
    pub bind: Option<String>,
    /// Advanced: discovery agent port.
    #[arg(long, default_value_t=DEFAULT_PORT, value_parser=clap::value_parser!(u16).range(1..))]
    pub port: u16,
    /// Show a code and return immediately instead of waiting (for scripts and the tray).
    #[arg(long)]
    pub no_wait: bool,
    #[arg(long)]
    pub json: bool,
}
pub async fn pair(args: PairArgs) -> Result<()> {
    if let Some(code) = args.code {
        if args.no_wait {
            bail!("--no-wait applies when showing a code")
        }
        let peer =
            super::pairing::join(&code, args.host.as_deref(), args.bind.as_deref(), args.port)
                .await?;
        if args.json {
            println!(
                "{}",
                serde_json::json!({"version":PROTOCOL_VERSION,"machine":peer.machine,"slug":peer.slug,"endpoint":peer.endpoint,"url":super::pairing::url_template(&peer.slug)})
            );
        } else {
            println!("Paired with {}", peer.machine.name);
            eprintln!(
                "Open its apps at {} (porch list --all shows them).",
                super::pairing::url_template(&peer.slug)
            );
            autostart_hint();
        }
        return Ok(());
    }
    if args.host.is_some() {
        bail!("--host is used with a code: porch pair CODE --host NAME-OR-IP")
    }
    let shown = super::pairing::show(args.bind.as_deref(), args.port).await?;
    if args.json {
        println!(
            "{}",
            serde_json::json!({"version":PROTOCOL_VERSION,"code":shown.code,"expires":shown.expires,"machine":shown.machine,"endpoint":shown.endpoint})
        );
    } else {
        println!("Pairing code: {}", shown.code);
        eprintln!(
            "On the other machine run:  porch pair {}\n{} is sharing on {}. The code works once and expires in 5 minutes.",
            shown.code,
            shown.machine.name,
            shown.endpoint.trim_end_matches('/')
        );
    }
    if args.no_wait {
        return Ok(());
    }
    if !args.json {
        eprintln!("Waiting for the other machine… (Ctrl-C to cancel)");
    }
    let name = super::pairing::wait(&shown.id).await?;
    println!("Paired with {name}");
    autostart_hint();
    Ok(())
}
/// Sharing started by pairing stops at reboot unless login startup is on.
fn autostart_hint() {
    if super::server::config().is_ok_and(|c| c.is_some()) && !super::autostart::installed() {
        eprintln!("To keep sharing after a restart: porch discovery autostart enable");
    }
}
pub async fn unpair(machine: &str) -> Result<()> {
    let (peer, reached) = super::pairing::unpair(machine).await?;
    println!("Unpaired {}", peer.machine.name);
    if !reached {
        eprintln!(
            "{} was unreachable; run `porch unpair` there too. Its access to this machine is already revoked.",
            peer.machine.name
        );
    }
    Ok(())
}
#[derive(Args, Debug)]
pub struct Peers {
    #[arg(long)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<PeerCommand>,
}
#[derive(Subcommand, Debug)]
pub enum PeerCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Refresh {
        #[arg(long)]
        json: bool,
    },
    /// Advanced/automation: enroll with a fingerprint and one-time token (see `porch pair`).
    #[command(alias = "trust")]
    Add {
        endpoint: String,
        #[arg(long)]
        fingerprint: String,
        /// Read the one-time token from a private file rather than standard input.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Forget local trust and stop local relays. Remote authorization is revoked on the server.
    Remove { id: String },
}
pub async fn run(command: Command) -> Result<()> {
    match command {
        Command::Enable {
            bind,
            port,
            share_apps,
            no_advertise,
        } => {
            enable(ServerConfig {
                bind,
                port,
                share_apps,
                advertise: !no_advertise,
            })
            .await?;
            eprintln!("Porch discovery enabled. Pair a machine with `porch pair`.");
        }
        Command::Serve { autostart } => serve(autostart).await?,
        Command::Autostart { action } => match action {
            AutostartAction::Enable => {
                if super::autostart::enable().await? {
                    eprintln!("Porch discovery now starts at login.");
                } else {
                    eprintln!(
                        "Porch discovery now starts at login. It is not ready yet and is retried every 30s; see {}",
                        crate::state::dir().join("discovery.log").display()
                    );
                }
            }
            AutostartAction::Disable => {
                super::autostart::disable().await?;
                eprintln!("Porch discovery no longer starts at login. Sharing is unchanged.");
            }
            AutostartAction::Status => println!(
                "{}",
                if super::autostart::installed() {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
        },
        Command::Disable => {
            disable().await?;
            eprintln!("Porch discovery disabled; apps and proxy remain running.");
        }
        Command::Status { json } => {
            let status = status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "Porch discovery: {}",
                    if status.running {
                        "running"
                    } else {
                        "disabled / stopped"
                    }
                );
                println!("Machine: {} ({})", status.machine.name, status.machine.id);
                if let Some(config) = status.config {
                    println!(
                        "Catalog: https://{}:{} · app access: {} · mDNS: {}",
                        config.bind, config.port, config.share_apps, config.advertise
                    );
                }
                if let Some(pin) = status.fingerprint {
                    println!("Certificate SHA-256: {pin}");
                }
                println!(
                    "Start at login: {}",
                    if status.autostart {
                        "on"
                    } else {
                        "off (porch discovery autostart enable)"
                    }
                );
            }
        }
        Command::PairToken => println!("{}", server::create_pairing_token()?),
        Command::Fingerprint => println!("{}", server::fingerprint()?),
        Command::Revoke => {
            server::revoke_clients()?;
            eprintln!("All discovery client authorizations revoked.");
        }
        Command::Disconnect => disconnect().await?,
        Command::Relay { peer, app, port } => {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
                tokio::select! { r=client::relay(&peer,&app,port)=>r?, _=term.recv()=>{}, _=tokio::signal::ctrl_c()=>{} }
            }
            #[cfg(not(unix))]
            client::relay(&peer, &app, port).await?;
        }
    }
    Ok(())
}
pub async fn peers(command: Peers) -> Result<()> {
    let json = command.json;
    match command.command {
        Some(PeerCommand::Add {
            endpoint,
            fingerprint,
            token_file,
        }) => {
            let token = if let Some(path) = token_file {
                let metadata = fs::metadata(&path)?;
                if metadata.len() > 1024 {
                    bail!("pairing token file is too large")
                }
                fs::read_to_string(path)?
            } else {
                use std::io::{BufRead, IsTerminal, Read};
                if std::io::stdin().is_terminal() {
                    eprintln!("Enter the server's one-time pairing token:");
                }
                let mut token = String::new();
                std::io::stdin().lock().take(1025).read_line(&mut token)?;
                if token.len() > 1024 {
                    bail!("pairing token is too large")
                }
                token
            };
            let peer = client::pair(&endpoint, &fingerprint, token.trim()).await?;
            // Peer contains a bearer secret. Only this deliberate public projection may be printed.
            println!(
                "{}",
                serde_json::json!({"version":PROTOCOL_VERSION,"machine":peer.machine,"endpoint":peer.endpoint,"fingerprint":peer.fingerprint})
            );
        }
        Some(PeerCommand::Remove { id }) => {
            disconnect_peer(&id).await?;
            client::remove_peer(&id)?;
        }
        Some(PeerCommand::Refresh { json: sub }) => {
            print_snapshot(client::snapshot(true).await?, json || sub)?
        }
        Some(PeerCommand::List { json: sub }) => {
            print_snapshot(client::snapshot(false).await?, json || sub)?
        }
        None => print_snapshot(client::snapshot(false).await?, json)?,
    }
    Ok(())
}
pub fn print_snapshot(snapshot: Snapshot, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }
    println!(
        "{} (this machine) · proxy {}",
        snapshot.local.machine.name,
        if snapshot.local.proxy_running {
            "running"
        } else {
            "stopped"
        }
    );
    if snapshot.local.apps.is_empty() {
        println!("  No local apps registered");
    }
    for app in &snapshot.local.apps {
        println!("  {}  {:?}  {}", app.name, app.health, app.local_url);
    }
    let settings = crate::state::proxy_settings().unwrap_or_default();
    for peer in &snapshot.peers {
        println!(
            "{} ({}) · {:?}",
            peer.machine.name, peer.machine.id, peer.sources
        );
        if let Some(catalog) = &peer.catalog {
            if catalog.apps.is_empty() {
                println!("  No apps registered");
            }
            for app in &catalog.apps {
                println!(
                    "  {}  {:?}  {}",
                    app.name,
                    app.health,
                    if !app.remote_access {
                        app.unavailable_reason
                            .clone()
                            .unwrap_or_else(|| "local only".into())
                    } else if let (Some(name), false) = (
                        super::gateway::hostname_app(&app.name),
                        peer.slug.is_empty(),
                    ) {
                        settings.url(&format!("{name}.{}", peer.slug))
                    } else {
                        format!("porch open {} --peer {}", app.name, peer.machine.id)
                    }
                );
            }
        }
        if let Some(error) = &peer.error {
            println!("  Unavailable: {error}");
        }
    }
    if !snapshot.candidates.is_empty() {
        println!(
            "Discovered machines (pair with `porch pair` on them, then `porch pair CODE` here):"
        );
        for c in &snapshot.candidates {
            println!("  {}  {}  {:?}", c.name, c.endpoint, c.source);
        }
    }
    if snapshot.peers.is_empty() {
        println!("No paired machines. Run `porch pair` on one machine and `porch pair CODE` on the other.");
    }
    for warning in &snapshot.warnings {
        eprintln!("Porch discovery: {warning}");
    }
    Ok(())
}
pub fn launch_tray() -> Result<()> {
    let sibling = std::env::current_exe()?.with_file_name("porch-tray");
    if !sibling.is_file() {
        bail!("porch-tray is not installed beside porch; install the desktop package or build with --features tray --bin porch-tray")
    }
    let mut c = std::process::Command::new(sibling);
    c.env("PORCH_STATE_DIR", crate::state::dir())
        .env("PORCH_BIN", std::env::current_exe()?);
    c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    c.spawn().context("launching Porch tray")?;
    Ok(())
}
