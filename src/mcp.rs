//! Newline-delimited stdio MCP transport. Stdout is reserved for JSON-RPC.
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const NAMES: &[&str] = &[
    "list",
    "get",
    "alias_add",
    "alias_remove",
    "tools_profiles",
    "proxy_start",
    "proxy_stop",
    "doctor",
    "prune",
    "version",
    "doctor_fix",
    "machines",
    "run",
    "stop",
    "logs",
];
/// What each tool does, for agents choosing between them.
fn description(n: &str) -> &'static str {
    match n {
        "list" => "List the apps Porch is serving on this computer: name, URL, upstream target, owning pid, health (ok, not listening, stopped, dead) and any sharing URLs. Use this to find the URL of a running dev server instead of guessing a localhost port.",
        "get" => "Print the stable URL for an app name, applying the current Git worktree prefix unless noWorktree is set. Works before the app runs, so it is safe for wiring one app's URL into another's config.",
        "alias_add" => "Route NAME.localhost to an existing local port that Porch did not start (a database UI, a Docker container). Fails if the name is taken unless force is set. private keeps it off paired machines; upstream (https, https://HOST or http://HOST) sets TLS and the Host sent to the port; insecure skips certificate checks for an https upstream (self-signed dev servers; trusting its CA is better).",
        "alias_remove" => "Remove a static alias created with alias_add. Never stops a process.",
        "tools_profiles" => "Report which tool profiles are active and which exist (core, proxy, diagnostics, all).",
        "proxy_start" => "Start the Porch proxy in the background if it is not running. Apps start it automatically, so this is rarely needed. https serves HTTPS with a local CA trusted for the user.",
        "proxy_stop" => "Stop the Porch proxy. Running apps keep their routes but their URLs stop answering until the proxy starts again.",
        "doctor" => "Read-only health check: proxy listeners, routes, name resolution, HTTPS trust and optional tools. Returns a non-zero exit code when it finds problems. Never changes anything; use doctor_fix to apply fixes.",
        "prune" => "Remove routes whose apps have exited. With force, also stop processes those routes left behind.",
        "version" => "Print the Porch version.",
        "run" => "Start a dev server in the background behind a stable URL (porch run --detach). Give command as an argument list, e.g. [\"pnpm\",\"dev\"], and cwd as the project directory; name is optional. Returns once the app answers, with its URL. Use logs to read its output and stop to end it.",
        "stop" => "Stop an app Porch started (its launcher and whole process tree). Aliases are not affected.",
        "logs" => "Read the output of an app started with run (porch run --detach). lines limits it to the last N lines.",
        "machines" => "List this computer's apps and every paired machine's apps, with the URL that opens each remote app through the local proxy (APP.MACHINE.localhost). Use this to reach a dev server running on another computer.",
        "doctor_fix" => "Apply doctor's safe fixes: move a stopped proxy off a busy port, prune dead routes, restart an outdated proxy. With yes, also terminate stopped or orphaned launches without asking.",
        _ => "",
    }
}
/// Extra CLI arguments each tool may pass through. Read-only tools accept only
/// read-only flags, so `extraArgs` can never turn an inspection into a repair.
fn allowed_extra(n: &str) -> Option<&'static [&'static str]> {
    match n {
        "list" => Some(&["--all", "--json"]),
        "get" => Some(&["--no-worktree"]),
        "doctor" | "version" | "tools_profiles" | "machines" | "logs" => Some(&[]),
        "doctor_fix" => Some(&[]),
        _ => None,
    }
}
/// Tools that never change routes, the proxy or processes.
const READ_ONLY: &[&str] = &[
    "list",
    "get",
    "tools_profiles",
    "doctor",
    "version",
    "machines",
    "logs",
];
fn profiles(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in s.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let p = match p {
            "default" => "core",
            "full" => "all",
            p => p,
        };
        if !["core", "all", "proxy", "diagnostics", "readonly", "apps"].contains(&p) {
            bail!("Unknown tools profile: {p}");
        }
        if !out.iter().any(|x| x == p) {
            out.push(p.to_owned());
        }
    }
    if out.is_empty() {
        bail!("No tools profiles given");
    }
    Ok(out)
}
fn enabled(p: &[String], n: &str) -> bool {
    p.iter().any(|p| {
        p == "all"
            || match p.as_str() {
                "core" => NAMES[..5].contains(&n) || n == "machines" || n == "logs",
                // Start and stop dev servers in the background.
                "apps" => ["run", "stop", "logs"].contains(&n),
                "proxy" => NAMES[5..7].contains(&n),
                "diagnostics" => ["doctor", "prune", "version", "doctor_fix"].contains(&n),
                // Inspection only: nothing that changes routes, the proxy or processes.
                "readonly" => READ_ONLY.contains(&n),
                _ => false,
            }
    })
}
fn definition(n: &str) -> Value {
    let mut properties = json!({});
    if n != "tools_profiles" {
        properties = json!({"cwd":{"type":"string","minLength":1},"extraArgs":{"type":"array","items":{"type":"string"}},"timeoutMs":{"type":"integer","minimum":1,"maximum":2147483647,"default":120000}});
    }
    let fields: &[&str] = match n {
        "get" => &["name", "noWorktree"],
        "alias_add" => &[
            "name", "port", "force", "tld", "private", "upstream", "insecure",
        ],
        "alias_remove" => &["name", "tld"],
        "proxy_start" => &[
            "port",
            "https",
            "noTls",
            "lan",
            "tld",
            "wildcard",
            "unprivileged",
        ],
        "proxy_stop" => &["port"],
        "prune" => &["force"],
        "doctor_fix" => &["yes"],
        "run" => &["command", "name"],
        "stop" => &["name"],
        "logs" => &["name", "lines"],
        _ => &[],
    };
    for f in fields {
        properties[*f] = match *f {
            "name" | "upstream" => json!({"type":"string","minLength":1}),
            "port" => json!({"type":"integer","minimum":1,"maximum":65535}),
            "lines" => json!({"type":"integer","minimum":1,"maximum":100000}),
            "command" => json!({"type":"array","minItems":1,"items":{"type":"string"}}),
            "tld" => json!({"type":"array","minItems":1,"items":{"type":"string"}}),
            _ => json!({"type":"boolean"}),
        };
    }
    let required: Vec<&str> = match n {
        "get" | "alias_remove" | "stop" | "logs" => vec!["name"],
        "run" => vec!["command"],
        "alias_add" => vec!["name", "port"],
        _ => vec![],
    };
    let read = READ_ONLY.contains(&n);
    json!({"name":n,"description":description(n),"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":read,"destructiveHint":(["alias_add","alias_remove","proxy_stop","prune","doctor_fix","stop"].contains(&n)),"idempotentHint":n!="alias_add","openWorldHint":n=="proxy_start"}})
}
fn validate(n: &str, a: &Value) -> Result<()> {
    let d = definition(n);
    let obj = a
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("arguments must be an object"))?;
    for r in d["inputSchema"]["required"].as_array().unwrap() {
        if !obj.contains_key(r.as_str().unwrap()) {
            bail!("{} is required", r);
        }
    }
    for (k, v) in obj {
        let schema = &d["inputSchema"]["properties"][k];
        if schema.is_null() {
            bail!("Unknown argument: {k}");
        }
        let valid = match schema["type"].as_str().unwrap() {
            "string" => v
                .as_str()
                .is_some_and(|s| !s.is_empty() && (k != "name" || !s.starts_with('-'))),
            "boolean" => v.is_boolean(),
            "integer" => v
                .as_u64()
                .is_some_and(|x| x >= 1 && x <= schema["maximum"].as_u64().unwrap()),
            "array" => v
                .as_array()
                .is_some_and(|a| (k != "tld" || !a.is_empty()) && a.iter().all(Value::is_string)),
            _ => false,
        };
        if !valid {
            bail!("Invalid argument: {k}");
        }
    }
    Ok(())
}
fn tool_result(v: Value, error: bool) -> Value {
    json!({"content":[{"type":"text","text":if let Some(s)=v.as_str(){s.to_owned()}else{v.to_string()}}],"structuredContent":{"result":v},"isError":error})
}
async fn call(n: &str, a: &Value, p: &[String]) -> Result<Value> {
    validate(n, a)?;
    if n == "tools_profiles" {
        return Ok(tool_result(
            json!({"activeProfiles":p,"availableProfiles":["core","proxy","diagnostics","readonly","all"]}),
            false,
        ));
    }
    if let (Some(allowed), Some(extra)) = (allowed_extra(n), a["extraArgs"].as_array()) {
        for arg in extra {
            let s = arg.as_str().unwrap_or_default();
            let flag = s.split('=').next().unwrap_or(s);
            if !allowed.contains(&flag) {
                bail!("{n} does not accept the argument {s:?}");
            }
        }
    }
    let mut argv: Vec<String> = match n {
        "list" => vec!["list".into(), "--json".into()],
        "alias_add" => vec![
            "alias".into(),
            a["name"].as_str().unwrap().into(),
            a["port"].to_string(),
        ],
        "alias_remove" => vec!["remove".into(), a["name"].as_str().unwrap().into()],
        "get" => vec!["get".into(), a["name"].as_str().unwrap().into()],
        "proxy_start" => vec!["proxy".into(), "start".into()],
        "proxy_stop" => vec!["proxy".into(), "stop".into()],
        "version" => vec!["--version".into()],
        "doctor_fix" => vec!["doctor".into(), "--fix".into()],
        "machines" => vec!["list".into(), "--all".into(), "--json".into()],
        "run" => {
            let mut v = vec!["run".into(), "--detach".into()];
            if let Some(name) = a["name"].as_str() {
                v.extend(["--name".into(), name.into()]);
            }
            v.push("--".into());
            v.extend(
                a["command"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c.as_str().unwrap().to_owned()),
            );
            v
        }
        "stop" => vec!["stop".into(), a["name"].as_str().unwrap().into()],
        "logs" => {
            let mut v = vec!["logs".into(), a["name"].as_str().unwrap().into()];
            if let Some(n) = a["lines"].as_u64() {
                v.extend(["--lines".into(), n.to_string()]);
            }
            v
        }
        _ => vec![n.into()],
    };
    if n.starts_with("proxy_") {
        if let Some(port) = a["port"].as_u64() {
            argv.extend(["--port".into(), port.to_string()]);
        }
    }
    for (key, flag) in [
        ("noWorktree", "--no-worktree"),
        ("force", "--force"),
        ("noTls", "--no-tls"),
        ("lan", "--lan"),
        ("wildcard", "--wildcard"),
        ("unprivileged", "--unprivileged"),
        ("yes", "--yes"),
        ("private", "--private"),
        ("insecure", "--insecure"),
        ("https", "--https"),
    ] {
        if a[key] == true {
            argv.push(flag.into());
        }
    }
    if let Some(up) = a["upstream"].as_str() {
        argv.extend(["--upstream".into(), up.into()]);
    }
    if let Some(tlds) = a["tld"].as_array() {
        for tld in tlds {
            argv.extend(["--tld".into(), tld.as_str().unwrap().into()]);
        }
    }
    if let Some(extra) = a["extraArgs"].as_array() {
        for arg in extra {
            let s = arg.as_str().unwrap();
            if s == "--foreground" || s.starts_with("--foreground=") {
                bail!("Foreground commands are unavailable over MCP");
            }
            argv.push(s.into());
        }
    }
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .env("CI", "1")
        .env_remove("FORCE_COLOR")
        .kill_on_drop(true);
    if let Some(cwd) = a["cwd"].as_str() {
        command.current_dir(cwd);
    }
    let ms = a["timeoutMs"].as_u64().unwrap_or(120000);
    let output = tokio::time::timeout(Duration::from_millis(ms), command.output()).await;
    match output {
        Ok(Ok(o)) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            Ok(
                json!({"content":[{"type":"text","text":format!("{}\n{}",stdout,stderr)}],"structuredContent":{"exitCode":o.status.code(),"stdout":stdout,"stderr":stderr},"isError":!o.status.success()}),
            )
        }
        Ok(Err(e)) => bail!("Failed to run porch: {e}"),
        Err(_) => bail!("porch command timed out after {ms}ms"),
    }
}
struct Server {
    profiles: Vec<String>,
    initialized: bool,
    shutdown: bool,
}
impl Server {
    async fn handle(&mut self, line: &str) -> Option<Value> {
        if line.trim().is_empty() {
            return None;
        }
        let m: Value = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => {
                return Some(
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
                )
            }
        };
        let id = m.get("id").cloned().unwrap_or(Value::Null);
        let err =
            |code, msg: &str| json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":msg}});
        if !m.is_object()
            || m["jsonrpc"] != "2.0"
            || !m["method"].is_string()
            || !(id.is_null() || id.is_string() || id.is_number())
        {
            return Some(err(-32600, "Invalid request"));
        }
        m.get("id")?;
        let params = m.get("params").cloned().unwrap_or(json!({}));
        if !params.is_object() {
            return Some(err(-32602, "params must be an object"));
        }
        let result = match m["method"].as_str().unwrap() {
            "initialize" => {
                if self.initialized {
                    return Some(err(-32600, "Already initialized"));
                }
                if params
                    .get("protocolVersion")
                    .is_some_and(|v| !v.is_string())
                {
                    return Some(err(-32602, "Invalid protocolVersion"));
                }
                self.initialized = true;
                let requested = params["protocolVersion"].as_str().unwrap_or("");
                let version = if ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"]
                    .contains(&requested)
                {
                    requested
                } else {
                    "2025-11-25"
                };
                json!({"protocolVersion":version,"capabilities":{"tools":{}},"serverInfo":{"name":"porch","version":env!("CARGO_PKG_VERSION")}})
            }
            "ping" => json!({}),
            "shutdown" => {
                self.shutdown = true;
                json!({})
            }
            "tools/list" | "tools/call" if !self.initialized => {
                return Some(err(-32002, "Server not initialized"))
            }
            "tools/list" => {
                let defs: Vec<_> = NAMES
                    .iter()
                    .filter(|n| enabled(&self.profiles, n))
                    .map(|n| definition(n))
                    .collect();
                let start = match params.get("cursor") {
                    None => 0,
                    Some(Value::String(s))
                        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) =>
                    {
                        s.parse::<usize>().unwrap_or(usize::MAX)
                    }
                    _ => usize::MAX,
                };
                if start > defs.len() {
                    return Some(err(-32602, "Invalid cursor"));
                }
                json!({"tools":defs[start..]})
            }
            "tools/call" => {
                let Some(n) = params["name"].as_str() else {
                    return Some(err(-32602, "name is required"));
                };
                if !NAMES.contains(&n) {
                    return Some(err(-32602, "Unknown tool"));
                }
                if !enabled(&self.profiles, n) {
                    tool_result(
                        json!("Tool is disabled; restart porch mcp --tools all"),
                        true,
                    )
                } else {
                    match call(
                        n,
                        params.get("arguments").unwrap_or(&json!({})),
                        &self.profiles,
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(e) => tool_result(json!(e.to_string()), true),
                    }
                }
            }
            _ => return Some(err(-32601, "Method not found")),
        };
        Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
    }
}
/// Serve MCP until shutdown or EOF; never emit human diagnostics on stdout.
pub async fn serve(profile: String) -> Result<()> {
    let mut server = Server {
        profiles: profiles(&profile)?,
        initialized: false,
        shutdown: false,
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if let Some(response) = server.handle(&line).await {
            out.write_all(format!("{response}\n").as_bytes()).await?;
            out.flush().await?;
        }
        if server.shutdown {
            break;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_selection() {
        assert_eq!(
            profiles("default,proxy,default").unwrap(),
            vec!["core", "proxy"]
        );
        assert!(profiles("bad").is_err());
        assert!(!enabled(&profiles("core").unwrap(), "proxy_start"));
        let ro = profiles("readonly").unwrap();
        for n in NAMES {
            let read = definition(n)["annotations"]["readOnlyHint"] == true;
            assert_eq!(enabled(&ro, n), read, "{n}");
        }
    }
    #[test]
    fn arguments() {
        assert!(validate("alias_add", &json!({"name":"web","port":3000})).is_ok());
        for a in [
            json!({"name":"-x","port":3}),
            json!({"name":"x","port":0}),
            json!({"name":"x","port":3,"unknown":true}),
        ] {
            assert!(validate("alias_add", &a).is_err());
        }
    }
    #[tokio::test]
    async fn read_only_tools_reject_repair_arguments() {
        for (tool, extra) in [
            ("doctor", "--fix"),
            ("doctor", "--yes"),
            ("list", "--force"),
            ("get", "--force"),
        ] {
            let err = call(
                tool,
                &json!({"extraArgs":[extra]})
                    .as_object()
                    .map(|o| {
                        let mut o = o.clone();
                        if tool == "get" {
                            o.insert("name".into(), json!("web"));
                        }
                        Value::Object(o)
                    })
                    .unwrap(),
                &["all".into()],
            )
            .await
            .unwrap_err();
            assert!(
                err.to_string().contains("does not accept"),
                "{tool} {extra}: {err}"
            );
        }
        let doctor = definition("doctor");
        assert_eq!(doctor["annotations"]["readOnlyHint"], true);
        assert_eq!(
            definition("doctor_fix")["annotations"]["destructiveHint"],
            true
        );
    }
    /// Every flag of the commands MCP wraps is either a typed tool argument
    /// or deliberately left out, so the two can't drift apart.
    #[test]
    fn tool_arguments_cover_the_cli_flags() {
        let cli = crate::cli::command();
        let camel = |flag: &str| {
            let mut out = String::new();
            let mut upper = false;
            for c in flag.chars() {
                if c == '-' {
                    upper = true;
                } else if upper {
                    out.extend(c.to_uppercase());
                    upper = false;
                } else {
                    out.push(c);
                }
            }
            out
        };
        for (path, tool, excluded) in [
            (vec!["alias"], "alias_add", vec!["remove", "help"]),
            (
                vec!["proxy", "start"],
                "proxy_start",
                // Foreground can't work over MCP; the rest need files or are
                // saved settings an agent shouldn't change behind the user.
                vec![
                    "foreground",
                    "skip-trust",
                    "ip",
                    "cert",
                    "key",
                    "bind",
                    "redirect-port",
                    "state-dir",
                    "help",
                ],
            ),
            (vec!["get"], "get", vec!["help"]),
            (vec!["stop"], "stop", vec!["help"]),
            (vec!["logs"], "logs", vec!["follow", "help"]),
        ] {
            let mut command = &cli;
            for part in &path {
                command = command.find_subcommand(part).unwrap();
            }
            let properties = definition(tool)["inputSchema"]["properties"].clone();
            for arg in command.get_arguments() {
                let Some(long) = arg.get_long() else { continue };
                if excluded.contains(&long) {
                    continue;
                }
                assert!(
                    !properties[camel(long)].is_null(),
                    "MCP tool {tool} has no argument for `porch {} --{long}`",
                    path.join(" ")
                );
            }
        }
    }
    #[tokio::test]
    async fn lifecycle() {
        let mut s = Server {
            profiles: profiles("core").unwrap(),
            initialized: false,
            shutdown: false,
        };
        assert_eq!(s.handle("{").await.unwrap()["error"]["code"], -32700);
        assert!(s
            .handle(r#"{"jsonrpc":"2.0","method":"ping"}"#)
            .await
            .is_none());
        assert_eq!(
            s.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
                .await
                .unwrap()["error"]["code"],
            -32002
        );
        s.handle(r#"{"jsonrpc":"2.0","id":2,"method":"initialize"}"#)
            .await;
        assert_eq!(
            s.handle(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#)
                .await
                .unwrap()["result"]["tools"]
                .as_array()
                .unwrap()
                .len(),
            7
        );
        let tools = s
            .handle(r#"{"jsonrpc":"2.0","id":5,"method":"tools/list"}"#)
            .await
            .unwrap();
        for tool in tools["result"]["tools"].as_array().unwrap() {
            assert!(tool["description"].as_str().unwrap().len() > 40, "{tool}");
        }
        s.handle(r#"{"jsonrpc":"2.0","id":4,"method":"shutdown"}"#)
            .await;
        assert!(s.shutdown);
    }
}
