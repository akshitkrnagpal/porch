//! `porch setup`: an idempotent first-run walkthrough.
//!
//! Prompts only on an interactive terminal; `--yes` or a non-TTY stdin accepts
//! the defaults, which are always the current saved choices.
use crate::{cli, ports, state};
use anyhow::{bail, Result};
use std::io::{BufRead, IsTerminal, Write};

#[derive(clap::Args, Debug, Default)]
pub struct SetupArgs {
    /// Accept the defaults (current settings) without prompting.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Proxy port (default 7462, or 443 with --https).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: Option<u16>,
    /// Opt in to HTTPS on port 443 (needs sudo and trusting a local CA).
    #[arg(long, conflicts_with = "no_https")]
    pub https: bool,
    /// Use plain HTTP without sudo (the default).
    #[arg(long)]
    pub no_https: bool,
}

/// Whether prompts may be shown.
pub fn interactive(yes: bool) -> bool {
    !yes && std::io::stdin().is_terminal() && std::env::var_os("CI").is_none()
}

fn read_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    Some(line.trim().to_owned())
}

/// Ask a yes/no question; non-interactive callers get `default`.
pub fn confirm(question: &str, default: bool, interactive: bool) -> bool {
    if !interactive {
        return default;
    }
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    match read_line(&format!("{question} {hint} ")).as_deref() {
        Some(a) if a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes") => true,
        Some(a) if a.eq_ignore_ascii_case("n") || a.eq_ignore_ascii_case("no") => false,
        _ => default,
    }
}

fn on_off(v: bool) -> &'static str {
    if v {
        "on"
    } else {
        "off"
    }
}

/// Choose a proxy port, explaining and avoiding conflicts unless explicitly chosen.
fn choose_port(
    wanted: u16,
    explicit: bool,
    ours: bool,
    https: &mut bool,
    interactive: bool,
) -> Result<u16> {
    let mut port = wanted;
    if !ours && !ports::available(port) {
        let who = ports::describe_owner(port);
        let next = ports::next_free(port.max(ports::FIRST_UNPRIVILEGED_PORT));
        if explicit {
            bail!(
                "port {port} is used by {who}; run `porch setup --port {}`",
                next.unwrap_or(ports::DEFAULT_PROXY_PORT)
            )
        }
        if port < ports::FIRST_UNPRIVILEGED_PORT {
            println!("Port {port} is used by {who}, so HTTPS on {port} is not possible right now.");
            *https = false;
            port = ports::DEFAULT_PROXY_PORT;
            return choose_port(port, false, false, https, interactive);
        }
        let Some(next) = next else {
            bail!("no free port near {port}; run `porch setup --port <free port>`")
        };
        println!("Port {port} is used by {who}; using port {next} instead.");
        port = next;
    }
    if interactive && !*https {
        if let Some(answer) = read_line(&format!("Proxy port [{port}]: ")) {
            if !answer.is_empty() {
                match answer.parse::<u16>() {
                    Ok(p) if p > 0 && (p == port || ports::available(p)) => port = p,
                    Ok(p) if p > 0 => {
                        println!(
                            "Port {p} is used by {}; keeping {port}.",
                            ports::describe_owner(p)
                        )
                    }
                    _ => println!("Not a port number; keeping {port}."),
                }
            }
        }
    }
    Ok(port)
}

fn private_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // UDP connect selects a route without sending packets.
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip().to_string();
    crate::discovery::server::validate_bind(&ip).ok()?;
    Some(ip)
}

pub async fn run(a: SetupArgs) -> Result<()> {
    let interactive = interactive(a.yes);
    let current = state::proxy_settings()?;
    let running = cli::proxy_running();
    if interactive {
        println!("Porch setup. Press Enter to keep the value in brackets.\n");
    }

    // 1. HTTPS opt-in (default: current choice; plain HTTP for new installs).
    let mut https = if a.https {
        true
    } else if a.no_https {
        false
    } else {
        confirm(
            "Use HTTPS on port 443? This needs sudo once and adds a local certificate authority to your trust store.",
            current.tls,
            interactive,
        )
    };

    // 2. Port, with conflict detection.
    let env_port = state::env("PORT").and_then(|p| p.parse::<u16>().ok());
    let explicit = a.port.or(env_port);
    let default_port = match (https, current.tls) {
        (true, true) | (false, false) => current.port,
        (true, false) => 443,
        (false, true) => ports::DEFAULT_PROXY_PORT,
    };
    let wanted = explicit.unwrap_or(default_port);
    let ours = running && current.port == wanted;
    let port = choose_port(wanted, explicit.is_some(), ours, &mut https, interactive)?;

    // 3. Start at login (reuses the service integration).
    let service_installed = state::dir().join("service.json").exists();
    let service = confirm(
        "Start Porch automatically at login? (installs a system service; asks for sudo)",
        service_installed,
        interactive,
    );

    // 4. Sharing with your other machines (discovery's public API only).
    let sharing_on = crate::discovery::status()
        .map(|s| s.config.is_some())
        .unwrap_or(false);
    let share = confirm(
        "Share your apps with your other machines on this network or tailnet? (they must pair first)",
        sharing_on,
        interactive,
    );

    // Apply: settings, proxy, service, sharing.
    let options = cli::ProxyOptions {
        port: Some(port),
        https,
        no_tls: !https,
        ..Default::default()
    };
    let settings = cli::resolve_settings(&options)?;
    if running && state::proxy_settings()? != settings {
        cli::stop_proxy().await?;
    }
    state::save_proxy_settings(&settings)?;
    cli::start_proxy(settings.clone(), false, true).await?;
    let settings = state::proxy_settings()?;

    let mut service_on = service_installed;
    if service && !service_installed {
        let mut args = vec![
            "install".to_owned(),
            "--port".into(),
            settings.port.to_string(),
        ];
        args.push(if settings.tls { "--https" } else { "--no-tls" }.into());
        match cli::integration("service", args).await {
            Ok(()) => service_on = true,
            Err(e) => {
                println!("Start at login was not set up ({e:#}); run `porch service install`")
            }
        }
    } else if !service && service_installed {
        match cli::integration("service", vec!["uninstall".into()]).await {
            Ok(()) => service_on = false,
            Err(e) => println!("Start at login is still on ({e:#}); run `porch service uninstall`"),
        }
    }

    let mut sharing = sharing_on;
    if share && !sharing_on {
        match private_ip() {
            Some(bind) => {
                let config = crate::discovery::types::ServerConfig {
                    bind: bind.clone(),
                    port: crate::discovery::types::DEFAULT_PORT,
                    advertise: true,
                    share_apps: true,
                };
                match crate::discovery::enable(config).await {
                    Ok(()) => sharing = true,
                    Err(e) => println!(
                        "Sharing was not enabled ({e:#}); run `porch discovery enable --bind {bind} --share-apps`"
                    ),
                }
            }
            None => println!(
                "No private network address found for sharing; run `porch discovery enable --bind <private ip> --share-apps`"
            ),
        }
    } else if !share && sharing_on {
        match crate::discovery::disable().await {
            Ok(()) => sharing = false,
            Err(e) => println!("Sharing is still on ({e:#}); run `porch discovery disable`"),
        }
    }

    println!(
        "\nPorch is ready: {} ({}).",
        settings.url("<app>"),
        if settings.tls {
            "HTTPS with a local CA"
        } else {
            "plain HTTP, no sudo"
        }
    );
    println!(
        "Start at login: {} · Sharing with your machines: {}",
        on_off(service_on),
        on_off(sharing)
    );
    println!(
        "Next: in your app's folder run `porch run pnpm dev` (or `porch run <your dev command>`)"
    );
    Ok(())
}
