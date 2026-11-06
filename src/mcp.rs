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
];
fn profiles(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in s.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let p = match p {
            "default" => "core",
            "full" => "all",
            p => p,
        };
        if !["core", "all", "proxy", "diagnostics"].contains(&p) {
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
                "core" => NAMES[..5].contains(&n),
                "proxy" => NAMES[5..7].contains(&n),
                "diagnostics" => NAMES[7..].contains(&n),
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
        "alias_add" => &["name", "port", "force", "tld"],
        "alias_remove" => &["name", "tld"],
        "proxy_start" => &["port", "noTls", "lan", "tld", "wildcard", "unprivileged"],
        "proxy_stop" => &["port"],
        "prune" => &["force"],
        _ => &[],
    };
    for f in fields {
        properties[*f] = match *f {
            "name" => json!({"type":"string","minLength":1}),
            "port" => json!({"type":"integer","minimum":1,"maximum":65535}),
            "tld" => json!({"type":"array","minItems":1,"items":{"type":"string"}}),
            _ => json!({"type":"boolean"}),
        };
    }
    let required: Vec<&str> = match n {
        "get" | "alias_remove" => vec!["name"],
        "alias_add" => vec!["name", "port"],
        _ => vec![],
    };
    let read = ["list", "get", "tools_profiles", "doctor", "version"].contains(&n);
    json!({"name":n,"description":format!("Porch {}", n.replace('_', " ")),"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":read,"destructiveHint":(["alias_add","alias_remove","proxy_stop","prune"].contains(&n)),"idempotentHint":n!="alias_add","openWorldHint":n=="proxy_start"}})
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
            json!({"activeProfiles":p,"availableProfiles":["core","proxy","diagnostics","all"]}),
            false,
        ));
    }
    // Route listing uses the same persistent state as the proxy.
    if n == "list" && a.as_object().unwrap().is_empty() {
        let state = crate::state::load()?;
        let settings = crate::state::proxy_settings()?;
        let routes: Vec<_> = state
            .routes
            .iter()
            .filter(|r| crate::state::route_alive(&state,r))
            .map(|r| json!({"name":r.name,"url":settings.url(&r.name),"target":r.target,"pid":r.pid,"urls":state.shares.get(&r.name)}))
            .collect();
        return Ok(tool_result(json!({"routes":routes}), false));
    }
    let mut argv: Vec<String> = match n {
        "list" => vec!["list".into(), "--json".into()],
        "alias_add" => vec![
            "alias".into(),
            a["name"].as_str().unwrap().into(),
            format!("http://127.0.0.1:{}", a["port"]),
        ],
        "alias_remove" => vec!["remove".into(), a["name"].as_str().unwrap().into()],
        "get" => vec!["get".into(), a["name"].as_str().unwrap().into()],
        "proxy_start" => vec!["proxy".into(), "start".into()],
        "proxy_stop" => vec!["proxy".into(), "stop".into()],
        "version" => vec!["--version".into()],
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
    ] {
        if a[key] == true {
            argv.push(flag.into());
        }
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
            5
        );
        s.handle(r#"{"jsonrpc":"2.0","id":4,"method":"shutdown"}"#)
            .await;
        assert!(s.shutdown);
    }
}
