//! CLI and process lifecycle. Child arguments are parsed only before the command.
use crate::state::{self, ProcessIdentity, ProxySettings, Route};
use anyhow::{bail, Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use fs2::FileExt;
use std::{
    collections::BTreeMap,
    fs,
    net::TcpListener,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Parser, Debug)]
#[command(
    name = "porch",
    version,
    about = "Stable named URLs for local development",
    disable_version_flag = true
)]
struct Cli {
    #[arg(short='v',short_alias='V',long,action=clap::ArgAction::Version)]
    version: (),
    #[command(flatten)]
    options: RunOptions,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Args, Debug, Default, Clone)]
struct RunOptions {
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    script: Option<String>,
    #[arg(long)]
    prefix: Option<String>,
    #[arg(long,value_parser=clap::value_parser!(u16).range(1..))]
    app_port: Option<u16>,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    tailscale: bool,
    #[arg(long)]
    funnel: bool,
    #[arg(long)]
    ngrok: bool,
    #[arg(long)]
    cloudflare: bool,
    #[arg(long)]
    lan: bool,
    #[arg(long)]
    ip: Option<String>,
    #[arg(long)]
    framework: Option<String>,
}
#[derive(Parser, Debug)]
struct App {
    #[command(flatten)]
    options: RunOptions,
    #[arg(trailing_var_arg=true,num_args=0..)]
    command: Vec<String>,
}
#[derive(Args, Debug, Default, Clone)]
struct ProxyOptions {
    #[arg(short='p',long,value_parser=clap::value_parser!(u16).range(1..))]
    port: Option<u16>,
    #[arg(long, conflicts_with = "no_tls")]
    https: bool,
    #[arg(long)]
    no_tls: bool,
    #[arg(long)]
    foreground: bool,
    #[arg(long)]
    skip_trust: bool,
    #[arg(long)]
    unprivileged: bool,
    #[arg(long)]
    lan: bool,
    #[arg(long)]
    ip: Option<String>,
    #[arg(long, value_delimiter = ',')]
    tld: Vec<String>,
    #[arg(long)]
    wildcard: bool,
    #[arg(long, requires = "key")]
    cert: Option<PathBuf>,
    #[arg(long, requires = "cert")]
    key: Option<PathBuf>,
    #[arg(long)]
    bind: Option<String>,
    #[arg(long,value_parser=clap::value_parser!(u16).range(1..))]
    redirect_port: Option<u16>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
}
#[derive(Subcommand, Debug)]
enum ProxyCommand {
    Start(ProxyOptions),
    Stop {
        #[arg(short = 'p', long)]
        port: Option<u16>,
    },
    Status,
}
#[derive(Subcommand, Debug)]
enum Commands {
    Run(App),
    Get {
        name: String,
        #[arg(long)]
        no_worktree: bool,
    },
    #[command(alias = "routes")]
    List {
        #[arg(long)]
        json: bool,
    },
    Alias {
        name: Option<String>,
        target: Option<String>,
        #[arg(long)]
        remove: Option<String>,
        #[arg(long)]
        force: bool,
        #[arg(long, value_delimiter = ',')]
        tld: Vec<String>,
    },
    #[command(alias = "rm")]
    Remove {
        name: String,
        #[arg(long, value_delimiter = ',')]
        tld: Vec<String>,
    },
    Prune {
        #[arg(long)]
        force: bool,
    },
    Clean {
        #[arg(long)]
        force: bool,
    },
    Proxy {
        #[command(subcommand)]
        command: Option<ProxyCommand>,
    },
    Mcp {
        #[arg(long, alias = "tools-profiles")]
        tools: Option<String>,
        profile: Option<String>,
    },
    Hosts {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Service {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Compose {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Share {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Lan,
    Doctor,
    Trust,
    Workspace,
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
    a
}
pub async fn run() -> Result<i32> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // --name explicitly bypasses reserved command dispatch.
    if leading_name(&raw) {
        let app = App::parse_from(std::iter::once("porch".to_string()).chain(raw));
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
        Some(Commands::Run(a)) => return launch(merge(options, a.options), a.command).await,
        Some(Commands::Named(mut args)) => {
            let name = args.remove(0);
            let a = App::parse_from(std::iter::once("porch".into()).chain(args));
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
        Some(Commands::List { json }) => list(json)?,
        Some(Commands::Alias {
            name,
            target,
            remove,
            force,
            tld,
        }) => {
            if let Some(n) = remove {
                remove_alias(&n, &tld)?
            } else {
                alias(
                    &name.context("name required")?,
                    &target.context("port or HTTP target required")?,
                    force,
                    &tld,
                )
                .await?
            }
            sync_hosts().await;
        }
        Some(Commands::Remove { name, tld }) => {
            remove_alias(&name, &tld)?;
            sync_hosts().await;
        }
        Some(Commands::Prune { force }) => {
            prune(force).await?;
            crate::integrations::prune_shares(force)?;
        }
        Some(Commands::Clean { force }) => {
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
                        crate::tls::config(s.cert.clone(), s.key.clone())?;
                    }
                    trust_if_needed(&s, o.skip_trust).await?;
                    return foreground(s).await.map(|_| 0);
                }
                start_proxy(s, o.skip_trust).await?;
            }
            ProxyCommand::Stop { port } => {
                if port.is_some_and(|p| state::proxy_settings().is_ok_and(|s| s.port != p)) {
                    bail!("no proxy at requested port in this state directory")
                }
                stop_proxy().await?;
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
        Some(Commands::Doctor) => integration("doctor", vec![]).await?,
        Some(Commands::Trust) => integration("trust", vec![]).await?,
        Some(Commands::Workspace) => {
            let cwd = std::env::current_dir()?;
            let root = crate::config::find_workspace_root(&cwd).unwrap_or(cwd);
            println!("{:#?}", crate::config::discover_workspace_packages(&root));
        }
    }
    Ok(0)
}
async fn integration(name: &str, args: Vec<String>) -> Result<()> {
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
fn resolve_settings(o: &ProxyOptions) -> Result<ProxySettings> {
    let mut s = state::proxy_settings()?;
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
    if let Some(v) = enabled("UNPRIVILEGED") {
        s.unprivileged = v
    }
    if o.unprivileged {
        s.unprivileged = true
    }
    if s.unprivileged && !tls_explicit {
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
        bail!("invalid proxy port")
    }
    if let Some(p) = explicit_port {
        if p == 0 {
            bail!("proxy port must be 1-65535")
        }
        s.port = p
    } else if (s.port == 443 && !s.tls) || (s.port == 80 && s.tls) {
        s.port = if s.tls { 443 } else { 80 }
    }
    if let Some(b) = &o.bind {
        let _: std::net::IpAddr = b.parse()?;
        s.bind = Some(b.clone())
    }
    if let Some(p) = o.redirect_port {
        s.redirect_port = Some(p)
    } else if s.tls && s.port == 443 {
        s.redirect_port = Some(80)
    }
    if !s.tls {
        s.redirect_port = None
    }
    if s.unprivileged && enabled("SYNC_HOSTS") == Some(true) {
        bail!("SYNC_HOSTS=1 cannot be combined with unprivileged mode")
    }
    Ok(s)
}
fn proxy_identity() -> Option<ProcessIdentity> {
    serde_json::from_slice(&fs::read(state::dir().join("proxy.identity")).ok()?).ok()
}
fn proxy_running() -> bool {
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
            for file in ["proxy.pid", "proxy.identity", "proxy.ready"] {
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
    tokio::select! {r=crate::proxy::serve_config(s)=>r,s=signals.recv()=>{s?;Ok(())}}
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
async fn start_proxy(mut s: ProxySettings, skip_trust: bool) -> Result<()> {
    let _startup = startup_lock().await?;
    if proxy_running() {
        let actual = state::proxy_settings()?;
        if actual != s {
            bail!("running proxy uses different settings; stop it first")
        }
        trust_if_needed(&s, skip_trust).await?;
        return Ok(());
    }
    // Detect privilege requirements before detaching; never leave a false ready marker.
    let bind = s.bind.as_deref().unwrap_or(if s.lan || s.unprivileged {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    });
    let needs_privilege = match TcpListener::bind((bind, s.port)) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => true,
        Err(e) => return Err(e).context("proxy port unavailable"),
    };
    let mut elevated = false;
    if needs_privilege {
        if s.unprivileged {
            bail!(
                "this OS does not permit unprivileged binding on port {}; choose --port 1355",
                s.port
            )
        }
        let sudo = Command::new("sudo")
            .args(["-n", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match sudo {Ok(status) if status.success()=>elevated=true,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{s.port=1355;s.redirect_port=None;eprintln!("sudo unavailable; using port 1355")},_=>bail!("port {} requires privileges; run proxy start interactively with sudo or choose --port 1355",s.port)}
    }
    state::load()?;
    let _ = fs::remove_file(state::dir().join("proxy.ready"));
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state::dir().join("proxy.log"))?;
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
            bail!(
                "proxy readiness timed out; see {}",
                state::dir().join("proxy.log").display()
            )
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    eprintln!("Porch proxy ready on {}", s.url("app"));
    trust_if_needed(&s, skip_trust).await?;
    Ok(())
}
async fn trust_if_needed(s: &ProxySettings, skip: bool) -> Result<()> {
    if s.tls
        && s.cert.is_none()
        && !s.unprivileged
        && !skip
        && enabled("SKIP_TRUST") != Some(true)
        && !state::dir().join("trust.json").exists()
    {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() || std::env::var_os("CI").is_some() {
            bail!("local CA is not trusted; run `porch trust` interactively once, or use --skip-trust / PORCH_SKIP_TRUST=1 with a client that loads the generated CA")
        }
        integration("trust", vec![]).await?;
    }
    Ok(())
}
async fn ensure_proxy() -> Result<ProxySettings> {
    let s = resolve_settings(&ProxyOptions::default())?;
    start_proxy(s.clone(), enabled("SKIP_TRUST") == Some(true)).await?;
    state::proxy_settings()
}
async fn stop_proxy() -> Result<()> {
    let _startup = startup_lock().await?;
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
    for f in ["proxy.pid", "proxy.identity", "proxy.ready"] {
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
    #[cfg(windows)]
    {
        let mut command = Command::new("taskkill");
        command.args(["/PID", &pid.to_string(), "/T"]);
        if signal == 9 {
            command.arg("/F");
        }
        let _ = command.status();
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
    #[cfg(windows)]
    signal_pid(pid, signal);
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
            Ok(Self {
                int: tokio::signal::windows::ctrl_c()?,
                brk: tokio::signal::windows::ctrl_break()?,
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
            tokio::select! {_=self.int.recv()=>Ok(2),_=self.brk.recv()=>Ok(2)}
        }
    }
}
fn list(json: bool) -> Result<()> {
    let s = state::load()?;
    let p = state::proxy_settings()?;
    let routes:Vec<_>=s.routes.iter().filter(|r|state::route_alive(&s,r)).map(|r|serde_json::json!({"name":r.name,"url":p.url(&r.name),"target":r.target,"pid":r.pid,"urls":s.shares.get(&r.name)})).collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"routes":routes,"proxy":p,"running":proxy_running()})
            )?
        )
    } else if routes.is_empty() {
        println!("No active routes")
    } else {
        for r in routes {
            println!(
                "{}  -> {}{}",
                r["url"].as_str().unwrap(),
                r["target"].as_str().unwrap(),
                r["pid"]
                    .as_u64()
                    .map(|p| format!(" (pid {p})"))
                    .unwrap_or_default()
            );
            if let Some(urls) = r["urls"].as_object() {
                for url in urls.values() {
                    println!("  {}", url.as_str().unwrap_or(""));
                }
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
async fn alias(name: &str, target: &str, force: bool, selected: &[String]) -> Result<()> {
    let target = if let Ok(p) = target.parse::<u16>() {
        if p == 0 {
            bail!("port must be nonzero")
        }
        format!("http://127.0.0.1:{p}")
    } else {
        let u = url::Url::parse(target)?;
        if !["http", "https"].contains(&u.scheme()) || u.host_str().is_none() {
            bail!("target must be an HTTP URL or TCP port")
        }
        target.to_owned()
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
            state::remove_route(s, &n);
            if !selected.is_empty() {
                if let Some(old) = old.filter(|r| r.pid.is_none()) {
                    for host in route_hostnames(&old.name, &p)
                        .into_iter()
                        .filter(|h| !incoming.contains(h))
                    {
                        s.routes.push(Route {
                            name: host,
                            target: old.target.clone(),
                            pid: None,
                        });
                    }
                }
            }
        }
        for n in names {
            state::remove_route(s, &n);
            s.routes.push(Route {
                name: n,
                target: target.clone(),
                pid: None,
            });
        }
        Ok(())
    })
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
            state::remove_route(s, &r.name);
            for host in route_hostnames(&r.name, &p)
                .into_iter()
                .filter(|h| !removing.contains(h))
            {
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
            if state::group_matches(child, i.child_started.as_deref()) {
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
            // Kernel release of a forced child's listening sockets can follow its signal briefly.
            if let Some(pid) = owner.child {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
                while state::group_matches(pid, owner.child_started.as_deref())
                    && tokio::time::Instant::now() < deadline
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }
    Ok(())
}
async fn prune(force: bool) -> Result<()> {
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
    listener: Option<TcpListener>,
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
            TcpListener::bind(("127.0.0.1", port))
                .with_context(|| format!("app port {port} unavailable"))?
        } else {
            let start = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .subsec_nanos() as u16
                % 1000;
            (0..1000)
                .map(|i| 4000 + (start + i) % 1000)
                .filter(|p| !used.contains(p) && ![4045, 4190].contains(p))
                .find_map(|p| TcpListener::bind(("127.0.0.1", p)).ok())
                .context("all app ports 4000-4999 are occupied")?
        };
        let port = listener.local_addr()?.port();
        state::remove_route(s, &name);
        s.routes.push(Route {
            name: name.clone(),
            target: format!("http://127.0.0.1:{port}"),
            pid: Some(owner),
        });
        s.owners.insert(name.clone(), identity);
        Ok(listener)
    })?;
    let port = listener.local_addr()?.port();
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
        eprintln!(
            "{} -> {}",
            p.env.get("PORTLESS_URL").unwrap_or(&p.plan.name),
            p.port.unwrap()
        );
    }
    let code = tokio::select! {status=child.wait()=>child_status(status?),failure=share_failure(&mut p.shares)=>{signal_group(id,15);let _=tokio::time::timeout(Duration::from_secs(3),child.wait()).await;failure?;bail!("sharing tunnel stopped")},changed=shutdown.changed()=>{changed?;let sig=(*shutdown.borrow()).unwrap_or(15);signal_group(id,sig);match tokio::time::timeout(Duration::from_secs(3),child.wait()).await{Ok(r)=>{r?;},Err(_)=>{signal_group(id,9);let _=child.wait().await;}}128+sig}};
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
    drop(p.shares.take());
    drop(p.lease.take());
    sync_hosts().await;
    Ok(code)
}
async fn launch(options: RunOptions, command: Vec<String>) -> Result<i32> {
    launch_extra(options, command, vec![]).await
}
async fn launch_extra(
    options: RunOptions,
    command: Vec<String>,
    extra: Vec<String>,
) -> Result<i32> {
    // Subscribe before publishing route ownership, so a signal observed by a
    // supervising caller cannot fall into the registration-to-spawn window.
    let mut signals = TerminationSignals::new()?;
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
        let tree = crate::process::TreeGuard::new(&child)?;
        tasks.spawn(async move{let _group=tree;let mut resources=prepared;let result=tokio::select!{s=child.wait()=>child_status(s?),failure=workspace_share_failure(&mut resources)=>{signal_group(pid,15);let _=tokio::time::timeout(Duration::from_secs(3),child.wait()).await;failure?;bail!("workspace sharing tunnel stopped")},_=r.changed()=>{let signal=(*r.borrow()).unwrap_or(15);signal_group(pid,signal);let _=tokio::time::timeout(Duration::from_secs(3),child.wait()).await;128+signal}};Ok::<_,anyhow::Error>(result)});
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
            "ca.pem",
            "ca-key.pem",
            "hosts-sync.token",
            "hosts-sync.secret",
            "proxy.lan",
            "proxy.port",
            "proxy.hosts-sync-token",
            "service.json",
            "tls.lock",
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
        name == "trust"
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
            && (state::dir().join("trust.json").exists()
                || state::dir().join("service.json").exists()
                || enabled("SYNC_HOSTS") != Some(false))
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
