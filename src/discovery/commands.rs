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
    Serve,
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
    /// Enroll using a fingerprint obtained through a trusted channel and one-time token.
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
            eprintln!("Porch discovery enabled. Pair peers with `porch discovery fingerprint` and `porch discovery pair-token`.");
        }
        Command::Serve => serve().await?,
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
                    if app.remote_access {
                        "open with --peer"
                    } else {
                        app.unavailable_reason.as_deref().unwrap_or("local only")
                    }
                );
            }
        }
        if let Some(error) = &peer.error {
            println!("  Unavailable: {error}");
        }
    }
    if !snapshot.candidates.is_empty() {
        println!("Discovered agents (pair before reading their apps):");
        for c in &snapshot.candidates {
            println!("  {}  {}  {:?}", c.name, c.endpoint, c.source);
        }
    }
    if snapshot.peers.is_empty() {
        println!("No paired peers. Use `porch peers add --help`.");
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
