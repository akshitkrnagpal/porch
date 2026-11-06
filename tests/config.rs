use porch::config::*;
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "porch-config-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn write(&self, path: &str, v: serde_json::Value) {
        let p = self.0.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, serde_json::to_vec(&v).unwrap()).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}
#[test]
fn native_configuration_wins_and_legacy_remains_a_fallback() {
    let f = Fixture::new();
    f.write("package.json", json!({"portless":"legacy-package"}));
    assert_eq!(load(&f.0).unwrap()["name"], "legacy-package");
    f.write("portless.json", json!({"name":"legacy-file"}));
    assert_eq!(load(&f.0).unwrap()["name"], "legacy-file");
    f.write(
        "package.json",
        json!({"porch":"native-package", "portless":"legacy-package"}),
    );
    assert_eq!(effective_config(&f.0).unwrap()["name"], "native-package");
    f.write(
        "porch.json",
        json!({"name":"native-file", "script":"start"}),
    );
    assert_eq!(load(&f.0).unwrap()["name"], "native-file");
    f.write("package.json", json!({"portless":"legacy-package"}));
    assert_eq!(effective_config(&f.0).unwrap()["name"], "native-file");
    fs::write(f.0.join("porch.json"), "{").unwrap();
    assert!(load(&f.0).unwrap_err().to_string().contains("porch.json"));
}

#[test]
fn bare_launch_runs_selected_package_script_and_config_precedence() {
    let f = Fixture::new();
    f.write("package.json",json!({"name":"@scope/site","scripts":{"dev":"vite","start":"node index.js"},"porch":{"name":"leaf","script":"start","appPort":4901}}));
    let p = launch_plans(&f.0, None, None, None, Some("")).unwrap();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].name, "leaf");
    assert_eq!(p[0].command, args(&["npm", "run", "start"]));
    assert_eq!(p[0].app_port, Some(4901));
    let p = launch_plans(
        &f.0,
        Some(args(&["node", "x.js"])),
        Some("explicit"),
        Some("dev"),
        Some("pr-42"),
    )
    .unwrap();
    assert_eq!(p[0].name, "pr-42.explicit");
    assert_eq!(p[0].command, args(&["node", "x.js"]));
}
#[test]
fn workspace_launches_servers_and_build_tasks_with_leaf_overrides() {
    let f = Fixture::new();
    f.write(
        "package.json",
        json!({"workspaces":["apps/*","tools/*","!apps/excluded"],"packageManager":"pnpm@10.0.0"}),
    );
    f.write("porch.json",json!({"apps":{"apps/api":{"name":"backend","appPort":4900}},"worktree":{"hostnameTemplate":"{worktree}-{name}"}}));
    f.write(
        "apps/web/package.json",
        json!({"name":"@org/web","scripts":{"dev":"vite"}}),
    );
    f.write("apps/api/package.json",json!({"name":"@org/api","scripts":{"serve":"node server.js"},"porch":{"name":"leaf-api","script":"serve","appPort":4902}}));
    f.write(
        "apps/excluded/package.json",
        json!({"name":"@org/excluded","scripts":{"dev":"node x"}}),
    );
    f.write(
        "tools/types/package.json",
        json!({"name":"@org/types","scripts":{"dev":"tsc --watch"}}),
    );
    let plans = launch_plans(&f.0, None, None, None, Some("")).unwrap();
    assert_eq!(plans.len(), 3);
    let api = plans.iter().find(|p| p.name == "leaf-api").unwrap();
    assert_eq!(api.app_port, Some(4902));
    assert_eq!(api.command, args(&["pnpm", "run", "serve"]));
    assert!(plans.iter().find(|p| p.name == "web.org").unwrap().proxy);
    assert!(!plans.iter().find(|p| p.name == "types.org").unwrap().proxy);
    let one = launch_plans(&f.0.join("apps/web"), None, None, None, Some("")).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].name, "web");
}
#[test]
fn workspace_collisions_fail_before_launch() {
    let f = Fixture::new();
    f.write("package.json", json!({"workspaces":["apps/*"]}));
    for p in ["a", "b"] {
        f.write(
            &format!("apps/{p}/package.json"),
            json!({"name":p,"scripts":{"dev":"node x"},"portless":"duplicate"}),
        );
    }
    assert!(launch_plans(&f.0, None, None, None, Some(""))
        .unwrap_err()
        .to_string()
        .contains("duplicate"));
}
#[test]
fn all_supported_frameworks_receive_only_server_flags() {
    for (a, strict, host) in [
        (vec!["vite"], true, "--host"),
        (vec!["vite", "preview"], true, "--host"),
        (vec!["vitepress", "dev"], true, "--host"),
        (vec!["vp", "dev"], true, "--host"),
        (vec!["react-router", "dev"], true, "--host"),
        (vec!["rsbuild", "dev"], false, "--host"),
        (vec!["astro", "dev"], false, "--host"),
        (vec!["ng", "serve"], false, "--host"),
        (vec!["react-native", "start"], false, "--host"),
        (vec!["expo", "start"], false, "--host"),
        (vec!["wrangler", "dev"], false, "--ip"),
    ] {
        let (out, bind) = adapt_command(&args(&a), Path::new("/tmp"), 4567, false).unwrap();
        assert!(
            out.windows(2).any(|w| w == args(&["--port", "4567"])),
            "{a:?}: {out:?}"
        );
        assert_eq!(out.iter().any(|s| s == "--strictPort"), strict);
        assert!(out.iter().any(|s| s == host));
        assert_eq!(bind.as_deref(), Some("127.0.0.1"));
    }
    for a in [
        vec!["vite", "build"],
        vec!["vite", "--mode", "dev", "build"],
        vec!["astro", "build"],
        vec!["vp", "build"],
        vec!["ng", "build"],
        vec!["react-router", "typegen"],
        vec!["react-native", "run-ios"],
        vec!["expo", "export"],
        vec!["wrangler", "deploy"],
        vec!["next", "dev"],
    ] {
        let a = args(&a);
        assert_eq!(
            adapt_command(&a, Path::new("/tmp"), 4567, false).unwrap().0,
            a
        );
    }
}
#[test]
fn package_runner_flags_and_child_separator_are_preserved() {
    for a in [
        vec!["npx", "-y", "vite", "dev"],
        vec!["npx", "--package", "vite", "vite", "dev"],
        vec!["pnpm", "exec", "vite", "dev"],
        vec!["yarn", "dlx", "vite", "dev"],
        vec!["bunx", "--bun", "vite", "dev"],
        vec!["npm", "exec", "--", "vite", "dev"],
    ] {
        let (out, _) = adapt_command(&args(&a), Path::new("/tmp"), 4567, false).unwrap();
        assert!(out.iter().any(|s| s == "4567"), "{a:?} -> {out:?}");
    }
    let (out, _) = adapt_command(
        &args(&["vite", "dev", "--", "child", "--keep"]),
        Path::new("/tmp"),
        4567,
        false,
    )
    .unwrap();
    let i = out.iter().position(|s| s == "--").unwrap();
    assert_eq!(&out[i..], args(&["--", "child", "--keep"]));
    assert!(out[..i].iter().any(|s| s == "4567"));
    let (out, _) = adapt_command(
        &args(&["vite", "--port=1234", "--host", "0.0.0.0"]),
        Path::new("/tmp"),
        4567,
        false,
    )
    .unwrap();
    assert!(!out.iter().any(|s| s == "4567"));
    assert_eq!(out, args(&["vite", "--port=1234", "--host", "0.0.0.0"]));
}
#[test]
fn script_framework_flags_use_npm_separator_and_refuse_shell_compounds() {
    let f = Fixture::new();
    for (raw, safe) in [
        ("vite dev", true),
        ("vite dev --open '/foo&bar'", true),
        ("vite dev 2>&1", true),
        ("vite dev&&node x", false),
        ("vite dev\nnode x", false),
        ("vite dev # comment", false),
        ("vite dev &> out.log", false),
        ("vite dev -- positional", false),
    ] {
        f.write("package.json", json!({"scripts":{"dev":raw}}));
        let (out, _) = adapt_command(&args(&["npm", "run", "dev"]), &f.0, 4567, false).unwrap();
        assert_eq!(out.iter().any(|s| s == "4567"), safe, "{raw}: {out:?}");
        if safe {
            assert_eq!(out[3], "--");
        }
    }
    f.write(
        "package.json",
        json!({"scripts":{"dev":"vite dev --port=1234"}}),
    );
    let (out, _) = adapt_command(
        &args(&["bun", "run", "dev", "--host=0.0.0.0"]),
        &f.0,
        4567,
        false,
    )
    .unwrap();
    assert!(!out.iter().any(|s| s == "4567"));
    assert_eq!(out, args(&["bun", "run", "dev", "--host=0.0.0.0"]));
}
#[test]
fn expo_lan_skips_host_even_when_script_is_unsafe_to_append() {
    let f = Fixture::new();
    f.write(
        "package.json",
        json!({"scripts":{"dev":"expo start && echo done"}}),
    );
    for a in [args(&["expo", "start"]), args(&["npm", "run", "dev"])] {
        let (out, host) = adapt_command(&a, &f.0, 4567, true).unwrap();
        assert!(host.is_none());
        assert!(!out.iter().any(|s| s == "--host"));
    }
}
#[test]
fn turbo_keeps_root_graph_and_respects_opt_out() {
    let f = Fixture::new();
    f.write(
        "package.json",
        json!({"workspaces":["apps/*"],"scripts":{"dev":"turbo dev"},"packageManager":"pnpm@10"}),
    );
    f.write("turbo.json", json!({"tasks":{"dev":{"persistent":true}}}));
    assert_eq!(
        turbo_command(&f.0, None).unwrap().unwrap(),
        args(&["pnpm", "run", "dev"])
    );
    assert_eq!(
        turbo_command(&f.0, Some("serve")).unwrap().unwrap(),
        args(&["pnpm", "exec", "turbo", "run", "serve"])
    );
    f.write("portless.json", json!({"turbo":false}));
    assert!(turbo_command(&f.0, None).unwrap().is_none());
}
#[test]
fn worktree_prefix_uses_last_segment_and_template_with_real_git() {
    let f = Fixture::new();
    let root = f.0.join("repo");
    fs::create_dir_all(&root).unwrap();
    let git = |cwd: &Path, a: &[&str]| {
        let o = std::process::Command::new("git")
            .args(a)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    git(&root, &["init", "-b", "main"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    let linked = f.0.join("linked");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "team/new-ui",
            linked.to_str().unwrap(),
        ],
    );
    assert_eq!(
        detect_worktree_prefix(&linked, "last-segment").as_deref(),
        Some("new-ui")
    );
    assert_eq!(
        detect_worktree_prefix(&linked, "branch").as_deref(),
        Some("team-new-ui")
    );
    assert!(detect_worktree_prefix(&root, "branch").is_none());
    fs::write(
        linked.join("portless.json"),
        r#"{"name":"web","worktree":{"hostnameTemplate":"{worktree}-{name}"}}"#,
    )
    .unwrap();
    let p = launch_plans(&linked, Some(args(&["true"])), None, None, None).unwrap();
    assert_eq!(p[0].name, "new-ui-web");
}

#[cfg(unix)]
#[test]
fn framework_script_forwarding_reaches_real_package_manager_children() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("node_modules/.bin")).unwrap();
    let shim = f.0.join("node_modules/.bin/vite");
    fs::write(
        &shim,
        "#!/bin/sh\nexec node \"$PORCH_CAPTURE_SCRIPT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let capture = f.0.join("capture.cjs");
    fs::write(&capture,"require('fs').writeFileSync(process.env.PORCH_CAPTURE_FILE, JSON.stringify(process.argv.slice(2)))").unwrap();
    let record = f.0.join("record.json");
    for raw in [
        "vite dev",
        "vite dev --open '/foo&bar'",
        "vite dev --open \"/foo;bar\"",
        "vite dev --open /foo\\ bar",
        "vite dev --tag v1#2",
        "vite dev --open /foo\\ #bar",
        "vite dev --tag \\#literal",
        "vite dev \"x\"#y",
        "vite dev 2>&1",
        "vite dev > out.log",
        "vite dev >> out.log",
        "vite dev < /dev/null",
        "vite dev --define SHA=$(printf abc)",
        "vite dev \\\n --open",
        "vite dev # comment",
        "vite dev&&echo done",
        "vite dev;echo done",
        "vite dev\necho done",
        "vite dev -- --extra",
        "NODE_ENV=production vite dev",
        "vite build",
        "vite --mode dev build",
    ] {
        f.write("package.json", json!({"scripts":{"dev":raw}}));
        let command = adapt_command(&args(&["npm", "run", "dev"]), &f.0, 4567, false)
            .unwrap()
            .0;
        let inserted = command.iter().any(|s| s == "4567");
        let _ = fs::remove_file(&record);
        let output = std::process::Command::new(&command[0])
            .args(&command[1..])
            .current_dir(&f.0)
            .env("PATH", augmented_path(&f.0))
            .env("PORCH_CAPTURE_SCRIPT", &capture)
            .env("PORCH_CAPTURE_FILE", &record)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{raw}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let received: Vec<String> = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        if inserted {
            assert!(
                received.windows(2).any(|w| w == args(&["--port", "4567"])),
                "lost flags: {raw}: {received:?}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn cli_turbo_workspace_preserves_graph_and_supplies_distinct_framework_environments() {
    turbo_workspace_flow(false);
}
#[cfg(unix)]
#[test]
fn cli_turbo_workspace_handles_package_porch_wrappers_without_recursion() {
    turbo_workspace_flow(true);
}
#[cfg(unix)]
fn turbo_workspace_flow(wrapper: bool) {
    let Some(binary) = option_env!("CARGO_BIN_EXE_porch") else {
        return;
    };
    struct ProxyCleanup {
        binary: String,
        state: PathBuf,
    }
    impl Drop for ProxyCleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new(&self.binary)
                .args(["proxy", "stop"])
                .env("PORCH_STATE_DIR", &self.state)
                .env("PORCH_SYNC_HOSTS", "0")
                .output();
        }
    }
    let f = Fixture::new();
    let state = f.0.join("state");
    let _cleanup = ProxyCleanup {
        binary: binary.into(),
        state: state.clone(),
    };
    f.write(
        "package.json",
        json!({"workspaces":["apps/*"],"scripts":{"dev":"node graph.cjs"}}),
    );
    f.write("turbo.json", json!({"tasks":{"dev":{"persistent":true}}}));
    fs::write(f.0.join("graph.cjs"),r#"require('fs').writeFileSync('root-argv.json',JSON.stringify(process.argv.slice(2)));const cp=require('child_process');for(const app of ['web','api']){const r=cp.spawnSync('npm',['run','dev'],{cwd:require('path').join(__dirname,'apps',app),stdio:'inherit',env:process.env});if(r.status)process.exit(r.status)}"#).unwrap();
    for app in ["web", "api"] {
        f.write(&format!("apps/{app}/package.json"),json!({"name":format!("@org/{app}"),"scripts":{"dev":"node node_modules/vite/bin/vite.js dev"}}));
        let p = f.0.join(format!("apps/{app}/node_modules/vite/bin"));
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("vite.js"),"require('fs').writeFileSync('captured.json',JSON.stringify({port:process.env.PORT,url:process.env.PORTLESS_URL,args:process.argv.slice(2)}))").unwrap();
        // Use the actual Vite CLI shape so its per-process preload recognizes it.
        f.write(
            &format!("apps/{app}/package.json"),
            json!({"name":format!("@org/{app}"),"scripts":{"dev":"vite dev"}}),
        );
        if wrapper {
            f.write(&format!("apps/{app}/package.json"),json!({"name":format!("@org/{app}"),"scripts":{"dev":format!("\"{binary}\""),"dev:app":"vite dev"},"portless":{"script":"dev:app"}}));
        }
        let bin = f.0.join(format!("apps/{app}/node_modules/.bin"));
        fs::create_dir_all(&bin).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../vite/bin/vite.js", bin.join("vite")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            let path = p.join("vite.js");
            let raw = fs::read_to_string(&path).unwrap();
            fs::write(&path, format!("#!/usr/bin/env node\n{raw}")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let output = std::process::Command::new(binary)
        .args(["--", "--filter=@org/web"])
        .current_dir(&f.0)
        .env("PORCH_STATE_DIR", &state)
        .env("PORCH_PORT", port.to_string())
        .env("PORCH_HTTPS", "0")
        .env("PORCH_SYNC_HOSTS", "0")
        .env("PORCH_PREFIX", "")
        .env("CI", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(f.0.join("root-argv.json")).unwrap())
            .unwrap(),
        json!(["--filter=@org/web"])
    );
    let mut ports = Vec::new();
    for app in ["web", "api"] {
        let captured: serde_json::Value = serde_json::from_slice(
            &fs::read(f.0.join(format!("apps/{app}/captured.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            captured["url"],
            format!("http://{app}.org.localhost:{port}")
        );
        let app_port = captured["port"].as_str().unwrap();
        ports.push(app_port.to_owned());
        let argv = captured["args"].as_array().unwrap();
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--port" && w[1] == app_port),
            "{captured}"
        );
    }
    assert_ne!(ports[0], ports[1]);
    let routes: serde_json::Value =
        serde_json::from_slice(&fs::read(state.join("state.json")).unwrap()).unwrap();
    assert!(routes["routes"].as_array().unwrap().is_empty());
}

#[test]
fn root_name_sets_workspace_project_domain_without_colliding_packages() {
    let f = Fixture::new();
    f.write("package.json", json!({"workspaces":["apps/*"]}));
    f.write("portless.json", json!({"name":"shop"}));
    for app in ["web", "api"] {
        f.write(
            &format!("apps/{app}/package.json"),
            json!({"name":app,"scripts":{"dev":"node server.js"}}),
        );
    }
    let plans = launch_plans(&f.0, None, None, None, Some("")).unwrap();
    assert_eq!(
        plans.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["api.shop", "web.shop"]
    );
}
