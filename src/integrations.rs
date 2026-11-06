//! Host integrations. Commands use argv directly; no command is evaluated by a shell.
use anyhow::{bail, Context, Result};
use std::{collections::BTreeSet, fs, net::UdpSocket, path::PathBuf, process::Command};

fn command(program: &str, args: &[String]) -> Result<String> {
    let output = Command::new(program).args(args).output().with_context(|| {
        format!("could not start {program}; install it and ensure it is on PATH")
    })?;
    if !output.status.success() {
        bail!(
            "{program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
fn exec(program: &str, args: &[&str]) -> Result<String> {
    command(
        program,
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
}
fn hostname(name: &str) -> Result<String> {
    let host = if name.contains('.') {
        name.to_owned()
    } else {
        format!("{name}.localhost")
    };
    if host.len() > 253
        || host.split('.').any(|s| {
            s.is_empty()
                || s.len() > 63
                || s.starts_with('-')
                || s.ends_with('-')
                || !s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        bail!("invalid hostname: {name}");
    }
    Ok(host.to_ascii_lowercase())
}
fn hosts_content(original: &str, names: &[String]) -> Result<String> {
    let mut result = String::new();
    let mut inside = false;
    for line in original.split_inclusive('\n') {
        match line.trim() {
            "# porch-start" => {
                if inside {
                    bail!("nested porch hosts block");
                }
                inside = true;
            }
            "# porch-end" => {
                if !inside {
                    bail!("unmatched porch hosts marker");
                }
                inside = false;
            }
            _ if !inside => result.push_str(line),
            _ => {}
        }
    }
    if inside {
        bail!("unterminated porch hosts block");
    }
    if !names.is_empty() {
        if !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("# porch-start\n");
        for name in names {
            result.push_str(&format!("127.0.0.1 {name}\n::1 {name}\n"));
        }
        result.push_str("# porch-end\n");
    }
    Ok(result)
}
fn hosts(action: &str) -> Result<()> {
    if !matches!(action, "sync" | "clean") {
        bail!("usage: hosts sync|clean");
    }
    let names = if action == "sync" {
        crate::state::load()?
            .routes
            .iter()
            .map(|r| hostname(&r.name))
            .collect::<Result<BTreeSet<_>>>()?
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    let path = if cfg!(windows) {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32/drivers/etc/hosts")
    } else {
        PathBuf::from("/etc/hosts")
    };
    let original = fs::read_to_string(&path)?;
    let updated = hosts_content(&original, &names)?;
    if original != updated {
        fs::write(&path, updated)
            .context("writing hosts file requires administrator privileges")?;
    }
    Ok(())
}
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn service(action: &str) -> Result<()> {
    if !matches!(action, "install" | "status" | "uninstall") {
        bail!("usage: service install|status|uninstall");
    }
    let exe = std::env::current_exe()?;
    let state = crate::state::dir();
    if cfg!(target_os = "macos") {
        let path = PathBuf::from("/Library/LaunchDaemons/dev.porch.proxy.plist");
        let p = path.to_str().context("non UTF-8 service path")?;
        match action {
            "status" => println!(
                "{}",
                exec("launchctl", &["print", "system/dev.porch.proxy"])?
            ),
            "install" => {
                let body=format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Label</key><string>dev.porch.proxy</string><key>ProgramArguments</key><array><string>{}</string><string>proxy</string><string>start</string><string>--foreground</string></array><key>EnvironmentVariables</key><dict><key>PORCH_STATE_DIR</key><string>{}</string></dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>",xml(&exe.to_string_lossy()),xml(&state.to_string_lossy()));
                fs::write(&path, body)
                    .context("service installation requires administrator privileges")?;
                exec("launchctl", &["bootstrap", "system", p])?;
            }
            _ => {
                exec("launchctl", &["bootout", "system", p])?;
                fs::remove_file(&path)?;
            }
        }
    } else if cfg!(target_os = "linux") {
        let path = "/etc/systemd/system/porch.service";
        match action {
            "status" => println!(
                "{}",
                exec("systemctl", &["status", "porch.service", "--no-pager"])?
            ),
            "install" => {
                let quote = |s: &str| -> Result<String> {
                    if s.contains(['\n', '\r', '\0']) {
                        bail!("invalid service path");
                    }
                    Ok(format!(
                        "\"{}\"",
                        s.replace('\\', "\\\\")
                            .replace('"', "\\\"")
                            .replace('%', "%%")
                            .replace('$', "$$")
                    ))
                };
                fs::write(path,format!("[Unit]\nDescription=Porch proxy\nAfter=network.target\n[Service]\nExecStart={} proxy start --foreground\nEnvironment={}\nRestart=on-failure\n[Install]\nWantedBy=multi-user.target\n",quote(&exe.to_string_lossy())?,quote(&format!("PORCH_STATE_DIR={}",state.display()))?))?;
                exec("systemctl", &["daemon-reload"])?;
                exec("systemctl", &["enable", "--now", "porch.service"])?;
            }
            _ => {
                exec("systemctl", &["disable", "--now", "porch.service"])?;
                fs::remove_file(path)?;
                exec("systemctl", &["daemon-reload"])?;
            }
        }
    } else {
        bail!("OS service integration supports macOS and Linux");
    }
    Ok(())
}

pub async fn run(args: Vec<String>) -> Result<()> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    match sub {
        "hosts" => hosts(rest.first().map(String::as_str).unwrap_or("sync")),
        "service" => service(rest.first().map(String::as_str).unwrap_or("status")),
        "trust" => {
            let ca = crate::state::dir().join("ca.pem");
            if !ca.is_file() {
                bail!(
                    "local CA missing: {}; start the TLS proxy first",
                    ca.display()
                );
            }
            let ca = ca.to_str().context("non UTF-8 CA path")?;
            if cfg!(target_os = "macos") {
                exec(
                    "security",
                    &[
                        "add-trusted-cert",
                        "-d",
                        "-r",
                        "trustRoot",
                        "-k",
                        "/Library/Keychains/System.keychain",
                        ca,
                    ],
                )?;
            } else if cfg!(target_os = "linux") {
                fs::copy(ca, "/usr/local/share/ca-certificates/porch.crt")?;
                exec("update-ca-certificates", &[])?;
            } else if cfg!(windows) {
                exec("certutil", &["-user", "-addstore", "Root", ca])?;
            } else {
                bail!("unsupported trust store");
            }
            Ok(())
        }
        "share" => {
            let provider = rest.first().map(String::as_str).unwrap_or("");
            let target = rest
                .get(1)
                .context("usage: share tailscale|serve|funnel|ngrok|cloudflare PORT")?;
            let port: u16 = target.parse().context("share target must be a TCP port")?;
            if port == 0 {
                bail!("port must be nonzero");
            }
            let url = format!("http://127.0.0.1:{port}");
            let (program, argv) = match provider {
                "tailscale" | "serve" => ("tailscale", vec!["serve", "--bg", &url]),
                "funnel" => ("tailscale", vec!["funnel", "--bg", &url]),
                "ngrok" => ("ngrok", vec!["http", &url]),
                "cloudflare" | "cloudflared" => ("cloudflared", vec!["tunnel", "--url", &url]),
                _ => bail!("unknown sharing provider"),
            };
            let status = Command::new(program)
                .args(argv)
                .status()
                .with_context(|| format!("starting {program}"))?;
            if !status.success() {
                bail!("{program} exited with {status}");
            }
            Ok(())
        }
        "lan" => {
            let socket = UdpSocket::bind("0.0.0.0:0")?;
            socket.connect("1.1.1.1:53")?;
            let ip = socket.local_addr()?.ip();
            println!("LAN address: {ip}");
            let daemon = mdns_sd::ServiceDaemon::new()?;
            let name = "porch.local.";
            let info = mdns_sd::ServiceInfo::new(
                "_http._tcp.local.",
                "Porch",
                name,
                ip.to_string(),
                1355,
                None,
            )?;
            daemon.register(info)?;
            for route in crate::state::load()?.routes {
                println!(
                    "{}: {} (configure client DNS/hosts to {ip}; proxy must bind 0.0.0.0)",
                    route.name, route.target
                );
            }
            println!("Announcing Porch over mDNS; press Ctrl-C to stop");
            tokio::signal::ctrl_c().await?;
            daemon.shutdown()?;
            Ok(())
        }
        "compose" => {
            let mut argv = vec!["compose".to_string()];
            argv.extend_from_slice(rest);
            if rest.is_empty() {
                bail!("usage: compose <docker compose arguments>");
            }
            println!("{}", command("docker", &argv)?);
            let output = exec("docker", &["compose", "ps", "--format", "json"])?;
            let containers: Vec<serde_json::Value> = if output.trim_start().starts_with('[') {
                serde_json::from_str(&output)?
            } else {
                output
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(serde_json::from_str)
                    .collect::<std::result::Result<_, _>>()?
            };
            let mut routes = Vec::new();
            for c in containers {
                let service = c["Service"].as_str().context("Compose service missing")?;
                if let Some(publishers) = c["Publishers"].as_array() {
                    for p in publishers {
                        if let Some(port) = p["PublishedPort"].as_u64().filter(|p| *p > 0) {
                            routes.push(crate::state::Route {
                                name: hostname(service)?,
                                target: format!("http://127.0.0.1:{port}"),
                                pid: None,
                            });
                            break;
                        }
                    }
                }
            }
            crate::state::mutate(|s| {
                for route in routes {
                    s.routes.retain(|r| r.name != route.name);
                    s.routes.push(route);
                }
            })?;
            Ok(())
        }
        "doctor" => {
            println!("state: {}", crate::state::dir().display());
            println!("routes: {}", crate::state::load()?.routes.len());
            for (tool, flag) in [
                ("openssl", "version"),
                ("docker", "--version"),
                ("tailscale", "version"),
                ("ngrok", "version"),
                ("cloudflared", "--version"),
            ] {
                match exec(tool, &[flag]) {
                    Ok(s) => println!("{tool}: {}", s.trim()),
                    Err(e) => println!("{tool}: {e}"),
                }
            }
            Ok(())
        }
        _ => bail!("expected hosts, service, trust, share, lan, compose, or doctor"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hosts_preserve_unmanaged() {
        let text = "127.0.0.1 localhost\n# porch-start\nold\n# porch-end\n# custom\n";
        assert_eq!(
            hosts_content(text, &[]).unwrap(),
            "127.0.0.1 localhost\n# custom\n"
        );
        assert!(hosts_content("# porch-start\n", &[]).is_err());
        assert!(hostname("evil\n127.0.0.1 injected").is_err());
    }
}
