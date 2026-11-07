//! CLI and process lifecycle. Child arguments are parsed only before the command.
use crate::state::{self, ProcessIdentity, ProxySettings, Route};
use anyhow::{bail, Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use fs2::FileExt;
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Parser, Debug)]
#[command(
    name = "porch",
    version,
    about = "Stable named URLs for local development",
    long_about = "Stable named URLs for local development.\n\nPorch runs your dev command on a free port and serves it at a name you choose, such as http://web.localhost. Run `porch` in a package or workspace root to start its dev script.",
    after_help = "Examples:\n  porch                         Run this package's dev script (every package at a workspace root)\n  porch run pnpm dev            Run a command, naming the app after the package\n  porch api node server.js      Run a command as api.localhost\n  porch alias db 5432           Route db.localhost to an existing port\n  porch list                    Show routes and their health\n  porch setup                   Guided first run (HTTPS, port, start at login)\n  porch doctor --fix            Find and fix common problems\n\nDocs: https://github.com/akshitkrnagpal/porch/blob/main/docs/reference.md",
    disable_version_flag = true
)]
struct Cli {
    /// Print the version
    #[arg(short='v',short_alias='V',long,action=clap::ArgAction::Version)]
    version: (),
    #[command(flatten)]
    options: RunOptions,
    /// Directory for routes, certificates and settings (default ~/.porch)
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Args, Debug, Default, Clone)]
struct RunOptions {
    /// App name; the URL becomes NAME.localhost
    #[arg(long)]
    name: Option<String>,
    /// Package script to run instead of `dev`
    #[arg(long)]
    script: Option<String>,
    /// Worktree prefix for the name; '' turns worktree naming off
    #[arg(long)]
    prefix: Option<String>,
    /// Give the app this fixed port instead of a free one
    #[arg(long,value_name="PORT",value_parser=clap::value_parser!(u16).range(1..))]
    app_port: Option<u16>,
    /// Take over the name if another app holds it, stopping that app
    #[arg(long)]
    force: bool,
    /// Also share the app on your tailnet over Tailscale Serve
    #[arg(long)]
    tailscale: bool,
    /// Also share the app publicly with Tailscale Funnel
    #[arg(long)]
    funnel: bool,
    /// Also share the app publicly through an ngrok tunnel
    #[arg(long)]
    ngrok: bool,
    /// Also share the app publicly through a Cloudflare quick tunnel
    #[arg(long)]
    cloudflare: bool,
    /// Publish NAME.local over mDNS so phones and other devices on the LAN can open it
    #[arg(long)]
    lan: bool,
    /// LAN address to advertise (default: detected automatically)
    #[arg(long)]
    ip: Option<String>,
    /// Framework whose port and host flags to add (detected automatically)
    #[arg(long)]
    framework: Option<String>,
    /// Keep this app off paired machines, even when sharing is on
    #[arg(long)]
    private: bool,
    /// Run in the background: print the URL once the app answers, then return.
    /// Output goes to `porch logs NAME`; stop it with `porch stop NAME`
    #[arg(long, short = 'd')]
    detach: bool,
}
#[derive(Parser, Debug)]
struct App {
    #[command(flatten)]
    options: RunOptions,
    /// Directory for routes, certificates and settings (default ~/.porch)
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// Command to run and its arguments
    #[arg(trailing_var_arg=true,num_args=0..)]
    command: Vec<String>,
}
#[derive(Args, Debug, Default, Clone)]
pub(crate) struct ProxyOptions {
    /// Proxy port (default 80, or 443 with HTTPS, when no sudo is needed; otherwise 7462)
    #[arg(short='p',long,value_parser=clap::value_parser!(u16).range(1..))]
    pub(crate) port: Option<u16>,
    /// Serve HTTPS and HTTP/2 with a local CA trusted for your user (no sudo)
    #[arg(long, conflicts_with = "no_tls")]
    pub(crate) https: bool,
    /// Serve plain HTTP
    #[arg(long)]
    pub(crate) no_tls: bool,
    /// Run in this terminal instead of in the background
    #[arg(long)]
    pub(crate) foreground: bool,
    /// Don't offer to trust the local CA
    #[arg(long)]
    pub(crate) skip_trust: bool,
    /// Never use sudo or edit the hosts file (implied for ports 1024 and up)
    #[arg(long)]
    pub(crate) unprivileged: bool,
    /// Serve .local names to other devices on your network
    #[arg(long)]
    pub(crate) lan: bool,
    /// LAN address to advertise (default: detected automatically)
    #[arg(long)]
    pub(crate) ip: Option<String>,
    /// Domains to serve (repeatable or comma-separated, default localhost)
    #[arg(long, value_delimiter = ',')]
    pub(crate) tld: Vec<String>,
    /// Send unknown subdomains to the closest registered parent
    #[arg(long)]
    pub(crate) wildcard: bool,
    /// Your own TLS certificate (PEM); turns on HTTPS
    #[arg(long, requires = "key", value_name = "FILE")]
    pub(crate) cert: Option<PathBuf>,
    /// Private key for --cert (PEM)
    #[arg(long, requires = "cert", value_name = "FILE")]
    pub(crate) key: Option<PathBuf>,
    /// Address to listen on (default: loopback only)
    #[arg(long, value_name = "IP")]
    pub(crate) bind: Option<String>,
    /// Extra plain-HTTP port that redirects to HTTPS
    #[arg(long,value_name="PORT",value_parser=clap::value_parser!(u16).range(1..))]
    pub(crate) redirect_port: Option<u16>,
    /// Directory for routes, certificates and settings (default ~/.porch)
    #[arg(long, value_name = "DIR")]
    pub(crate) state_dir: Option<PathBuf>,
}
#[derive(Subcommand, Debug)]
enum ProxyCommand {
    /// Start the proxy (it also starts on its own when you launch an app)
    Start(ProxyOptions),
    /// Stop the proxy; running apps keep their routes
    Stop {
        /// Only stop it if it is on this port
        #[arg(short = 'p', long)]
        port: Option<u16>,
    },
    /// Print whether the proxy is running
    Status,
}
#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a command behind a named URL, naming it from package.json, Git or the directory
    #[command(
        after_help = "Examples:\n  porch run pnpm dev\n  porch run --name docs npx vitepress dev\n  porch run --app-port 4500 -- node server.js"
    )]
    Run(App),
    /// Print an app's URL (works before the app is running, for cross-app references)
    Get {
        /// App name
        name: String,
        /// Ignore the Git worktree prefix
        #[arg(long)]
        no_worktree: bool,
    },
    /// List routes with their health and sharing URLs
    #[command(alias = "routes")]
    List {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Include paired LAN and tailnet machines in the versioned shared catalog.
        #[arg(long)]
        all: bool,
    },
    /// Pair with another machine: `porch pair` shows a code, `porch pair CODE` joins.
    Pair(crate::discovery::commands::PairArgs),
    /// Forget a paired machine on both sides.
    Unpair {
        /// Machine name or ID
        machine: String,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// List, refresh or add paired machines
    Peers(crate::discovery::commands::Peers),
    /// Manage the discovery agent that shares apps with paired machines
    Discovery {
        #[command(subcommand)]
        command: crate::discovery::commands::Command,
    },
    /// Open an app locally or through a paired machine's authenticated browser relay.
    Open {
        /// App name
        app: String,
        /// Paired machine that runs the app
        #[arg(long)]
        peer: Option<String>,
        /// Print the URL instead of opening a browser
        #[arg(long)]
        url_only: bool,
    },
    /// Copy a local app URL or a paired machine's loopback relay URL.
    Copy {
        /// App name
        app: String,
        /// Paired machine that runs the app
        #[arg(long)]
        peer: Option<String>,
    },
    /// Launch the optional native menu bar / tray companion.
    Tray,
    /// Route a name to something Porch didn't start (a port, Docker container or URL)
    #[command(
        after_help = "Examples:\n  porch alias db 5432\n  porch alias grafana http://127.0.0.1:3001\n  porch alias --remove db"
    )]
    Alias {
        /// Name for the route
        name: Option<String>,
        /// Port (3000) or HTTP(S) URL to send requests to
        target: Option<String>,
        /// Remove the alias with this name
        #[arg(long, value_name = "NAME")]
        remove: Option<String>,
        /// Replace an existing route with this name
        #[arg(long)]
        force: bool,
        /// Only register under these domains
        #[arg(long, value_delimiter = ',')]
        tld: Vec<String>,
        /// Keep this alias off paired machines, even when sharing is on
        #[arg(long)]
        private: bool,
        /// How to talk to the port: https (TLS, keep Host), https://HOST[:PORT]
        /// (TLS with that server name and Host) or http://HOST[:PORT] (that Host)
        #[arg(long, value_name = "SPEC")]
        upstream: Option<String>,
        /// With an https upstream, don't verify its certificate (self-signed dev servers)
        #[arg(long, requires = "upstream")]
        insecure: bool,
    },
    /// Stop an app Porch started (in this or another terminal, or with --detach)
    Stop {
        /// App name
        name: String,
    },
    /// Show the output of an app started with `porch run --detach`
    Logs {
        /// App name
        name: String,
        /// Keep printing new output
        #[arg(long, short = 'f')]
        follow: bool,
        /// Show only the last N lines
        #[arg(long, short = 'n', value_name = "N")]
        lines: Option<usize>,
    },
    /// Remove an alias
    #[command(alias = "rm")]
    Remove {
        /// Alias name
        name: String,
        /// Only remove it from these domains
        #[arg(long, value_delimiter = ',')]
        tld: Vec<String>,
    },
    /// Remove routes whose apps are gone
    Prune {
        /// Also stop processes left behind by those routes
        #[arg(long)]
        force: bool,
    },
    /// Remove everything Porch installed: trust, services, hosts entries and state
    Clean {
        /// Don't ask for confirmation
        #[arg(long)]
        force: bool,
    },
    /// Start, stop or check the proxy
    Proxy {
        #[command(subcommand)]
        command: Option<ProxyCommand>,
    },
    /// Run an MCP server over stdio so coding agents can find and manage your apps
    Mcp {
        /// Tool profiles: core, proxy, diagnostics, readonly or all
        #[arg(long, alias = "tools-profiles")]
        tools: Option<String>,
        /// Same as --tools
        profile: Option<String>,
    },
    /// Sync or clean Porch's block in the hosts file (`porch hosts sync|clean`)
    Hosts {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Start the proxy at login (`porch service install|status|uninstall`)
    Service {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run `docker compose` and give each HTTP service a named URL (`porch compose up`)
    Compose {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage Tailscale, ngrok and Cloudflare sharing for running apps
    Share {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show LAN mode status and addresses
    Lan,
    /// Check the proxy, routes, name resolution and trust, and suggest fixes
    Doctor {
        /// Apply safe fixes; terminating stopped or orphaned launches still asks first.
        #[arg(long)]
        fix: bool,
        /// Accept every fix, including terminating stopped or orphaned launches.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Interactive first-run setup: port, HTTPS, start at login, sharing.
    Setup(crate::setup::SetupArgs),
    /// Trust Porch's local CA for your user (login keychain / NSS databases; no sudo).
    Trust {
        /// Trust it system-wide instead, so every tool on the machine accepts it (asks for sudo).
        #[arg(long)]
        system: bool,
    },
    /// Show the packages Porch would start at this workspace root
    Workspace,
    /// Update porch to the latest release (or run `brew upgrade` for Homebrew installs).
    SelfUpdate(crate::update::SelfUpdateArgs),
    #[command(external_subcommand)]
    Named(Vec<String>),
}
fn merge(mut a: RunOptions, b: RunOptions) -> RunOptions {
    if b.name.is_some() {
        a.name = b.name
    }
    if b.script.is_some() {
        a.script = b.script
    }
    if b.prefix.is_some() {
        a.prefix = b.prefix
    }
    if b.app_port.is_some() {
        a.app_port = b.app_port
    }
    if b.ip.is_some() {
        a.ip = b.ip
    }
    if b.framework.is_some() {
        a.framework = b.framework
    }
    a.force |= b.force;
    a.tailscale |= b.tailscale;
    a.funnel |= b.funnel;
    a.ngrok |= b.ngrok;
    a.cloudflare |= b.cloudflare;
    a.lan |= b.lan;
    a.private |= b.private;
    a.detach |= b.detach;
    a
}
pub async fn run() -> Result<i32> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // --name explicitly bypasses reserved command dispatch.
    if leading_name(&raw) {
        let app = App::parse_from(std::iter::once("porch".to_string()).chain(raw));
        if let Some(d) = &app.state_dir {
            std::env::set_var("PORCH_STATE_DIR", d)
        }
        return launch(app.options, app.command).await;
    }
    let (cli, extra) = if let Some(at) = bare_separator(&raw) {
        (
            Cli::parse_from(std::iter::once("porch".to_owned()).chain(raw[..at].iter().cloned())),
            raw[at + 1..].to_vec(),
        )
    } else {
        (Cli::parse(), Vec::new())
    };
    if let Some(d) = cli.state_dir {
        std::env::set_var("PORCH_STATE_DIR", d)
    }
    let options = cli.options;
    if options.lan {
        std::env::set_var("PORCH_LAN", "1")
    }
    if let Some(ip) = &options.ip {
        std::env::set_var("PORCH_LAN_IP", ip)
    }
    if options.private {
        std::env::set_var("PORCH_PRIVATE", "1")
    }
    for (key, on) in [
        ("TAILSCALE", options.tailscale),
        ("FUNNEL", options.funnel),
        ("NGROK", options.ngrok),
        ("CLOUDFLARE", options.cloudflare),
    ] {
        if on {
            std::env::set_var(format!("PORCH_{key}"), "1");
        }
    }
    match cli.command {
        None => {
            let cwd = std::env::current_dir()?;
            if options.script.is_none()
                && !cwd.join("package.json").is_file()
                && crate::config::find_workspace_root(&cwd).is_none()
            {
                Cli::command().print_help()?;
                println!();
                return Ok(0);
            }
            return launch_extra(options, vec![], extra).await;
        }
        Some(Commands::Run(a)) => {
            if let Some(d) = &a.state_dir {
                std::env::set_var("PORCH_STATE_DIR", d)
            }
            return launch(merge(options, a.options), a.command).await;
        }
        Some(Commands::SelfUpdate(a)) => return crate::update::run(a).await,
        Some(Commands::Named(mut args)) => {
            let name = args.remove(0);
            let a = App::parse_from(std::iter::once("porch".into()).chain(args));
            if let Some(d) = &a.state_dir {
                std::env::set_var("PORCH_STATE_DIR", d)
            }
            let mut o = merge(options, a.options);
            o.name = Some(name);
            return launch(o, a.command).await;
        }
        Some(Commands::Get { name, no_worktree }) => {
            let n = if no_worktree {
                normalize(&name)?
            } else {
                let cwd = std::env::current_dir()?;
                let c = crate::config::effective_config(&cwd)?;
                let p = crate::config::detect_worktree_prefix(
                    &cwd,
                    c["worktree"]["prefix"].as_str().unwrap_or("last-segment"),
                );
                normalize(&crate::config::apply_worktree_prefix(
                    &name,
                    p.as_deref(),
                    c["worktree"]["hostnameTemplate"].as_str(),
                ))?
            };
            println!("{}", resolve_settings(&ProxyOptions::default())?.url(&n));
        }
        Some(Commands::List { json, all }) => {
            if all {
                crate::discovery::commands::print_snapshot(
                    crate::discovery::client::snapshot(false).await?,
                    json,
                )?;
            } else {
                list(json)?;
            }
        }
        Some(Commands::Pair(p)) => crate::discovery::commands::pair(p).await?,
        Some(Commands::Unpair { machine, json }) => {
            crate::discovery::commands::unpair(&machine, json).await?
        }
        Some(Commands::Peers(p)) => crate::discovery::commands::peers(p).await?,
        Some(Commands::Discovery { command }) => crate::discovery::commands::run(command).await?,
        Some(Commands::Open {
            app,
            peer,
            url_only,
        }) => {
            let url = crate::discovery::app_url(&app, peer.as_deref()).await?;
            if !url_only {
                crate::discovery::open_url(&url)?;
            }
            println!("{url}");
        }
        Some(Commands::Copy { app, peer }) => {
            let url = crate::discovery::app_url(&app, peer.as_deref()).await?;
            crate::discovery::copy_url(&url)?;
            println!("{url}");
        }
        Some(Commands::Tray) => crate::discovery::commands::launch_tray()?,
        Some(Commands::Alias {
            name,
            target,
            remove,
            force,
            tld,
            private,
            upstream,
            insecure,
        }) => {
            if private {
                std::env::set_var("PORCH_PRIVATE", "1")
            }
            if let Some(n) = remove {
                remove_alias(&n, &tld)?
            } else {
                let upstream = upstream
                    .map(|spec| state::Upstream::parse(&spec, insecure))
                    .transpose()?;
                alias(
                    &name.context("name required")?,
                    &target.context("port or HTTP target required")?,
                    force,
                    &tld,
                    upstream,
                )
                .await?
            }
            sync_hosts().await;
        }
        Some(Commands::Stop { name }) => return stop_app(&name).await,
        Some(Commands::Logs {
            name,
            follow,
            lines,
        }) => return show_logs(&name, follow, lines).await,
        Some(Commands::Remove { name, tld }) => {
            remove_alias(&name, &tld)?;
            sync_hosts().await;
        }
        Some(Commands::Prune { force }) => {
            prune(force).await?;
            crate::integrations::prune_shares(force)?;
        }
        Some(Commands::Clean { force }) => {
            // Per-user trust is removed as this user, before any elevation.
            crate::integrations::untrust_user_scope()?;
            if clean_needs_elevation() {
                return elevate(&raw).await;
            }
            stop_proxy().await?;
            prune(force).await?;
            crate::integrations::clean()?;
            clean_state()?;
        }
        Some(Commands::Proxy { command }) => match command.unwrap_or(ProxyCommand::Status) {
            ProxyCommand::Start(o) => {
                if let Some(d) = &o.state_dir {
                    std::env::set_var("PORCH_STATE_DIR", d)
                }
                let s = resolve_settings(&o)?;
                if o.foreground {
                    if s.tls {
                        state::load()?;
                        crate::tls::config_for_settings(&s)?;
                    }
                    trust_if_needed(&s, o.skip_trust).await?;
                    return foreground(s).await.map(|_| 0);
                }
                start_proxy(s, o.skip_trust, port_explicit(&o)).await?;
            }
            ProxyCommand::Stop { port } => {
                if port.is_some_and(|p| state::proxy_settings().is_ok_and(|s| s.port != p)) {
                    bail!("no proxy at requested port in this state directory")
                }
                let was_running = proxy_running();
                stop_proxy().await?;
                println!(
                    "{}",
                    if was_running {
                        "Proxy stopped"
                    } else {
                        "Proxy was not running"
                    }
                );
            }
            ProxyCommand::Status => println!(
                "{}",
                if proxy_running() {
                    "running"
                } else {
                    "stopped"
                }
            ),
        },
        Some(Commands::Mcp { tools, profile }) => {
            crate::mcp::serve(
                tools
                    .or(profile)
                    .or_else(|| state::env("MCP_TOOLS"))
                    .unwrap_or_else(|| "core".into()),
            )
            .await?
        }
        Some(Commands::Hosts { args }) => integration("hosts", args).await?,
        Some(Commands::Service { args }) => integration("service", args).await?,
        Some(Commands::Compose { args }) => {
            if args.iter().any(|a| a == "up") {
                ensure_proxy().await?;
            }
            integration("compose", args).await?;
        }
        Some(Commands::Share { args }) => integration("share", args).await?,
        Some(Commands::Lan) => integration("lan", vec![]).await?,
        Some(Commands::Doctor { fix, yes }) => {
            integration("doctor", vec![]).await?;
            if crate::doctor::run(fix, yes).await? > 0 {
                return Ok(1);
            }
        }
        Some(Commands::Setup(args)) => crate::setup::run(args).await?,
        Some(Commands::Trust { system }) => {
            if system {
                // Per-user trust lives in this user's stores: remove it before elevating.
                crate::integrations::untrust_user_scope()?;
                integration("trust", vec!["--system".into()]).await?
            } else {
                integration("trust", vec![]).await?
            }
        }
        Some(Commands::Workspace) => {
            let cwd = std::env::current_dir()?;
            let root = crate::config::find_workspace_root(&cwd).unwrap_or(cwd);
            let plans = crate::config::launch_plans(
                &root,
                None,
                options.name.as_deref(),
                options.script.as_deref(),
                options.prefix.as_deref(),
            )?;
            let p = resolve_settings(&ProxyOptions::default())?;
            use crate::style::{self, Stream::Stdout};
            let width = plans
                .iter()
                .map(|x| p.url(&x.name).len())
                .max()
                .unwrap_or(3);
            println!(
                "{}",
                style::dim(Stdout, &format!("{:<width$}  {:<8} COMMAND", "URL", "DIR"))
            );
            for plan in plans {
                let rel = plan.cwd.strip_prefix(&root).unwrap_or(&plan.cwd);
                let rel = if rel.as_os_str().is_empty() {
                    ".".to_owned()
                } else {
                    rel.display().to_string()
                };
                let url = if plan.proxy {
                    p.url(&plan.name)
                } else {
                    "(task, no URL)".to_owned()
                };
                println!(
                    "{}{}  {:<8} {}",
                    style::url(Stdout, &url),
                    " ".repeat(width.saturating_sub(url.len())),
                    rel,
                    style::dim(Stdout, &plan.command.join(" "))
                );
            }
        }
    }
    Ok(0)
}
pub(crate) async fn integration(name: &str, args: Vec<String>) -> Result<()> {
    let argv: Vec<String> = std::iter::once(name.into()).chain(args).collect();
    if requires_elevation(name, &argv[1..]) {
        let code = elevate(&argv).await?;
        if code != 0 {
            bail!("elevated {name} command exited {code}")
        }
        return Ok(());
    }
    crate::integrations::run(argv).await
}
fn enabled(key: &str) -> Option<bool> {
    state::env(key).map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
}
pub fn normalize(n: &str) -> Result<String> {
    let n = n.trim_end_matches('.').to_ascii_lowercase();
    if n.is_empty()
        || n.len() > 253
        || n.split('.').any(|s| {
            s.is_empty()
                || s.len() > 63
                || s.starts_with('-')
                || s.ends_with('-')
                || !s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        bail!("invalid DNS name: {n}")
    }
    Ok(n)
}
fn tlds(values: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for v in values {
        for t in v.split(',') {
            let t = normalize(t.trim())?;
            if !out.contains(&t) {
                out.push(t)
            }
        }
    }
    if out.is_empty() {
        bail!("at least one TLD required")
    }
    Ok(out)
}
/// Did the caller choose the proxy port (flag or PORCH_PORT)? Explicit ports never move.
pub(crate) fn port_explicit(o: &ProxyOptions) -> bool {
    o.port.is_some() || state::env("PORT").is_some()
}
pub(crate) fn resolve_settings(o: &ProxyOptions) -> Result<ProxySettings> {
    use crate::ports::{DEFAULT_PROXY_PORT, FIRST_UNPRIVILEGED_PORT};
    let saved = state::proxy_settings()?;
    let mut s = saved.clone();
    let tls_explicit = o.https || o.no_tls || state::env("HTTPS").is_some() || o.cert.is_some();
    if let Some(v) = enabled("HTTPS") {
        s.tls = v
    }
    if o.https {
        s.tls = true
    }
    if o.no_tls {
        s.tls = false
    }
    let unprivileged_choice = if o.unprivileged {
        Some(true)
    } else {
        enabled("UNPRIVILEGED")
    };
    if unprivileged_choice == Some(true) && !tls_explicit {
        s.tls = false
    }
    if let Some(v) = enabled("LAN") {
        s.lan = v;
        if !v {
            s.lan_ip = None;
            if s.tlds == ["local"] {
                s.tlds = vec!["localhost".into()]
            }
        }
    }
    if o.lan {
        s.lan = true
    }
    if let Some(ip) = o.ip.clone().or_else(|| state::env("LAN_IP")) {
        let _: std::net::IpAddr = ip.parse().context("invalid LAN IP")?;
        s.lan = true;
        s.lan_ip = Some(ip)
    }
    if let Some(v) = enabled("WILDCARD") {
        s.wildcard = v
    }
    if o.wildcard {
        s.wildcard = true
    }
    if let Some(t) = state::env("TLD") {
        s.tlds = tlds(&[t])?
    }
    if !o.tld.is_empty() {
        s.tlds = tlds(&o.tld)?
    }
    if s.lan {
        s.tlds = vec!["local".into()]
    }
    if let Some(c) = &o.cert {
        s.cert = Some(fs::canonicalize(c)?);
        s.key = Some(fs::canonicalize(
            o.key.as_ref().context("--cert requires --key")?,
        )?);
        s.tls = true
    }
    if !s.tls {
        s.cert = None;
        s.key = None
    }
    let explicit_port = o
        .port
        .or_else(|| state::env("PORT").and_then(|p| p.parse().ok()));
    if state::env("PORT").is_some() && explicit_port.is_none() {
        bail!("invalid proxy port; set a number, e.g. `PORCH_PORT={DEFAULT_PROXY_PORT} porch proxy start`")
    }
    let sync_requested = enabled("SYNC_HOSTS") == Some(true);
    let fresh = !state::dir().join("proxy.json").exists();
    let mut clean = false;
    // Back to the port-free standard port after a temporary fallback.
    // Only between runs: a running proxy keeps the port it has.
    if explicit_port.is_none() && crate::ports::clean_urls_enabled() && !proxy_running() {
        if let Some(p) = saved.preferred_port {
            if crate::ports::clean_port_bindable(p) && (p == 443) == s.tls {
                s.port = p;
                clean = true;
            }
        }
    }
    if clean {
        s.preferred_port = None;
    } else if let Some(p) = explicit_port {
        if p == 0 {
            bail!("proxy port must be 1-65535; run `porch proxy start --port {DEFAULT_PROXY_PORT}`")
        }
        s.port = p
    } else if (s.tls && s.port == 80) || (!s.tls && s.port == 443) {
        // Switching protocol leaves the other protocol's standard port. Move to
        // the new protocol's standard port when no sudo is needed for it,
        // otherwise to the no-sudo high port; 443 with sudo takes `--port 443`.
        let standard = if s.tls { 443 } else { 80 };
        if saved.unprivileged
            && crate::ports::clean_urls_enabled()
            && crate::ports::clean_port_bindable(standard)
        {
            s.port = standard;
            clean = true;
        } else {
            s.port = DEFAULT_PROXY_PORT
        }
    } else if fresh
        && !sync_requested
        && unprivileged_choice != Some(false)
        && crate::ports::clean_urls_enabled()
    {
        // First run: serve URLs without a port when this OS lets us bind the
        // standard port without sudo. Otherwise keep the no-sudo default.
        let standard = if s.tls { 443 } else { 80 };
        if crate::ports::clean_port_bindable(standard) {
            s.port = standard;
            clean = true;
        }
    }
    if s.preferred_port == Some(s.port) {
        s.preferred_port = None;
    }
    // Port and privilege mode are one choice: high ports never need sudo or
    // hosts-file edits (browsers resolve *.localhost natively), unless the user
    // explicitly asked for hosts synchronization.
    s.unprivileged = match unprivileged_choice {
        Some(v) => v,
        None if clean => true,
        None if s.port >= FIRST_UNPRIVILEGED_PORT => !sync_requested,
        None => saved.unprivileged && saved.port == s.port,
    };
    if let Some(b) = &o.bind {
        let _: std::net::IpAddr = b.parse()?;
        s.bind = Some(b.clone())
    }
    if let Some(p) = o.redirect_port {
        s.redirect_port = Some(p)
    } else if s.tls && s.port == 443 && (!s.unprivileged || crate::ports::clean_port_bindable(80)) {
        s.redirect_port = Some(80)
    }
    if !s.tls {
        s.redirect_port = None
    }
    if s.unprivileged && sync_requested {
        bail!("PORCH_SYNC_HOSTS=1 cannot be combined with --unprivileged; drop one, e.g. `PORCH_SYNC_HOSTS=0 porch proxy start --unprivileged`")
    }
    Ok(s)
}
fn proxy_identity() -> Option<ProcessIdentity> {
    serde_json::from_slice(&fs::read(state::dir().join("proxy.identity")).ok()?).ok()
}
pub(crate) fn proxy_running() -> bool {
    proxy_identity().is_some_and(|p| state::matches(&p))
        && fs::read_to_string(state::dir().join("proxy.ready"))
            .ok()
            .is_some_and(|v| v.trim().parse::<u32>().ok() == proxy_identity().map(|i| i.pid))
}
fn daemon_lock() -> Result<fs::File> {
    state::load()?;
    let mut o = fs::OpenOptions::new();
    o.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let f = o.open(state::dir().join("proxy.lock"))?;
    state::inherit_owner(&state::dir().join("proxy.lock"))?;
    f.try_lock_exclusive()
        .context("another proxy owns this state directory")?;
    Ok(f)
}
struct ProxyGuard {
    pid: u32,
}
impl Drop for ProxyGuard {
    fn drop(&mut self) {
        if proxy_identity().is_some_and(|p| p.pid == self.pid) {
            for file in [
                "proxy.pid",
                "proxy.identity",
                "proxy.ready",
                "proxy.version",
            ] {
                let _ = fs::remove_file(state::dir().join(file));
            }
        }
    }
}
async fn foreground(s: ProxySettings) -> Result<()> {
    let mut signals = TerminationSignals::new()?;
    let _lock = daemon_lock()?;
    let i = state::identity(std::process::id()).context("cannot identify proxy process")?;
    let _guard = ProxyGuard { pid: i.pid };
    let _ = fs::remove_file(state::dir().join("proxy.ready"));
    state::save_proxy_settings(&s)?;
    state::atomic_write("proxy.pid", i.pid.to_string().as_bytes())?;
    state::atomic_write("proxy.identity", &serde_json::to_vec(&i)?)?;
    state::atomic_write("proxy.version", PROXY_VERSION.as_bytes())?;
    crate::health::reconcile_quietly();
    let cleanup = tokio::spawn(crate::health::reconcile_forever(Duration::from_secs(5)));
    let result = tokio::select! {r=crate::proxy::serve_config(s)=>r,s=signals.recv()=>{s?;Ok(())}};
    cleanup.abort();
    result
}
const PROXY_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Version recorded by the running proxy; `None` for proxies predating the record.
pub(crate) fn running_proxy_version() -> Option<String> {
    fs::read_to_string(state::dir().join("proxy.version"))
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}
fn version_key(v: &str) -> Vec<u64> {
    v.split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}
/// A running proxy older than this binary (or with no recorded version).
pub(crate) fn proxy_outdated() -> bool {
    proxy_running()
        && running_proxy_version().is_none_or(|v| version_key(&v) < version_key(PROXY_VERSION))
}
/// Restart an older running proxy with this binary. Routes live in state and survive.
pub(crate) async fn restart_outdated_proxy() -> Result<bool> {
    if !proxy_outdated() {
        return Ok(false);
    }
    let settings = state::proxy_settings()?;
    eprintln!(
        "Restarting the Porch proxy: it runs {}, this porch is {PROXY_VERSION}. Routes are kept.",
        running_proxy_version().unwrap_or_else(|| "an older version".into())
    );
    stop_proxy()
        .await
        .context("could not stop the outdated proxy; run `porch proxy stop` and retry")?;
    Box::pin(start_proxy(settings, true, true)).await?;
    Ok(true)
}
/// A busy proxy port: move to the next free port unless the user chose this one.
pub(crate) fn move_off_busy_port(mut s: ProxySettings, explicit: bool) -> Result<ProxySettings> {
    use crate::ports::{DEFAULT_PROXY_PORT, FIRST_UNPRIVILEGED_PORT};
    let port = s.port;
    let who = crate::ports::describe_owner(port);
    let next = crate::ports::next_free(port.max(FIRST_UNPRIVILEGED_PORT))
        .context("no free proxy port found nearby; run `porch proxy start --port <free port>`")?;
    if explicit {
        bail!("proxy port {port} unavailable: it is used by {who}. Stop that process, or use the next free port: `porch proxy start --port {next}`")
    }
    if port < FIRST_UNPRIVILEGED_PORT {
        let fallback = if crate::ports::available(DEFAULT_PROXY_PORT) {
            DEFAULT_PROXY_PORT
        } else {
            next
        };
        if s.unprivileged {
            // A no-sudo standard port (port-free URLs) that something else
            // took: fall back to the high port for now, and remember to come
            // back to the standard port once it is free again.
            s.port = fallback;
            s.redirect_port = None;
            s.preferred_port = Some(port);
            state::save_proxy_settings(&s)?;
            eprintln!(
                "Port {port} is used by {who}; Porch uses port {fallback} for now (apps: {}) and returns to port {port} on a later start once it's free.",
                s.url("<name>")
            );
            return Ok(s);
        }
        if s.tls {
            bail!("proxy port {port} unavailable: it is used by {who}. Use HTTPS on a port that needs no sudo instead: `porch proxy start --https --port {fallback}`")
        }
        bail!("proxy port {port} unavailable: it is used by {who}. Use plain HTTP without sudo instead: `porch proxy start --no-tls --port {fallback}`")
    }
    s.port = next;
    state::save_proxy_settings(&s)?;
    eprintln!(
        "Port {port} is used by {who}; Porch now uses port {next} (saved). Apps: {}",
        s.url("<name>")
    );
    Ok(s)
}
fn settings_args(s: &ProxySettings) -> Vec<String> {
    let mut a = vec![
        "proxy".into(),
        "start".into(),
        "--foreground".into(),
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
        a.extend(["--tld".into(), t.clone()])
    }
    for (flag, on) in [
        ("--lan", s.lan),
        ("--wildcard", s.wildcard),
        ("--unprivileged", s.unprivileged),
    ] {
        if on {
            a.push(flag.into())
        }
    }
    if let Some(ip) = &s.lan_ip {
        a.extend(["--ip".into(), ip.clone()])
    }
    if let Some(bind) = &s.bind {
        a.extend(["--bind".into(), bind.clone()])
    }
    if let (Some(c), Some(k)) = (&s.cert, &s.key) {
        a.extend([
            "--cert".into(),
            c.to_string_lossy().into(),
            "--key".into(),
            k.to_string_lossy().into(),
        ])
    }
    if let Some(p) = s.redirect_port {
        a.extend(["--redirect-port".into(), p.to_string()])
    }
    a
}
fn authenticate_sudo(interactive: bool, port: u16) -> Result<()> {
    let guidance = format!("port {port} requires administrator privileges; run `porch proxy start` in a terminal to complete sudo setup, or switch to the no-sudo default: `porch proxy start --no-tls --port {}`", crate::ports::DEFAULT_PROXY_PORT);
    let mut command = Command::new("sudo");
    if !interactive {
        command.arg("-n");
    }
    command.arg("-v");
    if !interactive {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    let status = command.status().with_context(|| guidance.clone())?;
    if !status.success() {
        bail!("{guidance}");
    }
    Ok(())
}
pub(crate) async fn start_proxy(
    mut s: ProxySettings,
    skip_trust: bool,
    port_explicit: bool,
) -> Result<()> {
    // Starting clears a deliberate stop, so launchers watch over it again.
    let _ = fs::remove_file(state::dir().join("proxy.stopped"));
    if restart_outdated_proxy().await? {
        return Ok(());
    }
    let _startup = startup_lock().await?;
    if proxy_running() {
        let actual = state::proxy_settings()?;
        // Privilege mode is derived from the port; it alone never blocks reuse.
        let comparable = ProxySettings {
            unprivileged: actual.unprivileged,
            ..s.clone()
        };
        if actual != comparable {
            bail!(
                "the running proxy uses different settings ({}); run `porch proxy stop` and retry",
                actual.url("<name>")
            )
        }
        trust_if_needed(&s, skip_trust).await?;
        return Ok(());
    }
    // Detect privilege requirements before detaching; never leave a false ready marker.
    let mut needs_privilege = false;
    let mut privileged_port = s.port;
    match crate::sockets::reserve(s.port) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            needs_privilege = true;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let port = s.port;
            let released = tokio::task::spawn_blocking(move || {
                crate::ports::wait_released(port, Duration::from_secs(60))
            })
            .await?;
            if !released {
                s = move_off_busy_port(s, port_explicit)?;
            }
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "proxy port {} unavailable; choose another: `porch proxy start --port {}`",
                    s.port,
                    crate::ports::DEFAULT_PROXY_PORT
                )
            })
        }
    }
    if let Some(port) = s.redirect_port.filter(|p| s.tls && *p != s.port) {
        match crate::sockets::reserve(port) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                needs_privilege = true;
                privileged_port = port;
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "HTTP redirect port {port} unavailable: it is used by {}; run `porch proxy start --https --redirect-port 8080`",
                        crate::ports::describe_owner(port)
                    )
                })
            }
        }
    }
    let mut elevated = false;
    if needs_privilege
        && s.unprivileged
        && !port_explicit
        && s.port < crate::ports::FIRST_UNPRIVILEGED_PORT
    {
        // Saved port-free settings on an OS (or binary) that no longer allows
        // binding the low port without sudo: fall back instead of failing.
        let old = s.port;
        s.port = if crate::ports::available(crate::ports::DEFAULT_PROXY_PORT) {
            crate::ports::DEFAULT_PROXY_PORT
        } else {
            crate::ports::next_free(crate::ports::DEFAULT_PROXY_PORT)
                .context("no free proxy port found; run `porch proxy start --port <free port>`")?
        };
        s.redirect_port = None;
        state::save_proxy_settings(&s)?;
        eprintln!(
            "This computer doesn't allow port {old} without sudo; Porch now uses port {} (saved). Apps: {}",
            s.port,
            s.url("<name>")
        );
        needs_privilege = false;
    }
    if needs_privilege {
        if s.unprivileged {
            bail!(
                "this OS does not permit unprivileged binding on port {}; run `porch proxy start --port {}`",
                s.port,
                crate::ports::DEFAULT_PROXY_PORT
            )
        }
        use std::io::IsTerminal;
        authenticate_sudo(
            std::io::stdin().is_terminal() && std::env::var_os("CI").is_none(),
            privileged_port,
        )?;
        elevated = true;
    }
    state::load()?;
    let _ = fs::remove_file(state::dir().join("proxy.ready"));
    let mut log_options = fs::OpenOptions::new();
    log_options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let log = log_options.open(state::dir().join("proxy.log"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        log.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let exe = std::env::current_exe()?;
    let mut c = if elevated {
        let mut c = Command::new("sudo");
        c.args(["-n", "env"])
            .arg(format!("PORCH_STATE_DIR={}", state::dir().display()));
        for (k, v) in
            std::env::vars().filter(|(k, _)| k.starts_with("PORCH_") || k.starts_with("PORTLESS_"))
        {
            c.arg(format!("{k}={v}"));
        }
        c.arg(exe);
        c
    } else {
        Command::new(exe)
    };
    c.args(settings_args(&s))
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    // Off the launching console: Ctrl+C in that terminal, or closing it, must
    // not reach the shared proxy. It is stopped by request instead.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(
            windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP
                | windows_sys::Win32::System::Threading::DETACHED_PROCESS,
        );
    }
    let mut child = c.spawn().context("starting proxy daemon")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if proxy_running() {
            break;
        }
        if let Some(status) = child.try_wait()? {
            if proxy_running() {
                break;
            }
            bail!(
                "proxy exited {status}: {}",
                fs::read_to_string(state::dir().join("proxy.log")).unwrap_or_default()
            )
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            let log = state::dir().join("proxy.log");
            let text = fs::read_to_string(&log).unwrap_or_default();
            let tail: Vec<&str> = text.lines().rev().take(20).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            bail!(
                "proxy readiness timed out; see {}{}",
                log.display(),
                if tail.is_empty() {
                    " (empty)".to_owned()
                } else {
                    format!(":\n{}", tail.join("\n"))
                }
            )
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    eprintln!(
        "{}",
        crate::style::dim(
            crate::style::Stream::Stderr,
            &format!(
                "Porch proxy ready on {}",
                s.url("<name>").replace("<name>.", "*.")
            )
        )
    );
    trust_if_needed(&s, skip_trust).await?;
    Ok(())
}
async fn trust_if_needed(s: &ProxySettings, skip: bool) -> Result<()> {
    // Per-user trust needs no sudo, so high-port (unprivileged) HTTPS is trusted too.
    if s.tls
        && s.cert.is_none()
        && !skip
        && enabled("SKIP_TRUST") != Some(true)
        && !matches!(
            crate::integrations::ca_trust(),
            crate::integrations::CaTrust::Trusted(_) | crate::integrations::CaTrust::Unverified(_)
        )
    {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() || std::env::var_os("CI").is_some() {
            if s.unprivileged {
                // Unattended no-sudo starts (menu bar, scripts) keep working; browsers warn until trusted.
                eprintln!("Browsers will warn about {} until you run `porch trust` once in a terminal (no sudo).", s.url("<app>"));
                return Ok(());
            }
            bail!("local CA is not trusted; run `porch trust` interactively once, or use --skip-trust / PORCH_SKIP_TRUST=1 with a client that loads the generated CA")
        }
        integration("trust", vec![]).await?;
    }
    Ok(())
}
async fn ensure_proxy() -> Result<ProxySettings> {
    let o = ProxyOptions::default();
    let s = resolve_settings(&o)?;
    start_proxy(s, enabled("SKIP_TRUST") == Some(true), port_explicit(&o)).await?;
    state::proxy_settings()
}
pub(crate) async fn stop_proxy() -> Result<()> {
    let _startup = startup_lock().await?;
    // Tell running launchers this stop is deliberate, so they don't restart it.
    let _ = state::atomic_write("proxy.stopped", b"1\n");
    if let Some(i) = proxy_identity() {
        if state::matches(&i) {
            #[cfg(unix)]
            if unsafe { libc::kill(i.pid as i32, 15) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            {
                drop(_startup);
                let code = elevate(&["proxy".into(), "stop".into()]).await?;
                if code != 0 {
                    bail!("elevated proxy stop exited {code}")
                }
                return Ok(());
            }
            #[cfg(not(unix))]
            signal_pid(i.pid, 15);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while state::matches(&i) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            if state::matches(&i) {
                signal_pid(i.pid, 9);
                let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                while state::matches(&i) && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                if state::matches(&i) {
                    bail!("proxy could not be stopped; run as its owner or an administrator")
                }
            }
        }
    }
    // Process exit and lock release are separate kernel observations. Hold the
    // startup lock until both are complete before another daemon can start.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let _stopped = loop {
        match daemon_lock() {
            Ok(lock) => break lock,
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(e).context("proxy lock is still held after stop");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    for f in [
        "proxy.pid",
        "proxy.identity",
        "proxy.ready",
        "proxy.version",
    ] {
        match fs::remove_file(state::dir().join(f)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn signal_pid(pid: u32, signal: i32) {
    if pid == 0 || pid > i32::MAX as u32 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, signal);
    };
    // Only Porch processes are signalled by PID; they listen for stop requests.
    #[cfg(windows)]
    if signal == 9 || !crate::shutdown::request(pid) {
        crate::shutdown::kill_tree(pid);
    }
}
fn signal_group(pid: u32, signal: i32) {
    if pid == 0 || pid > i32::MAX as u32 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), signal);
    };
    // Children lead their own console process group. Ctrl+Break reaches them
    // from the launcher that shares their console; from anywhere else, ask
    // their launcher instead (callers do) and force only when told to.
    #[cfg(windows)]
    if signal == 9 {
        crate::shutdown::kill_tree(pid);
    } else {
        crate::shutdown::interrupt_group(pid);
    }
}
struct TerminationSignals {
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hup: tokio::signal::unix::Signal,
    #[cfg(windows)]
    int: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    brk: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    close: tokio::signal::windows::CtrlClose,
}
impl TerminationSignals {
    fn new() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Self {
                int: signal(SignalKind::interrupt())?,
                term: signal(SignalKind::terminate())?,
                hup: signal(SignalKind::hangup())?,
            })
        }
        #[cfg(windows)]
        {
            crate::shutdown::listen()?;
            Ok(Self {
                int: tokio::signal::windows::ctrl_c()?,
                brk: tokio::signal::windows::ctrl_break()?,
                close: tokio::signal::windows::ctrl_close()?,
            })
        }
    }
    async fn recv(&mut self) -> Result<i32> {
        #[cfg(unix)]
        {
            tokio::select! {_=self.int.recv()=>Ok(2),_=self.term.recv()=>Ok(15),_=self.hup.recv()=>Ok(1)}
        }
        #[cfg(windows)]
        {
            // A stop request from another Porch process is the SIGTERM equivalent.
            tokio::select! {
                _=self.int.recv()=>Ok(2),
                _=self.brk.recv()=>Ok(2),
                _=self.close.recv()=>Ok(1),
                _=crate::shutdown::requested()=>Ok(15),
            }
        }
    }
}
fn list(json: bool) -> Result<()> {
    let (_, rows) = crate::health::reconcile()?;
    let s = state::load()?;
    let p = state::proxy_settings()?;
    let routes: Vec<_> = rows
        .iter()
        .map(|row| {
            let r = &row.route;
            serde_json::json!({
                "name": r.name,
                "url": p.url(&r.name),
                "target": r.target,
                "pid": r.pid,
                "urls": s.shares.get(&r.name),
                "health": row.health.label(),
                "private": s.private.contains(&r.name),
                "stopped_seconds": row.stopped_for.map(|d| d.as_secs()),
            })
        })
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "routes": routes,
                "proxy": p,
                "running": proxy_running(),
                "proxy_version": running_proxy_version(),
            }))?
        )
    } else if routes.is_empty() {
        println!("No active routes")
    } else {
        use crate::style::{self, Stream::Stdout};
        if !proxy_running() {
            println!(
                "{} {}",
                style::yellow(Stdout, "!"),
                style::yellow(
                    Stdout,
                    "The proxy isn't running, so these URLs won't answer. Start it with `porch proxy start` (or launch an app)."
                )
            );
        }
        let width = routes
            .iter()
            .map(|r| r["url"].as_str().unwrap_or("").len())
            .max()
            .unwrap_or(0)
            .max(3);
        println!(
            "{}",
            style::dim(
                Stdout,
                &format!("  {:<14} {:<width$}  TARGET", "HEALTH", "URL")
            )
        );
        for (row, r) in rows.iter().zip(&routes) {
            let label = format!("{:<14}", row.health.label());
            let (dot, label) = match row.health {
                crate::health::Health::Ok => {
                    (style::green(Stdout, "●"), style::green(Stdout, &label))
                }
                crate::health::Health::NotListening | crate::health::Health::Stopped => {
                    (style::yellow(Stdout, "●"), style::yellow(Stdout, &label))
                }
                crate::health::Health::Dead { .. } => {
                    (style::red(Stdout, "●"), style::red(Stdout, &label))
                }
            };
            let url = r["url"].as_str().unwrap_or("");
            println!(
                "{dot} {label} {}{}  {}",
                style::url(Stdout, url),
                " ".repeat(width.saturating_sub(url.len())),
                style::dim(
                    Stdout,
                    &format!(
                        "{}{}",
                        r["target"].as_str().unwrap_or(""),
                        r["pid"]
                            .as_u64()
                            .map(|p| format!(" (pid {p})"))
                            .unwrap_or_default()
                    )
                )
            );
            if let Some(urls) = r["urls"].as_object() {
                for url in urls.values() {
                    println!(
                        "                   {}",
                        style::url(Stdout, url.as_str().unwrap_or(""))
                    );
                }
            }
            let hint = match row.health {
                crate::health::Health::Ok => None,
                crate::health::Health::NotListening => Some(
                    "nothing accepts connections on the target; make sure the app uses the PORT env var, then run `porch doctor`".to_owned(),
                ),
                crate::health::Health::Stopped => Some(format!(
                    "processes are stopped{} (frozen by job control); run `porch doctor --fix`",
                    row.stopped_for
                        .map(|d| format!(" for {}s", d.as_secs()))
                        .unwrap_or_default()
                )),
                crate::health::Health::Dead { .. } => Some(
                    "its launcher exited but app processes remain; run `porch prune --force`".to_owned(),
                ),
            };
            if let Some(hint) = hint {
                println!("  {}", style::dim(Stdout, &hint));
            }
        }
    }
    Ok(())
}
fn alias_names(name: &str, selected: &[String]) -> Result<Vec<String>> {
    let n = normalize(name)?;
    let settings = state::proxy_settings()?;
    if selected.is_empty() {
        return Ok(vec![n]);
    }
    let ts = tlds(selected)?;
    for t in &ts {
        if !settings.tlds.contains(t) {
            bail!("TLD {t} is not configured on the proxy")
        }
    }
    Ok(ts.iter().map(|t| format!("{n}.{t}")).collect())
}
fn route_hostnames(name: &str, p: &ProxySettings) -> Vec<String> {
    if p.lan || p.tlds.iter().any(|t| name.ends_with(&format!(".{t}"))) {
        return vec![p.hostname(name)];
    }
    p.tlds.iter().map(|t| format!("{name}.{t}")).collect()
}
fn conflicting_names(s: &state::State, names: &[String], p: &ProxySettings) -> Vec<String> {
    let hosts: Vec<_> = names.iter().flat_map(|n| route_hostnames(n, p)).collect();
    s.routes
        .iter()
        .filter(|r| {
            route_hostnames(&r.name, p)
                .iter()
                .any(|h| hosts.contains(h))
        })
        .map(|r| r.name.clone())
        .collect()
}
async fn alias(
    name: &str,
    target: &str,
    force: bool,
    selected: &[String],
    upstream: Option<state::Upstream>,
) -> Result<()> {
    if upstream.is_some() && target.parse::<u16>().is_err() {
        bail!("--upstream needs a port target (it always dials this computer), e.g. `porch alias api 8443 --upstream https`")
    }
    let target = if let Ok(p) = target.parse::<u16>() {
        if p == 0 {
            bail!("port must be nonzero")
        }
        format!("http://127.0.0.1:{p}")
    } else if !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()) {
        bail!("port {target} is out of range; use 1-65535")
    } else {
        let u = url::Url::parse(target).with_context(|| {
            format!("target {target:?} must be a port (3000) or a URL (http://127.0.0.1:3000)")
        })?;
        if u.query().is_some() || u.fragment().is_some() {
            bail!("target {target:?} can't have a query or fragment; Porch forwards each request's own path and query")
        }
        if !["http", "https"].contains(&u.scheme()) || u.host_str().is_none() {
            bail!("target must be an HTTP URL or TCP port")
        }
        if !u.username().is_empty() || u.password().is_some() {
            bail!("target {target:?} can't contain credentials; put them in your app's config instead")
        }
        // Store the normalized, percent-encoded form so the proxy can always parse it.
        u.as_str().trim_end_matches('/').to_owned()
    };
    let names = alias_names(name, selected)?;
    let p = state::proxy_settings()?;
    evict_conflicts(&names, &p, force).await?;
    state::transaction(|s| {
        let conflicts = conflicting_names(s, &names, &p);
        for n in &conflicts {
            if let Some(r) = s.routes.iter().find(|r| &r.name == n) {
                if state::route_alive(s, r) && (!force || r.pid.is_some()) {
                    bail!("route {n} already exists; use --force")
                }
            }
        }
        // Preserve unaffected TLDs when replacing one part of a static multi-TLD alias.
        let incoming: Vec<_> = names.iter().flat_map(|n| route_hostnames(n, &p)).collect();
        for n in conflicts {
            let old = s.routes.iter().find(|r| r.name == n).cloned();
            let was_private = s.private.contains(&n);
            let old_upstream = s.upstreams.get(&n).cloned();
            state::remove_route(s, &n);
            if !selected.is_empty() {
                if let Some(old) = old.filter(|r| r.pid.is_none()) {
                    for host in route_hostnames(&old.name, &p)
                        .into_iter()
                        .filter(|h| !incoming.contains(h))
                    {
                        if was_private {
                            s.private.insert(host.clone());
                        }
                        if let Some(up) = &old_upstream {
                            s.upstreams.insert(host.clone(), up.clone());
                        }
                        s.routes.push(Route {
                            name: host,
                            target: old.target.clone(),
                            pid: None,
                        });
                    }
                }
            }
        }
        for n in &names {
            state::remove_route(s, n);
            s.routes.push(Route {
                name: n.clone(),
                target: target.clone(),
                pid: None,
            });
            if enabled("PRIVATE") == Some(true) {
                s.private.insert(n.clone());
            }
            if let Some(up) = &upstream {
                s.upstreams.insert(n.clone(), up.clone());
            }
        }
        Ok(())
    })?;
    let shown = resolve_settings(&ProxyOptions::default()).unwrap_or(p);
    for n in &names {
        println!("{} -> {target}", shown.url(n));
    }
    Ok(())
}
fn remove_alias(name: &str, selected: &[String]) -> Result<()> {
    let names = alias_names(name, selected)?;
    let p = state::proxy_settings()?;
    let removing: Vec<_> = names.iter().flat_map(|n| route_hostnames(n, &p)).collect();
    state::transaction(|s| {
        let old: Vec<_> = s
            .routes
            .iter()
            .filter(|r| {
                r.pid.is_none()
                    && route_hostnames(&r.name, &p)
                        .iter()
                        .any(|h| removing.contains(h))
            })
            .cloned()
            .collect();
        if old.is_empty() {
            bail!("no matching static alias found for {name}")
        }
        for r in old {
            let was_private = s.private.contains(&r.name);
            let old_upstream = s.upstreams.get(&r.name).cloned();
            state::remove_route(s, &r.name);
            for host in route_hostnames(&r.name, &p)
                .into_iter()
                .filter(|h| !removing.contains(h))
            {
                if was_private {
                    s.private.insert(host.clone());
                }
                if let Some(up) = &old_upstream {
                    s.upstreams.insert(host.clone(), up.clone());
                }
                s.routes.push(Route {
                    name: host,
                    target: r.target.clone(),
                    pid: None,
                });
            }
        }
        Ok(())
    })
}
fn kill_route(s: &state::State, name: &str, signal: i32) -> Result<()> {
    if let Some(i) = s.owners.get(name) {
        if let Some(child) = i.child {
            // Windows: only a group's own launcher sends it Ctrl+Break. The
            // recorded child can be a daemon that took over the route and
            // leads no console group, so Ctrl+Break to its id reaches no one.
            // Callers ask the owning launcher to stop instead (`signal_pid`),
            // and force with SIGKILL semantics.
            let allowed = cfg!(unix) || signal == 9;
            if allowed && state::group_matches(child, i.child_started.as_deref()) {
                signal_group(child, signal);
            }
        }
    } else if s
        .routes
        .iter()
        .any(|r| r.name == name && r.pid.is_some_and(state::alive))
    {
        bail!("legacy route lacks process identity; cannot safely kill its process")
    }
    Ok(())
}
async fn evict_conflicts(names: &[String], settings: &ProxySettings, force: bool) -> Result<()> {
    let s = state::load()?;
    let conflicts = conflicting_names(&s, names, settings);
    for n in &conflicts {
        let r = s.routes.iter().find(|r| &r.name == n).unwrap();
        if state::route_alive(&s, r) && !force {
            bail!("route {n} already exists; use --force")
        }
    }
    for n in conflicts {
        kill_route(&s, &n, 15)?;
        if let Some(owner) = s.owners.get(&n) {
            if state::matches(owner) && state::process_owned(owner.pid) {
                signal_pid(owner.pid, 15);
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
            while (state::matches(owner)
                || owner
                    .child
                    .is_some_and(|pid| state::group_matches(pid, owner.child_started.as_deref())))
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            kill_route(&s, &n, 9)?;
            if state::matches(owner) && state::process_owned(owner.pid) {
                signal_pid(owner.pid, 9);
            }
            // The route, and a fixed port its app held, are only free once
            // the old launcher and its app group are gone. A killed process
            // exits once the kernel finishes with it, which on a loaded
            // machine can take well over a second; the bound only catches a
            // process stuck in the kernel, and then the takeover fails rather
            // than racing the old app for its port.
            let gone = || {
                !state::matches(owner)
                    && owner.child.is_none_or(|pid| {
                        !state::group_matches(pid, owner.child_started.as_deref())
                    })
            };
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !gone() {
                if tokio::time::Instant::now() >= deadline {
                    bail!("route {n}: its launcher or app did not exit after SIGKILL; retry once it has")
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    Ok(())
}
/// Terminate launches (including job-control stopped ones) and drop their routes.
pub(crate) async fn terminate_routes(names: &[String]) -> Result<()> {
    let s = state::load()?;
    for n in names {
        kill_route(&s, n, 15)?;
        if let Some(owner) = s.owners.get(n) {
            if state::matches(owner) && state::process_owned(owner.pid) {
                signal_pid(owner.pid, 15);
                #[cfg(unix)]
                signal_pid(owner.pid, libc::SIGCONT);
            }
            #[cfg(unix)]
            if let Some(child) = owner.child {
                if state::group_matches(child, owner.child_started.as_deref()) {
                    // Resume stopped members so the pending SIGTERM is delivered.
                    signal_group(child, libc::SIGCONT);
                }
            }
        }
    }
    let settings = state::proxy_settings()?;
    evict_conflicts(names, &settings, true).await?;
    state::mutate(|st| {
        for n in names {
            if st
                .routes
                .iter()
                .find(|r| &r.name == n)
                .is_some_and(|r| !state::route_alive(st, r))
            {
                state::remove_route(st, n);
            }
        }
    })?;
    crate::health::reconcile_quietly();
    Ok(())
}
pub(crate) async fn prune(force: bool) -> Result<()> {
    let old = state::load()?;
    let names: Vec<_> = old
        .routes
        .iter()
        .filter(|r| r.pid.is_some() && !state::route_alive(&old, r))
        .map(|r| r.name.clone())
        .collect();
    for n in names {
        kill_route(&old, &n, if force { 9 } else { 15 })?;
        if let Some(owner) = old.owners.get(&n) {
            if let Some(pid) = owner.child {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                while state::group_matches(pid, owner.child_started.as_deref())
                    && tokio::time::Instant::now() < deadline
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                if state::group_matches(pid, owner.child_started.as_deref()) {
                    eprintln!("{n} has surviving processes; run porch prune --force");
                    continue;
                }
            }
        }
        state::mutate(|s| {
            if s.routes.iter().find(|r| r.name == n).map(|r| r.pid)
                == old.routes.iter().find(|r| r.name == n).map(|r| r.pid)
            {
                state::remove_route(s, &n);
            }
        })?;
    }
    Ok(())
}
async fn sync_hosts() {
    if let Err(e) = crate::integrations::request_hosts_sync().await {
        eprintln!("Hosts sync: {e}");
    }
}

struct Lease {
    name: String,
    owner: u32,
    listener: Option<crate::sockets::Reservation>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = state::mutate(|s| {
            if s.routes
                .iter()
                .any(|r| r.name == self.name && r.pid == Some(self.owner))
            {
                state::remove_route(s, &self.name)
            }
        });
    }
}
/// Default range for automatically assigned app ports.
const APP_PORTS: (u16, u16) = (4000, 4999);
/// Ports browsers refuse to fetch (lockd, sieve); never assigned automatically.
const BROWSER_BLOCKED_PORTS: [u16; 2] = [4045, 4190];
/// The inclusive range automatic app ports come from: `PORCH_APP_PORTS=START-END`,
/// or 4000-4999.
fn app_port_range() -> Result<(u16, u16)> {
    parse_app_ports(state::env("APP_PORTS").as_deref())
}
fn parse_app_ports(value: Option<&str>) -> Result<(u16, u16)> {
    let Some(value) = value else {
        return Ok(APP_PORTS);
    };
    let parsed = value.trim().split_once('-').and_then(|(a, b)| {
        let (a, b) = (a.trim().parse::<u16>().ok()?, b.trim().parse::<u16>().ok()?);
        (crate::ports::FIRST_UNPRIVILEGED_PORT <= a && a <= b).then_some((a, b))
    });
    parsed.with_context(|| {
        format!(
            "PORCH_APP_PORTS must be START-END with 1024 <= START <= END <= 65535, not {value:?}"
        )
    })
}
/// Every assignable port in `range`, starting at a random one so concurrent
/// launches from different state directories rarely probe the same port first.
fn app_port_candidates((first, last): (u16, u16)) -> Result<impl Iterator<Item = u16>> {
    let len = u32::from(last - first) + 1;
    let start = getrandom::u32().map_err(|e| anyhow::anyhow!("secure random: {e}"))? % len;
    Ok((0..len)
        .map(move |i| first + ((start + i) % len) as u16)
        .filter(|p| !BROWSER_BLOCKED_PORTS.contains(p) && *p != crate::ports::DISCOVERY_PORT))
}
fn reserve(
    plan: &crate::config::LaunchPlan,
    requested: Option<u16>,
    force: bool,
) -> Result<(u16, Lease)> {
    let owner = std::process::id();
    let identity = state::identity(owner).context("cannot identify launcher")?;
    let name = normalize(&plan.name)?;
    let settings = state::proxy_settings()?;
    let listener = state::transaction(|s| {
        let conflicts = conflicting_names(s, std::slice::from_ref(&name), &settings);
        for n in conflicts {
            if let Some(r) = s.routes.iter().find(|r| r.name == n) {
                if state::route_alive(s, r) && (!force || r.pid.is_some()) {
                    bail!("route {n} already exists; use --force")
                }
            }
            state::remove_route(s, &n);
        }
        let used: Vec<u16> = s
            .routes
            .iter()
            .filter(|r| r.name != name && state::route_alive(s, r))
            .filter_map(|r| url::Url::parse(&r.target).ok()?.port_or_known_default())
            .collect();
        let listener = if let Some(port) = requested {
            if used.contains(&port) {
                bail!("app port {port} is already reserved by another route")
            }
            crate::sockets::reserve(port).with_context(|| format!("app port {port} unavailable"))?
        } else {
            let range = app_port_range()?;
            let mut last_error = None;
            app_port_candidates(range)?
                .filter(|p| !used.contains(p))
                .find_map(|p| {
                    crate::sockets::reserve(p)
                        .map_err(|e| last_error = Some(e))
                        .ok()
                })
                .with_context(|| {
                    let (first, last) = range;
                    match last_error {
                        // Name the cause when binding failed for another reason
                        // (for example, out of file descriptors).
                        Some(e) if e.kind() != std::io::ErrorKind::AddrInUse => {
                            format!("no app port in {first}-{last} could be reserved: {e}")
                        }
                        _ => format!("all app ports {first}-{last} are occupied"),
                    }
                })?
        };
        let port = listener.port()?;
        state::remove_route(s, &name);
        s.routes.push(Route {
            name: name.clone(),
            target: format!("http://127.0.0.1:{port}"),
            pid: Some(owner),
        });
        if enabled("PRIVATE") == Some(true) {
            s.private.insert(name.clone());
        }
        s.owners.insert(name.clone(), identity);
        Ok(listener)
    })?;
    let port = listener.port()?;
    Ok((
        port,
        Lease {
            name,
            owner,
            listener: Some(listener),
        },
    ))
}
struct Prepared {
    plan: crate::config::LaunchPlan,
    port: Option<u16>,
    lease: Option<Lease>,
    env: BTreeMap<String, String>,
    shares: Option<crate::integrations::ShareSession>,
}
fn child_env(port: u16, name: &str, s: &ProxySettings) -> BTreeMap<String, String> {
    let mut e = BTreeMap::new();
    e.insert("PORT".into(), port.to_string());
    e.insert("PORTLESS_URL".into(), s.url(name));
    e.insert("PORCH_URL".into(), s.url(name));
    e.insert(
        "__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS".into(),
        s.tlds
            .iter()
            .map(|t| format!(".{t}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    if s.lan {
        e.insert("PORTLESS_LAN".into(), "1".into());
        e.insert("PORCH_LAN".into(), "1".into());
    }
    if s.tls && state::dir().join("ca.pem").is_file() {
        e.insert(
            "NODE_EXTRA_CA_CERTS".into(),
            state::dir().join("ca.pem").to_string_lossy().into(),
        );
    }
    e
}
fn child_status(s: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(1))
    }
    #[cfg(windows)]
    {
        s.code().unwrap_or(1)
    }
}
async fn spawn_prepared(
    mut p: Prepared,
    mut shutdown: tokio::sync::watch::Receiver<Option<i32>>,
    bypass: bool,
    lan: bool,
) -> Result<i32> {
    let (command, host) = if let Some(port) = p.port {
        crate::config::adapt_command(&p.plan.command, &p.plan.cwd, port, lan)?
    } else {
        (p.plan.command.clone(), None)
    };
    let first = command.first().context("empty child command")?;
    let path = crate::config::augmented_path(&p.plan.cwd);
    let program = child_program(first, &p.plan.cwd, &path);
    let mut c = tokio::process::Command::new(program);
    c.args(&command[1..])
        .current_dir(&p.plan.cwd)
        .env("PATH", crate::config::augmented_path(&p.plan.cwd))
        .envs(&p.env)
        .kill_on_drop(true);
    if let Some(h) = host {
        c.env("HOST", h);
    } else if p.port.is_some() {
        c.env_remove("HOST");
    }
    if !bypass && p.port.is_some() {
        c.env("PORCH_ACTIVE", "1").env("PORTLESS_ACTIVE", "1");
    }
    crate::process::configure(&mut c);
    if let Some(l) = p.lease.as_mut() {
        l.listener.take();
    }
    let mut child = c
        .spawn()
        .with_context(|| format!("starting {} in {}", command.join(" "), p.plan.cwd.display()))?;
    let id = child
        .id()
        .context("child exited before ownership recorded")?;
    let mut tree = Some(crate::process::TreeGuard::new(&child)?);
    if let Some(l) = &p.lease {
        state::mutate(|s| {
            if let Some(i) = s.owners.get_mut(&l.name).filter(|i| i.pid == l.owner) {
                i.child = Some(id);
                i.child_started = state::identity(id).map(|p| p.started);
            }
        })?;
        {
            use crate::style::{self, Stream::Stderr};
            eprintln!(
                "{} {}",
                style::url(Stderr, p.env.get("PORCH_URL").unwrap_or(&p.plan.name)),
                style::dim(Stderr, &format!("-> {}", p.port.unwrap()))
            );
        }
    }
    // Keep the proxy up while this app runs: if it dies without a deliberate
    // `porch proxy stop`, start it again with the saved settings.
    let supervisor = p.lease.is_some().then(|| {
        tokio::spawn(async {
            let mut was_running = proxy_running();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let running = proxy_running();
                if was_running && !running && !state::dir().join("proxy.stopped").exists() {
                    eprintln!("porch: the proxy stopped unexpectedly; restarting it");
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = tokio::process::Command::new(exe)
                            .args(["proxy", "start"])
                            .env("PORCH_SKIP_TRUST", "1")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .status()
                            .await;
                    }
                }
                was_running = proxy_running();
            }
        })
    });
    let listen_check = p.port.filter(|_| p.lease.is_some()).map(|port| {
        // The suggested rerun keeps this exact name (worktree prefix included).
        let quote = |a: &str| {
            if !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
            {
                a.to_owned()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        };
        tokio::spawn(crate::health::listen_check(
            port,
            id,
            format!(
                "--name {} --prefix '' {}",
                quote(&p.plan.name),
                p.plan
                    .command
                    .iter()
                    .map(|a| quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        ))
    });
    // The child is reaped only at the end (see `process::wait_exit`): until
    // then its pid still names its process group, which the cleanup below
    // signals.
    let code = tokio::select! {status=crate::process::wait_exit(&mut child)=>child_status(status?),failure=share_failure(&mut p.shares)=>{signal_group(id,15);let _=tokio::time::timeout(Duration::from_secs(3),crate::process::wait_exit(&mut child)).await;failure?;bail!("sharing tunnel stopped")},changed=shutdown.changed()=>{changed?;let sig=(*shutdown.borrow()).unwrap_or(15);signal_group(id,sig);match tokio::time::timeout(Duration::from_secs(3),crate::process::wait_exit(&mut child)).await{Ok(r)=>{r?;},Err(_)=>{signal_group(id,9);crate::process::wait_exit(&mut child).await?;}}128+sig}};
    if let Some(task) = listen_check {
        task.abort();
    }
    if let Some(task) = supervisor {
        task.abort();
    }
    let mut transferred = false;
    if code == 0 && shutdown.borrow().is_none() {
        if let (Some(port), Some(lease)) = (p.port, p.lease.as_ref()) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            let listener = loop {
                if let Some(pid) = crate::process::listening_process(port).filter(|pid| *pid != id)
                {
                    break Some(pid);
                }
                if tokio::time::Instant::now() >= deadline {
                    break None;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            };
            if let Some(pid) = listener {
                if let Some(mut identity) = state::identity(pid) {
                    let group_started = state::load()?
                        .owners
                        .get(&lease.name)
                        .and_then(|i| i.child_started.clone());
                    #[cfg(unix)]
                    let group = unsafe { libc::getpgid(pid as i32) } as u32;
                    #[cfg(windows)]
                    let group = pid;
                    let retain_original_group = cfg!(windows) || group == id;
                    identity.child = Some(group);
                    identity.child_started = if group == id {
                        group_started
                    } else {
                        state::identity(group).map(|i| i.started)
                    };
                    let result = state::transaction(|s| {
                        if shutdown.borrow().is_some() {
                            return Ok(());
                        }
                        if let Some(r) = s
                            .routes
                            .iter_mut()
                            .find(|r| r.name == lease.name && r.pid == Some(lease.owner))
                        {
                            r.pid = Some(pid);
                            s.owners.insert(lease.name.clone(), identity);
                            s.shares.remove(&lease.name);
                            transferred = true;
                        }
                        Ok(())
                    });
                    if result.is_err() || !transferred {
                        signal_group(group, 9);
                        result?;
                    }
                    if transferred && retain_original_group {
                        if let Some(guard) = tree.take() {
                            guard.release()?;
                        }
                    }
                }
            }
        }
    }
    if !transferred {
        signal_group(id, 15);
    } // Dispose descendants unless ownership moved to a daemon.
    drop(tree.take());
    // Reap last: the group is disposed of (or handed to the daemon) above.
    let _ = child.wait().await;
    drop(p.shares.take());
    drop(p.lease.take());
    sync_hosts().await;
    Ok(code)
}
async fn launch(options: RunOptions, command: Vec<String>) -> Result<i32> {
    launch_extra(options, command, vec![]).await
}
fn log_path(name: &str) -> PathBuf {
    state::dir().join("logs").join(format!("{name}.log"))
}
/// `porch run --detach`: start this same command as a background launcher
/// writing to a log, wait until its app answers, print the URL and return.
async fn detach(options: &RunOptions, command: &[String]) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let plans = crate::config::launch_plans(
        &cwd,
        (!command.is_empty()).then(|| command.to_vec()),
        options.name.as_deref(),
        options.script.as_deref(),
        options.prefix.as_deref(),
    )?;
    let names: Vec<String> = plans
        .iter()
        .filter(|p| p.proxy)
        .map(|p| p.name.clone())
        .collect();
    let log_name = names.first().cloned().unwrap_or_else(|| "workspace".into());
    let log = log_path(&log_name);
    state::load()?;
    fs::create_dir_all(log.parent().unwrap())?;
    let out = fs::File::create(&log)?;
    let mut child = Command::new(std::env::current_exe()?);
    child
        .args(std::env::args_os().skip(1))
        .env("PORCH_DETACHED", "1")
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session: closing this terminal doesn't stop the app.
        unsafe {
            child.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        child.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let mut child = child.spawn().context("starting the background launcher")?;
    let settings = resolve_settings(&ProxyOptions::default())?;
    let timeout = crate::health::listen_timeout().max(Duration::from_secs(5));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let tail = fs::read_to_string(&log).unwrap_or_default();
            let tail: Vec<_> = tail.lines().rev().take(20).collect();
            for line in tail.into_iter().rev() {
                eprintln!("{line}");
            }
            bail!(
                "the app exited ({status}) before it was ready; full output: {}",
                log.display()
            );
        }
        let s = state::load()?;
        let ready = !names.is_empty()
            && names.iter().all(|n| {
                s.routes
                    .iter()
                    .find(|r| &r.name == n)
                    .is_some_and(|r| crate::health::target_listening(&r.target))
            });
        if ready || names.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "porch: still waiting for the app to answer after {}s; it keeps starting in the background (`porch logs {log_name}`)",
                timeout.as_secs()
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    use crate::style::{self, Stream::Stdout};
    for n in &names {
        println!("{}", style::url(Stdout, &settings.url(n)));
    }
    eprintln!(
        "{}",
        style::dim(
            crate::style::Stream::Stderr,
            &format!("Running in the background. Logs: porch logs {log_name} · Stop: porch stop {log_name}")
        )
    );
    Ok(0)
}
/// `porch stop NAME`: stop the launcher and app behind a route.
async fn stop_app(name: &str) -> Result<i32> {
    let n = normalize(name)?;
    let s = state::load()?;
    let p = state::proxy_settings()?;
    let names = conflicting_names(&s, std::slice::from_ref(&n), &p);
    let launched: Vec<String> = names
        .into_iter()
        .filter(|m| s.routes.iter().any(|r| &r.name == m && r.pid.is_some()))
        .collect();
    if launched.is_empty() {
        if s.routes.iter().any(|r| r.name == n) {
            bail!("{n} is an alias, not an app Porch started; remove it with `porch alias --remove {n}`")
        }
        bail!("no running app named {n}; see `porch list`")
    }
    evict_conflicts(&launched, &p, true).await?;
    crate::health::reconcile_quietly();
    for m in &launched {
        println!("Stopped {}", p.url(m));
    }
    Ok(0)
}
/// `porch logs NAME`: print (and with --follow, keep printing) a background app's output.
async fn show_logs(name: &str, follow: bool, lines: Option<usize>) -> Result<i32> {
    let n = normalize(name)?;
    let path = log_path(&n);
    let text = fs::read_to_string(&path).with_context(|| {
        format!("no log for {n}; logs exist for apps started with `porch run --detach`")
    })?;
    let all: Vec<&str> = text.lines().collect();
    let start = lines.map_or(0, |k| all.len().saturating_sub(k));
    for line in &all[start..] {
        println!("{line}");
    }
    if !follow {
        return Ok(0);
    }
    let mut offset = text.len() as u64;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.len() < offset {
            offset = 0; // the app was restarted and its log recreated
        }
        if meta.len() > offset {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut f = fs::File::open(&path)?;
            f.seek(SeekFrom::Start(offset))?;
            let mut chunk = Vec::new();
            f.read_to_end(&mut chunk)?;
            offset += chunk.len() as u64;
            std::io::stdout().write_all(&chunk)?;
            std::io::stdout().flush()?;
        }
    }
}
async fn launch_extra(
    options: RunOptions,
    command: Vec<String>,
    extra: Vec<String>,
) -> Result<i32> {
    if options.detach && std::env::var_os("PORCH_DETACHED").is_none() {
        let mut all = command.clone();
        all.extend(extra.iter().cloned());
        return detach(&options, &all).await;
    }
    // Subscribe before publishing route ownership, so a signal observed by a
    // supervising caller cannot fall into the registration-to-spawn window.
    let mut signals = TerminationSignals::new()?;
    crate::health::reconcile_quietly();
    let cwd = std::env::current_dir()?;
    let explicit = !command.is_empty();
    if let Some(port) = options.app_port {
        std::env::set_var("PORCH_APP_PORT", port.to_string());
    }
    let mut plans = crate::config::launch_plans(
        &cwd,
        explicit.then_some(command),
        options.name.as_deref(),
        options.script.as_deref(),
        options.prefix.as_deref(),
    )?;
    let bypass = std::env::var("PORCH")
        .ok()
        .or_else(|| std::env::var("PORTLESS").ok())
        .is_some_and(|v| v == "0" || v == "false" || v == "skip");
    if !bypass
        && (std::env::var_os("PORCH_ACTIVE").is_some()
            || std::env::var_os("PORTLESS_ACTIVE").is_some())
    {
        bail!("recursive porch launch detected; configure a separate dev:app script")
    }
    if options.lan {
        std::env::set_var("PORCH_LAN", "1")
    }
    if let Some(ip) = &options.ip {
        std::env::set_var("PORCH_LAN_IP", ip)
    }
    for p in &mut plans {
        if let Some(port) = options.app_port {
            p.app_port = Some(port)
        }
        if bypass {
            p.proxy = false
        }
    }
    let s = if plans.iter().any(|p| p.proxy) {
        ensure_proxy().await?
    } else {
        state::proxy_settings()?
    };
    let mut providers = Vec::new();
    for (name, on) in [
        (
            "tailscale",
            options.tailscale || enabled("TAILSCALE") == Some(true),
        ),
        ("funnel", options.funnel || enabled("FUNNEL") == Some(true)),
        ("ngrok", options.ngrok || enabled("NGROK") == Some(true)),
        (
            "cloudflare",
            options.cloudflare || enabled("CLOUDFLARE") == Some(true),
        ),
    ] {
        if on {
            providers.push(name.to_owned())
        }
    }
    let turbo =
        if !explicit && options.name.is_none() && !bypass && plans.iter().any(|p| p.cwd != cwd) {
            crate::config::turbo_command(&cwd, options.script.as_deref())?
        } else {
            None
        };
    let mut prepared = Vec::new();
    for mut plan in plans {
        let (port, lease) = if plan.proxy {
            evict_conflicts(std::slice::from_ref(&plan.name), &s, options.force).await?;
            let (p, l) = reserve(&plan, plan.app_port, options.force)?;
            (Some(p), Some(l))
        } else {
            (None, None)
        };
        if let (Some(f), Some(port)) = (&options.framework, port) {
            let (args, _) =
                crate::config::adapt_command(std::slice::from_ref(f), &plan.cwd, port, s.lan)?;
            plan.command.extend(args.into_iter().skip(1));
        }
        let mut env = port
            .map(|p| child_env(p, &plan.name, &s))
            .unwrap_or_default();
        let shares = if let Some(port) = port {
            if providers.is_empty() {
                None
            } else {
                let sh = crate::integrations::start_shares(port, &providers).await?;
                env.extend(sh.urls.clone());
                state::mutate(|st| {
                    st.shares.insert(plan.name.clone(), sh.urls.clone());
                })?;
                Some(sh)
            }
        } else {
            None
        };
        prepared.push(Prepared {
            plan,
            port,
            lease,
            env,
            shares,
        });
    }
    sync_hosts().await;
    let (tx, rx) = tokio::sync::watch::channel(None);
    let mut tasks = tokio::task::JoinSet::new();
    let mut turbo_manifest = None;
    if let Some(mut command) = turbo {
        if !extra.is_empty() {
            if command.first().is_some_and(|s| s == "npm")
                && command.get(1).is_some_and(|s| s == "run")
            {
                command.push("--".into())
            }
            command.extend(extra);
        }
        let root_config = crate::config::load(&cwd)?;
        let turbo_script = options
            .script
            .as_deref()
            .or(root_config["script"].as_str())
            .unwrap_or("dev");
        let mut entries = BTreeMap::new();
        for p in &mut prepared {
            let mut env = p.env.clone();
            env.insert("_PORCH_TURBO_SCRIPT".into(), turbo_script.into());
            env.insert(
                "_PORCH_COMMAND".into(),
                serde_json::to_string(&p.plan.command)?,
            );
            if let Some(port) = p.port {
                let (_, host) =
                    crate::config::adapt_command(&p.plan.command, &p.plan.cwd, port, s.lan)?;
                if let Some(host) = host {
                    env.insert("HOST".into(), host);
                }
            }
            entries.insert(p.plan.cwd.clone(), env);
            if let Some(l) = p.lease.as_mut() {
                l.listener.take();
            }
        }
        let (manifest, node_options) = crate::config::write_turbo_manifest(&entries)?;
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert("NODE_OPTIONS".into(), node_options);
        let plan = crate::config::LaunchPlan {
            cwd: cwd.clone(),
            name: "workspace".into(),
            command,
            app_port: None,
            proxy: false,
        };
        // Retain route and sharing leases for the whole Turbo process lifetime.
        let path = crate::config::augmented_path(&cwd);
        let mut process =
            tokio::process::Command::new(child_program(&plan.command[0], &cwd, &path));
        process
            .args(&plan.command[1..])
            .current_dir(&cwd)
            .env("PATH", crate::config::augmented_path(&cwd))
            .envs(env)
            .kill_on_drop(true);
        crate::process::configure(&mut process);
        let mut child = process.spawn()?;
        // Right after spawn: on Windows the child stays suspended until then.
        let tree = crate::process::TreeGuard::new(&child)?;
        let pid = child.id().context("Turbo process exited")?;
        state::mutate(|st| {
            for p in &prepared {
                if let Some(owner) = st.owners.get_mut(&p.plan.name) {
                    owner.child = Some(pid);
                    owner.child_started = state::identity(pid).map(|p| p.started);
                }
            }
        })?;
        let mut r = rx.clone();
        tasks.spawn(async move{
            // Reap last (see `process::wait_exit`): the guard disposes of the
            // group while the child's pid still names it.
            let group=tree;let mut resources=prepared;
            let result=tokio::select!{s=crate::process::wait_exit(&mut child)=>s.map(child_status).map_err(anyhow::Error::from),failure=workspace_share_failure(&mut resources)=>{signal_group(pid,15);let _=tokio::time::timeout(Duration::from_secs(3),crate::process::wait_exit(&mut child)).await;failure.and_then(|_|Err(anyhow::anyhow!("workspace sharing tunnel stopped")))},_=r.changed()=>{let signal=(*r.borrow()).unwrap_or(15);signal_group(pid,signal);let _=tokio::time::timeout(Duration::from_secs(3),crate::process::wait_exit(&mut child)).await;Ok(128+signal)}};
            drop(group);
            let _ = child.wait().await;
            result
        });
        turbo_manifest = Some(manifest);
    } else {
        for p in prepared {
            tasks.spawn(spawn_prepared(p, rx.clone(), bypass, s.lan));
        }
    }
    let mut result = Ok(0);
    let signal = signals.recv();
    tokio::pin!(signal);
    while !tasks.is_empty() {
        tokio::select! {finished=tasks.join_next()=>{if let Some(r)=finished {match r{Ok(Ok(0))=>{},Ok(Ok(code))=>{if result.as_ref().is_ok_and(|v|*v==0){result=Ok(code);let _=tx.send(Some(15));}},Ok(Err(e))=>{if result.as_ref().is_ok_and(|v|*v==0){result=Err(e)}let _=tx.send(Some(15));},Err(e)=>{result=Err(e.into());let _=tx.send(Some(15));}}}},sig=&mut signal=>{let sig=sig?;result=Ok(128+sig);let _=tx.send(Some(sig));while let Some(r)=tasks.join_next().await{if let Err(e)=r {eprintln!("child cleanup: {e}")}}}}
    }
    if let Some(manifest) = turbo_manifest {
        let _ = fs::remove_file(manifest);
        let _ = fs::remove_file(
            state::dir().join(format!("turbo-env-loader-{}.cjs", std::process::id())),
        );
    }
    sync_hosts().await;
    result
}

fn clean_state() -> Result<()> {
    state::transaction(|s| {
        for n in s.owners.keys() {
            kill_route(s, n, 15)?;
        }
        *s = state::State::default();
        Ok(())
    })?;
    for entry in fs::read_dir(state::dir())? {
        let entry = entry?;
        let n = entry.file_name();
        let n = n.to_string_lossy();
        let known = [
            "state.json",
            "proxy.json",
            "proxy.log",
            "proxy.pid",
            "proxy.identity",
            "proxy.ready",
            "proxy.version",
            "ca.pem",
            "ca-key.pem",
            "hosts-sync.token",
            "hosts-sync.secret",
            "proxy.lan",
            "proxy.port",
            "proxy.hosts-sync-token",
            "service.json",
            "tls.lock",
            "tls-profile-version",
            "proxy.hosts-capability",
            "hosts-localhost-hint",
        ]
        .contains(&n.as_ref())
            || n.starts_with("dev-manifest-")
            || n.starts_with("turbo-env-loader-");
        if known && entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn leading_name(args: &[String]) -> bool {
    let mut i = 0;
    let mut found = false;
    while let Some(arg) = args.get(i) {
        if arg == "--name" || arg.starts_with("--name=") {
            found = true;
        }
        if [
            "--name",
            "--prefix",
            "--script",
            "--app-port",
            "--framework",
            "--ip",
        ]
        .contains(&arg.as_str())
        {
            i += 2;
        } else if arg.starts_with("--name=")
            || [
                "--force",
                "--tailscale",
                "--funnel",
                "--ngrok",
                "--cloudflare",
                "--lan",
            ]
            .contains(&arg.as_str())
        {
            i += 1;
        } else {
            break;
        }
    }
    found
}
async fn share_failure(shares: &mut Option<crate::integrations::ShareSession>) -> Result<()> {
    if let Some(s) = shares {
        s.wait_failure().await
    } else {
        std::future::pending().await
    }
}

async fn startup_lock() -> Result<fs::File> {
    state::load()?;
    let mut o = fs::OpenOptions::new();
    o.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let f = o.open(state::dir().join("startup.lock"))?;
    state::inherit_owner(&state::dir().join("startup.lock"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match f.try_lock_exclusive() {
            Ok(()) => return Ok(f),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("another proxy startup did not finish")
        };
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn requires_elevation(name: &str, args: &[String]) -> bool {
    #[cfg(unix)]
    {
        if unsafe { libc::geteuid() } == 0 {
            return false;
        }
        if std::env::var_os("PORCH_HOSTS_FILE").is_some()
            || std::env::var_os("PORCH_SERVICE_FILE").is_some()
            || std::env::var_os("PORCH_TRUST_DIR").is_some()
        {
            return false;
        }
        (name == "trust" && args.iter().any(|a| a == "--system"))
            || (name == "service"
                && args
                    .first()
                    .is_some_and(|a| a == "install" || a == "uninstall"))
            || (name == "hosts" && args.first().is_none_or(|a| a == "sync" || a == "clean"))
    }
    #[cfg(not(unix))]
    {
        let _ = (name, args);
        false
    }
}
async fn elevate(args: &[String]) -> Result<i32> {
    use std::io::IsTerminal;
    let mut c = tokio::process::Command::new("sudo");
    if !std::io::stdin().is_terminal() || std::env::var_os("CI").is_some() {
        c.arg("-n");
    }
    c.arg("env")
        .arg(format!("PORCH_STATE_DIR={}", state::dir().display()));
    for (k, v) in
        std::env::vars().filter(|(k, _)| k.starts_with("PORCH_") || k.starts_with("PORTLESS_"))
    {
        c.arg(format!("{k}={v}"));
    }
    c.arg(std::env::current_exe()?).args(args);
    let status = c.status().await.context(
        "administrator privileges required; install sudo or run the command as administrator",
    )?;
    Ok(child_status(status))
}

async fn workspace_share_failure(resources: &mut [Prepared]) -> Result<()> {
    let waits = resources
        .iter_mut()
        .filter_map(|p| p.shares.as_mut())
        .map(|s| Box::pin(s.wait_failure()))
        .collect::<Vec<_>>();
    if waits.is_empty() {
        return std::future::pending().await;
    }
    let (result, _, _) = futures_util::future::select_all(waits).await;
    result
}
fn clean_needs_elevation() -> bool {
    #[cfg(unix)]
    {
        (unsafe { libc::geteuid() != 0 })
            && std::env::var_os("PORCH_HOSTS_FILE").is_none()
            && std::env::var_os("PORCH_SERVICE_FILE").is_none()
            && std::env::var_os("PORCH_TRUST_DIR").is_none()
            && (crate::integrations::trust_scope().as_deref() == Some("system")
                || state::dir().join("service.json").exists()
                || crate::integrations::has_managed_hosts())
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn bare_separator(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            return Some(i);
        }
        if [
            "--name",
            "--prefix",
            "--script",
            "--app-port",
            "--framework",
            "--ip",
            "--state-dir",
        ]
        .contains(&a.as_str())
        {
            i += 2
        } else if [
            "--force",
            "--tailscale",
            "--funnel",
            "--ngrok",
            "--cloudflare",
            "--lan",
        ]
        .contains(&a.as_str())
            || a.starts_with("--script=")
            || a.starts_with("--app-port=")
        {
            i += 1
        } else {
            break;
        }
    }
    None
}

fn child_program(
    program: &str,
    cwd: &std::path::Path,
    paths: &std::ffi::OsStr,
) -> std::ffi::OsString {
    #[cfg(windows)]
    if std::path::Path::new(program).is_relative() && program.contains(['/', '\\']) {
        return crate::process::resolve_program(&cwd.join(program).to_string_lossy(), paths);
    }
    let _ = cwd;
    crate::process::resolve_program(program, paths)
}

#[cfg(all(test, unix))]
mod elevation_tests {
    use super::*;
    #[test]
    #[ignore = "invoked by the isolated PTY acceptance harness"]
    fn sudo_prompt_child() {
        use std::io::IsTerminal;
        let result = authenticate_sudo(std::io::stdin().is_terminal(), 443);
        if std::env::var_os("PORCH_SUDO_EXPECT_FAILURE").is_some() {
            let message = format!("{:#}", result.unwrap_err());
            assert!(message.contains("terminal"), "{message}");
            assert!(message.contains("--no-tls --port 7462"), "{message}");
            println!("ELEVATION_FAILURE_OK");
        } else {
            result.unwrap();
            println!("ELEVATION_OK");
        }
    }
    #[test]
    fn unattended_or_missing_sudo_offers_explicit_setup_choices() {
        let root = std::env::temp_dir().join(format!("porch-no-sudo-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for installed in [false, true] {
            if installed {
                crate::test_support::write_executable(
                    &root.join("sudo"),
                    "#!/bin/sh\n[ \"$1\" = -n ] && [ \"$2\" = -v ] || exit 99\nexit 1\n",
                );
            }
            let out = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "cli::elevation_tests::sudo_prompt_child",
                    "--nocapture",
                ])
                .env("PATH", &root)
                .env("PORCH_SUDO_EXPECT_FAILURE", "1")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(String::from_utf8_lossy(&out.stdout).contains("ELEVATION_FAILURE_OK"));
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fresh_sudo_credentials_prompt_with_a_terminal() {
        let script = r#"
import os,pty,select,sys,tempfile,time,signal
with tempfile.TemporaryDirectory(prefix='porch-sudo-') as root:
 shim=root+'/sudo'
 with open(shim,'w') as f:
  f.write('#!/usr/bin/env python3\nimport sys,os\nassert sys.argv[1:]==["-v"],sys.argv\nassert sys.stdin.isatty()\nprint("password:",flush=True)\nassert input()=="fixture-password"\n')
 os.chmod(shim,0o755)
 pid,fd=pty.fork()
 if pid==0:
  os.environ['PATH']=root+':'+os.environ['PATH']
  os.execv(sys.argv[1],[sys.argv[1],'--ignored','--exact','cli::elevation_tests::sudo_prompt_child','--nocapture'])
 data=b''; sent=False; deadline=time.monotonic()+10
 try:
  while time.monotonic()<deadline:
   if select.select([fd],[],[],.1)[0]:
    try: chunk=os.read(fd,65536)
    except OSError: break
    if not chunk: break
    data+=chunk
    if b'password:' in data and not sent:
     os.write(fd,b'fixture-password\n'); sent=True
   done,status=os.waitpid(pid,os.WNOHANG)
   if done: break
  assert b'ELEVATION_OK' in data,data.decode(errors='replace')
 finally:
  try: os.kill(pid,signal.SIGKILL)
  except ProcessLookupError: pass
  try: os.waitpid(pid,0)
  except ChildProcessError: pass
  os.close(fd)
"#;
        let out = Command::new("python3")
            .args(["-c", script])
            .arg(std::env::current_exe().unwrap())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[cfg(test)]
mod app_port_tests {
    use super::*;
    #[test]
    fn app_port_range_defaults_and_validates() {
        assert_eq!(parse_app_ports(None).unwrap(), (4000, 4999));
        assert_eq!(
            parse_app_ports(Some(" 20000 - 20031 ")).unwrap(),
            (20000, 20031)
        );
        assert_eq!(parse_app_ports(Some("5000-5000")).unwrap(), (5000, 5000));
        for bad in [
            "",
            "5000",
            "5000-4999",
            "80-90",
            "5000-70000",
            "a-b",
            "-5000",
        ] {
            assert!(parse_app_ports(Some(bad)).is_err(), "{bad:?}");
        }
    }
    #[test]
    fn app_port_candidates_cover_the_range_once_without_blocked_ports() {
        let mut ports: Vec<u16> = app_port_candidates((4000, 4999)).unwrap().collect();
        assert_eq!(ports.len(), 998);
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), 998);
        assert!(!ports.contains(&4045) && !ports.contains(&4190));
        let narrow: Vec<u16> = app_port_candidates((7460, 7464)).unwrap().collect();
        assert_eq!(narrow.len(), 4);
        assert!(!narrow.contains(&crate::ports::DISCOVERY_PORT));
        assert_eq!(
            app_port_candidates((65535, 65535))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![65535]
        );
    }
}
