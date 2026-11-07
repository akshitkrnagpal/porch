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
    /// Proxy port (default 7462, for HTTP and HTTPS alike).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: Option<u16>,
    /// Opt in to HTTPS with a local CA trusted for your user (no sudo).
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
fn choose_port(wanted: u16, explicit: bool, ours: bool, interactive: bool) -> Result<u16> {
    let mut port = wanted;
    if !ours && ports::has_listener(port) {
        let who = ports::describe_owner(port);
        let next = ports::next_free(port.max(ports::FIRST_UNPRIVILEGED_PORT));
        if explicit {
            bail!(
                "port {port} is used by {who}; run `porch setup --port {}`",
                next.unwrap_or(ports::DEFAULT_PROXY_PORT)
            )
        }
        if port < ports::FIRST_UNPRIVILEGED_PORT {
            println!(
                "Port {port} is used by {who}; using port {} instead.",
                ports::DEFAULT_PROXY_PORT
            );
            port = ports::DEFAULT_PROXY_PORT;
            return choose_port(port, false, false, interactive);
        }
        let Some(next) = next else {
            bail!("no free port near {port}; run `porch setup --port <free port>`")
        };
        println!("Port {port} is used by {who}; using port {next} instead.");
        port = next;
    }
    if interactive {
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
    let https = if a.https {
        true
    } else if a.no_https {
        false
    } else {
        confirm(
            "Use HTTPS? Porch adds a local certificate authority to your user's trust store (no sudo; macOS asks for your password or Touch ID).",
            current.tls,
            interactive,
        )
    };

    // 2. Port, with conflict detection.
    let env_port = state::env("PORT").and_then(|p| p.parse::<u16>().ok());
    let explicit = a.port.or(env_port);
    // Keep the current port unless it is the other protocol's privileged standard port.
    let default_port = if https != current.tls && current.port < ports::FIRST_UNPRIVILEGED_PORT {
        ports::DEFAULT_PROXY_PORT
    } else {
        current.port
    };
    // URLs without a port: offered when this OS lets Porch bind the standard
    // port as this user (macOS, Windows, Linux with a lowered
    // ip_unprivileged_port_start). Never needs sudo.
    let standard = if https { 443 } else { 80 };
    let already_clean = current.unprivileged && current.port == standard;
    let fresh = !state::dir().join("proxy.json").exists();
    let clean_possible = explicit.is_none()
        && ports::clean_urls_enabled()
        && ((running && already_clean) || ports::clean_port_bindable(standard));
    let clean = clean_possible
        && confirm(
            &format!(
                "Use URLs without a port, like {}://app.localhost? (no sudo needed on this computer)",
                if https { "https" } else { "http" }
            ),
            already_clean || fresh || current.port < ports::FIRST_UNPRIVILEGED_PORT,
            interactive,
        );
    if explicit.is_none() && !clean_possible && cfg!(target_os = "linux") && interactive {
        println!("Tip: for URLs without a port on Linux, allow unprivileged low ports once with\n  echo 'net.ipv4.ip_unprivileged_port_start=80' | sudo tee /etc/sysctl.d/50-porch.conf && sudo sysctl --system\nthen run `porch setup` again.");
    }
    let port = if clean {
        standard
    } else {
        let default_port = if default_port < ports::FIRST_UNPRIVILEGED_PORT && current.unprivileged
        {
            ports::DEFAULT_PROXY_PORT
        } else {
            default_port
        };
        let wanted = explicit.unwrap_or(default_port);
        let ours = running && current.port == wanted;
        choose_port(wanted, explicit.is_some(), ours, interactive)?
    };

    // 3. Start at login (reuses the service integration).
    let service_installed = state::dir().join("service.json").exists();
    let autostart_installed = crate::discovery::autostart::installed();
    let service = confirm(
        "Start Porch automatically at login? (installs a system service; asks for sudo)",
        service_installed || autostart_installed,
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
        unprivileged: clean,
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

    // Start at login covers sharing too, or paired machines lose this one at reboot.
    if service && sharing && !crate::discovery::autostart::installed() {
        if let Err(e) = crate::discovery::autostart::enable().await {
            println!(
                "Sharing will not start at login ({e:#}); run `porch discovery autostart enable`"
            );
        }
    } else if !service && crate::discovery::autostart::installed() {
        if let Err(e) = crate::discovery::autostart::disable().await {
            println!(
                "Sharing still starts at login ({e:#}); run `porch discovery autostart disable`"
            );
        }
    }

    println!(
        "\nPorch is ready: {} ({}).",
        settings.url("<app>"),
        match (settings.tls, settings.unprivileged) {
            (true, true) => "HTTPS with a local CA, no sudo",
            (true, false) => "HTTPS with a local CA",
            (false, _) => "plain HTTP, no sudo",
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
