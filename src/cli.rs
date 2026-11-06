use crate::state::{self, Route};
use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};
fn value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
fn port(args: &[String]) -> u16 {
    value(args, "--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1355)
}
pub async fn run() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let cmd = a.first().map(String::as_str).unwrap_or("help");
    match cmd {
 "help"|"--help"|"-h"=>println!("porch <name> [--port PORT] [--framework FRAMEWORK] -- <command> [args]\nporch proxy start|stop|status [--port PORT] [--https] [--foreground]\nporch alias <name> <target> | remove <name> | get <name> | list | routes | prune | clean\nporch mcp [profile]\nporch hosts|service|share|lan|compose|doctor|trust"),
 "--version"|"-V"=>println!("porch {}",env!("CARGO_PKG_VERSION")),
 "mcp"=>crate::mcp::serve(a.get(1).cloned().unwrap_or_else(||"default".into())).await?,
 "hosts"|"service"|"share"|"lan"|"compose"|"doctor"|"trust"=>crate::integrations::run(a).await?,
 "workspace"=>{let cwd=std::env::current_dir()?;let root=crate::config::find_workspace_root(&cwd).unwrap_or(cwd);println!("{:#?}",crate::config::discover_workspace_packages(&root));},
 "list"|"routes"=>println!("{}",serde_json::to_string_pretty(&state::load()?)?),
 "get"=>{let n=a.get(1).context("name required")?;let s=state::load()?;let r=s.routes.iter().find(|r|&r.name==n).context("route not found")?;println!("{}",r.target);},
 "alias"=>{let n=normalize(a.get(1).context("name required")?)?;let t=a.get(2).context("target required")?.clone();url::Url::parse(&t)?;state::mutate(|s|{s.routes.retain(|r|r.name!=n);s.routes.push(Route{name:n,target:t,pid:None})})?;},
 "remove"|"rm"=>{let n=a.get(1).context("name required")?;state::mutate(|s|s.routes.retain(|r|&r.name!=n))?;},
 "prune"|"clean"=>state::mutate(|s|s.routes.retain(|r|r.pid.is_none_or(state::alive)))?,
 "proxy"=>{let action=a.get(1).map(String::as_str).unwrap_or("status");let p=state::dir().join("proxy.pid");match action {
 "start"=>{if a.iter().any(|x|x=="--foreground"){let _guard=daemon_lock()?;crate::proxy::serve(value(&a,"--bind").unwrap_or_else(||"127.0.0.1".into()),port(&a),a.iter().any(|x|x=="--https"),value(&a,"--cert").map(Into::into),value(&a,"--key").map(Into::into)).await?;}else{state::load()?;if let Ok(pid)=std::fs::read_to_string(&p){if pid.trim().parse().ok().is_some_and(state::alive){bail!("proxy already running")}}let child=Command::new(std::env::current_exe()?).args(&a).arg("--foreground").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;std::fs::write(p,child.id().to_string())?;println!("proxy started {}",child.id());}},
 "stop"=>{let pid:u32=std::fs::read_to_string(&p)?.trim().parse()?;let process=Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]).output()?; let command=String::from_utf8_lossy(&process.stdout);if !command.contains("proxy start") || !command.contains("--foreground") {bail!("PID is not an owned proxy")}
 unsafe{libc::kill(pid as i32,libc::SIGTERM); }std::fs::remove_file(p)?;},
 "status"=>{let up=std::fs::read_to_string(p).ok().and_then(|s|s.trim().parse().ok()).is_some_and(state::alive);println!("{}",if up{"running"}else{"stopped"});},_=>bail!("unknown proxy action")}},
 _=> launch(a).await?,
 }
    Ok(())
}
fn normalize(n: &str) -> Result<String> {
    let n = n.trim_end_matches(".localhost").to_lowercase();
    if n.is_empty()
        || !n
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
    {
        bail!("invalid route name")
    }
    Ok(n)
}
async fn launch(a: Vec<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let root = crate::config::find_workspace_root(&cwd).unwrap_or_else(|| cwd.clone());
    let config = crate::config::load(&root)?;
    crate::config::validate_config(&config)?;
    let effective = crate::config::resolve_app_config(&config, &root, &cwd);
    let base = if a[0] == "run" {
        effective["name"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or(crate::config::infer_name(&cwd)?)
    } else {
        a[0].clone()
    };
    let prefix = crate::config::detect_worktree_prefix(
        &cwd,
        effective["worktree"]["prefix"].as_str().unwrap_or("branch"),
    );
    let name = normalize(&crate::config::apply_worktree_prefix(
        &base,
        prefix.as_deref(),
        effective["worktree"]["hostnameTemplate"].as_str(),
    ))?;
    let split = a.iter().position(|x| x == "--").unwrap_or(1);
    let mut command = a[split..].to_vec();
    if command.first().is_some_and(|x| x == "--") {
        command.remove(0);
    }
    if command.is_empty() {
        command = crate::config::resolve_script_command("dev", &cwd)
            .context("command required after -- or package dev script")?;
    }
    let requested = value(&a, "--app-port")
        .map(|p| p.parse::<u16>())
        .transpose()?
        .unwrap_or(0);
    let listener = std::net::TcpListener::bind(("127.0.0.1", requested))?;
    let port = listener.local_addr()?.port();
    let target = format!("http://127.0.0.1:{port}");
    let owner = std::process::id();
    let mut duplicate = false;
    state::mutate(|s| {
        if s.routes
            .iter()
            .any(|r| r.name == name && r.pid.is_some_and(state::alive))
        {
            duplicate = true;
        } else {
            s.routes.retain(|r| r.name != name);
            s.routes.push(Route {
                name: name.clone(),
                target,
                pid: Some(owner),
            });
        }
    })?;
    if duplicate {
        bail!("route already owned by a running process")
    }
    drop(listener);
    let mut c = tokio::process::Command::new(&command[0]);
    c.args(&command[1..])
        .env("PORT", port.to_string())
        .env("HOST", "127.0.0.1")
        .kill_on_drop(true);
    if let Some(framework) = value(&a, "--framework") {
        if ["vite", "next", "astro", "nuxt"].contains(&framework.as_str()) {
            c.arg("--port").arg(port.to_string());
        }
    }
    let result=async {let mut child=c.spawn()?;println!("http://{name}.localhost:1355 -> {port}");tokio::select!{s=child.wait()=>{let s=s?;if !s.success(){bail!("child exited with {s}")}},_=tokio::signal::ctrl_c()=>{child.kill().await?;}} Ok::<_,anyhow::Error>(())}.await;
    state::mutate(|s| {
        s.routes
            .retain(|r| !(r.name == name && r.pid == Some(owner)))
    })?;
    result
}

fn daemon_lock() -> Result<std::fs::File> {
    use fs2::FileExt;
    state::load()?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(state::dir().join("proxy.lock"))?;
    file.try_lock_exclusive()
        .context("another proxy owns this state directory")?;
    Ok(file)
}
