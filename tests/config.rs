#[cfg(unix)]
mod common;
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
    assert_eq!(one[0].name, "web.org");
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
fn non_javascript_dev_servers_get_port_and_host() {
    for (a, want) in [
        (
            vec!["python3", "-m", "http.server"],
            vec![
                "python3",
                "-m",
                "http.server",
                "--bind",
                "127.0.0.1",
                "4567",
            ],
        ),
        (
            vec!["python", "manage.py", "runserver"],
            vec!["python", "manage.py", "runserver", "127.0.0.1:4567"],
        ),
        (
            vec!["uv", "run", "flask", "run"],
            vec![
                "uv",
                "run",
                "flask",
                "run",
                "--port",
                "4567",
                "--host",
                "127.0.0.1",
            ],
        ),
        (
            vec!["uvicorn", "main:app", "--reload"],
            vec![
                "uvicorn",
                "main:app",
                "--reload",
                "--port",
                "4567",
                "--host",
                "127.0.0.1",
            ],
        ),
        (
            vec!["fastapi", "dev", "main.py"],
            vec![
                "fastapi",
                "dev",
                "main.py",
                "--port",
                "4567",
                "--host",
                "127.0.0.1",
            ],
        ),
        (
            vec!["gunicorn", "app:app"],
            vec!["gunicorn", "app:app", "--bind", "127.0.0.1:4567"],
        ),
        (
            vec!["poetry", "run", "mkdocs", "serve"],
            vec![
                "poetry",
                "run",
                "mkdocs",
                "serve",
                "--dev-addr",
                "127.0.0.1:4567",
            ],
        ),
        (
            vec!["streamlit", "run", "app.py"],
            vec![
                "streamlit",
                "run",
                "app.py",
                "--server.port",
                "4567",
                "--server.address",
                "127.0.0.1",
                "--server.headless",
                "true",
            ],
        ),
        (
            vec!["hugo", "server", "-D"],
            vec![
                "hugo",
                "server",
                "-D",
                "--port",
                "4567",
                "--bind",
                "127.0.0.1",
            ],
        ),
        (
            vec!["bundle", "exec", "jekyll", "serve"],
            vec![
                "bundle",
                "exec",
                "jekyll",
                "serve",
                "--port",
                "4567",
                "--host",
                "127.0.0.1",
            ],
        ),
        (
            vec!["jupyter", "lab"],
            vec![
                "jupyter",
                "lab",
                "--port=4567",
                "--ServerApp.port_retries=0",
                "--no-browser",
            ],
        ),
    ] {
        let (out, _) = adapt_command(&args(&a), Path::new("/tmp"), 4567, false).unwrap();
        assert_eq!(out, args(&want), "{a:?}");
    }
    // The user's own port, address and subcommands are left alone.
    for a in [
        vec!["python3", "-m", "http.server", "8000"],
        vec!["python", "manage.py", "runserver", "0.0.0.0:8000"],
        vec!["python", "manage.py", "migrate"],
        vec!["flask", "routes"],
        vec!["gunicorn", "-b", "0.0.0.0:9000", "app:app"],
        vec!["mkdocs", "build"],
        vec!["hugo"],
        vec!["python3", "script.py"],
        vec!["rails", "server"],
    ] {
        let a = args(&a);
        assert_eq!(
            adapt_command(&a, Path::new("/tmp"), 4567, false).unwrap().0,
            a
        );
    }
    let (out, _) = adapt_command(
        &args(&["flask", "run", "--port=5001"]),
        Path::new("/tmp"),
        4567,
        false,
    )
    .unwrap();
    assert_eq!(
        out,
        args(&["flask", "run", "--port=5001", "--host", "127.0.0.1"])
    );
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
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("node_modules/.bin")).unwrap();
    let shim = f.0.join("node_modules/.bin/vite");
    common::write_executable(
        &shim,
        "#!/bin/sh\nexec node \"$PORCH_CAPTURE_SCRIPT\" \"$@\"\n",
    );
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
                .env("PORCH_CLEAN_URLS", "0")
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
            let path = p.join("vite.js");
            let raw = fs::read_to_string(&path).unwrap();
            common::write_executable(&path, &format!("#!/usr/bin/env node\n{raw}"));
        }
    }
    let port = common::free_port();
    let output = std::process::Command::new(binary)
        .args(["--", "--filter=@org/web"])
        .current_dir(&f.0)
        .env("PORCH_STATE_DIR", &state)
        .env("PORCH_CLEAN_URLS", "0")
        .env("PORCH_PORT", port.to_string())
        .env("PORCH_APP_PORTS", common::app_ports(port))
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

#[test]
fn leaf_does_not_inherit_workspace_root_app_settings() {
    let f = Fixture::new();
    f.write("package.json", json!({"workspaces":["apps/*"],"porch":{"name":"shop","appPort":4901,"script":"start","worktree":{"prefix":"branch"}}}));
    for app in ["web", "api"] {
        f.write(
            &format!("apps/{app}/package.json"),
            json!({"name":app,"scripts":{"dev":"node server.js","start":"node wrong.js"}}),
        );
        let config = effective_config(&f.0.join(format!("apps/{app}"))).unwrap();
        assert!(config.get("name").is_none(), "root name leaked: {config}");
        assert!(
            config.get("appPort").is_none(),
            "root port leaked: {config}"
        );
        assert!(
            config.get("script").is_none(),
            "root script leaked: {config}"
        );
        assert_eq!(config["worktree"]["prefix"], "branch");
        let plan =
            launch_plans(&f.0.join(format!("apps/{app}")), None, None, None, Some("")).unwrap();
        assert_eq!(plan[0].name, format!("{app}.shop"));
        assert_eq!(plan[0].command, args(&["npm", "run", "dev"]));
        assert!(plan[0].app_port.is_none());
    }
    let root = launch_plans(&f.0, None, None, None, Some("")).unwrap();
    assert_eq!(
        root.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["api.shop", "web.shop"]
    );
    assert!(root.iter().all(|p| p.app_port.is_none()));
    assert!(root
        .iter()
        .all(|p| p.command == args(&["npm", "run", "start"])));
}

#[test]
fn all_config_callers_follow_the_same_source_precedence() {
    for (file, porch_key, portless_key, expected) in [
        (Some("porch.json"), Some("dev"), Some("legacy"), "start"),
        (None, Some("dev"), Some("legacy"), "dev"),
        (Some("portless.json"), None, Some("legacy"), "start"),
        (None, None, Some("legacy"), "legacy"),
    ] {
        let f = Fixture::new();
        let mut package = json!({"workspaces":["apps/*"],"scripts":{"dev":"turbo dev","start":"turbo start","legacy":"turbo legacy"}});
        if let Some(script) = porch_key {
            package["porch"] = json!({"script":script});
        }
        if let Some(script) = portless_key {
            package["portless"] = json!({"script":script});
        }
        f.write("package.json", package);
        f.write("turbo.json", json!({"tasks":{}}));
        if let Some(file) = file {
            f.write(file, json!({"script":"start"}));
        }
        f.write("apps/web/package.json", json!({"name":"web","scripts":{"dev":"node dev.js","start":"node start.js","legacy":"node legacy.js"}}));
        assert_eq!(load(&f.0).unwrap()["script"], expected);
        assert_eq!(
            effective_config(&f.0).unwrap()["script"],
            expected,
            "{file:?}"
        );
        assert_eq!(
            turbo_command(&f.0, None).unwrap().unwrap(),
            args(&["npm", "run", expected])
        );
        assert_eq!(
            launch_plans(&f.0, None, None, None, Some("")).unwrap()[0].command,
            args(&["npm", "run", expected])
        );
    }
}

#[test]
fn leaf_sources_override_matching_root_app_with_the_same_precedence() {
    for (file, native, legacy, expected) in [
        (Some("porch.json"), Some("dev"), Some("legacy"), "start"),
        (Some("portless.json"), Some("dev"), Some("legacy"), "dev"),
        (Some("portless.json"), None, Some("legacy"), "start"),
        (None, None, Some("legacy"), "legacy"),
        (None, None, None, "root"),
    ] {
        let f = Fixture::new();
        f.write("package.json", json!({"workspaces":["apps/*"]}));
        f.write("porch.json", json!({"name":"shop","worktree":{"prefix":"branch","hostnameTemplate":"{worktree}-{name}"},"apps":{"apps/web":{"script":"root","name":"root-web","appPort":4901}}}));
        let mut package = json!({"name":"web","scripts":{"root":"node root.js","start":"node start.js","dev":"node dev.js","legacy":"node legacy.js"}});
        if let Some(script) = native {
            package["porch"] = json!({"script":script,"name":script,"appPort":4902,"worktree":{"prefix":"last-segment"}});
        }
        if let Some(script) = legacy {
            package["portless"] = json!({"script":script,"name":script,"appPort":4902,"worktree":{"prefix":"last-segment"}});
        }
        f.write("apps/web/package.json", package);
        if let Some(file) = file {
            f.write(&format!("apps/web/{file}"), json!({"script":"start","name":"start","appPort":4902,"worktree":{"prefix":"last-segment"}}));
        }
        let leaf = f.0.join("apps/web");
        let local = load(&leaf).unwrap();
        let effective = effective_config(&leaf).unwrap();
        if expected == "root" {
            assert!(local.get("script").is_none());
        } else {
            assert_eq!(local["script"], expected);
        }
        assert_eq!(effective["script"], expected);
        assert_eq!(
            effective["name"],
            if expected == "root" {
                "root-web"
            } else {
                expected
            }
        );
        assert_eq!(
            effective["appPort"],
            if expected == "root" { 4901 } else { 4902 }
        );
        assert_eq!(
            effective["worktree"]["hostnameTemplate"],
            "{worktree}-{name}"
        );
        assert_eq!(
            effective["worktree"]["prefix"],
            if expected == "root" {
                "branch"
            } else {
                "last-segment"
            }
        );
        for dir in [&leaf, &f.0] {
            let plan = launch_plans(dir, None, None, None, Some("")).unwrap();
            assert_eq!(plan[0].command, args(&["npm", "run", expected]));
            assert_eq!(plan[0].name, effective["name"].as_str().unwrap());
        }
        assert!(turbo_command(&leaf, None).unwrap().is_none());
    }
}

#[test]
fn malformed_native_configuration_is_not_hidden_by_package_or_legacy_fallbacks() {
    let f = Fixture::new();
    f.write(
        "package.json",
        json!({"workspaces":["apps/*"],"porch":{"script":"dev"},"portless":{"script":"start"}}),
    );
    f.write("portless.json", json!({"script":"start"}));
    f.write("turbo.json", json!({"tasks":{}}));
    f.write(
        "apps/web/package.json",
        json!({"name":"web","scripts":{"dev":"node x"}}),
    );
    fs::write(f.0.join("porch.json"), "{").unwrap();
    assert!(load(&f.0).unwrap_err().to_string().contains("porch.json"));
    assert!(effective_config(&f.0)
        .unwrap_err()
        .to_string()
        .contains("porch.json"));
    assert!(effective_config(&f.0.join("apps/web"))
        .unwrap_err()
        .to_string()
        .contains("porch.json"));
    assert!(launch_plans(&f.0, None, None, None, Some(""))
        .unwrap_err()
        .to_string()
        .contains("porch.json"));
    assert!(turbo_command(&f.0, None)
        .unwrap_err()
        .to_string()
        .contains("porch.json"));
}

#[cfg(unix)]
#[test]
fn simultaneous_leaf_launches_keep_distinct_workspace_routes_and_ports() {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    struct Running {
        children: Vec<Child>,
        state: PathBuf,
    }
    impl Drop for Running {
        fn drop(&mut self) {
            for child in &mut self.children {
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
                let _ = child.wait();
            }
            let _ = Command::new(env!("CARGO_BIN_EXE_porch"))
                .args(["proxy", "stop"])
                .env("PORCH_STATE_DIR", &self.state)
                .env("PORCH_CLEAN_URLS", "0")
                .env("PORCH_SYNC_HOSTS", "0")
                .output();
        }
    }
    let f = Fixture::new();
    let proxy_port = common::free_port();
    let root_app_port = common::free_port();
    f.write("package.json", json!({"workspaces":["apps/*"],"porch":{"name":"shop","appPort":root_app_port,"script":"wrong"}}));
    let mut running = Running {
        children: Vec::new(),
        state: f.0.join("state"),
    };
    for app in ["web", "api"] {
        f.write(
            &format!("apps/{app}/package.json"),
            json!({"name":app,"scripts":{"dev":"node server.cjs","wrong":"node missing.cjs"}}),
        );
        fs::write(f.0.join(format!("apps/{app}/server.cjs")), r#"const fs=require('fs');require('http').createServer((q,s)=>s.end('ok')).listen(Number(process.env.PORT),'127.0.0.1',()=>{fs.writeFileSync('ready.tmp',JSON.stringify({port:process.env.PORT,url:process.env.PORCH_URL||process.env.PORTLESS_URL}));fs.renameSync('ready.tmp','ready.json')})"#).unwrap();
        running.children.push(
            Command::new(env!("CARGO_BIN_EXE_porch"))
                .current_dir(f.0.join(format!("apps/{app}")))
                .env("PORCH_STATE_DIR", &running.state)
                .env("PORCH_CLEAN_URLS", "0")
                .env("PORCH_PORT", proxy_port.to_string())
                .env("PORCH_APP_PORTS", common::app_ports(proxy_port))
                .env("PORCH_HTTPS", "0")
                .env("PORCH_SYNC_HOSTS", "0")
                .env("PORCH_PREFIX", "")
                .env("CI", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !["web", "api"]
        .iter()
        .all(|app| f.0.join(format!("apps/{app}/ready.json")).exists())
    {
        for child in &mut running.children {
            assert!(
                child.try_wait().unwrap().is_none(),
                "leaf launcher exited before its server became ready"
            );
        }
        assert!(
            Instant::now() < deadline,
            "leaf launches did not both become ready"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let captures: Vec<serde_json::Value> = ["web", "api"]
        .iter()
        .map(|app| {
            let capture: serde_json::Value = serde_json::from_slice(
                &fs::read(f.0.join(format!("apps/{app}/ready.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(
                capture["url"],
                format!("http://{app}.shop.localhost:{proxy_port}")
            );
            capture
        })
        .collect();
    assert_ne!(captures[0]["port"], captures[1]["port"]);
    let routes: serde_json::Value =
        serde_json::from_slice(&fs::read(running.state.join("state.json")).unwrap()).unwrap();
    assert_eq!(routes["routes"].as_array().unwrap().len(), 2);
}

#[test]
fn explicit_workspace_graph_commands_reject_a_shared_app_port() {
    let f = Fixture::new();
    f.write(
        "package.json",
        json!({"workspaces":["apps/*"],"scripts":{"dev":"turbo run dev"}}),
    );
    f.write("turbo.json", json!({"tasks":{"dev":{"persistent":true}}}));
    for app in ["web", "api"] {
        f.write(
            &format!("apps/{app}/package.json"),
            json!({"name":app,"scripts":{"dev":"vite"}}),
        );
    }
    for command in [
        args(&["pnpm", "run", "dev"]),
        args(&["npm", "run", "dev"]),
        args(&["yarn", "dev"]),
        args(&["bun", "run", "dev"]),
        args(&["turbo", "run", "dev"]),
        args(&["pnpm", "exec", "turbo", "run", "dev"]),
        args(&["turbo", "run", "dev", "--filter=*"]),
    ] {
        let error = launch_plans(&f.0, Some(command.clone()), Some("shop"), None, Some(""))
            .unwrap_err()
            .to_string();
        assert!(error.contains("bare `porch`"), "{command:?}: {error}");
    }
    assert_eq!(
        launch_plans(&f.0, None, None, None, Some(""))
            .unwrap()
            .len(),
        2
    );
    assert!(launch_plans(
        &f.0,
        Some(args(&["node", "server.js"])),
        Some("single"),
        None,
        Some("")
    )
    .is_ok());
}
#[test]
fn package_manager_script_shorthands_resolve_framework_flags() {
    let f = Fixture::new();
    for (raw, framework_flags) in [
        ("astro dev", vec!["--port", "4567", "--host", "127.0.0.1"]),
        (
            "vite",
            vec!["--port", "4567", "--strictPort", "--host", "127.0.0.1"],
        ),
    ] {
        f.write("package.json", json!({"scripts":{"dev":raw,"install":raw}}));
        for invocation in [
            &["pnpm", "dev"][..],
            &["pnpm", "run", "dev"],
            &["yarn", "dev"],
            &["bun", "dev"],
            &["bun", "run", "dev"],
        ] {
            let (out, host) = adapt_command(&args(invocation), &f.0, 4567, false).unwrap();
            let mut expected = args(invocation);
            expected.extend(args(&framework_flags));
            assert_eq!(out, expected, "{raw}: {invocation:?}");
            assert_eq!(host.as_deref(), Some("127.0.0.1"));
        }
        let (out, _) = adapt_command(&args(&["npm", "run", "dev"]), &f.0, 4567, false).unwrap();
        let mut expected = args(&["npm", "run", "dev", "--"]);
        expected.extend(args(&framework_flags));
        assert_eq!(out, expected, "{raw}: npm run dev");
        // npm has no script shorthand; builtins never resolve same-named scripts.
        for untouched in [&["npm", "dev"][..], &["pnpm", "install"]] {
            let (out, _) = adapt_command(&args(untouched), &f.0, 4567, false).unwrap();
            assert_eq!(out, args(untouched));
        }
        // Forwarded arguments after the shorthand are respected.
        let (out, _) =
            adapt_command(&args(&["pnpm", "dev", "--port", "9999"]), &f.0, 4567, false).unwrap();
        assert!(!out.iter().any(|s| s == "4567"), "{out:?}");
    }
}
#[test]
fn child_path_takes_bins_from_the_project_only() {
    let f = Fixture::new();
    let bin = |dir: &str| {
        let p = f.0.join(dir).join("node_modules/.bin");
        fs::create_dir_all(&p).unwrap();
        p
    };
    // Someone else's directory above the project, such as /tmp or C:\.
    let planted = bin("outer");
    fs::create_dir_all(f.0.join("outer/repo/.git")).unwrap();
    let root_bin = bin("outer/repo");
    let app_bin = bin("outer/repo/app");
    let cwd = f.0.join("outer/repo/app/src");
    fs::create_dir_all(&cwd).unwrap();
    let path = augmented_path(&cwd);
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    assert_eq!(dirs[..2], [app_bin, root_bin], "{dirs:?}");
    assert!(!dirs.contains(&planted), "{dirs:?}");
    // Outside any project and outside the home directory: this directory only.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    if !home.is_some_and(|h| f.0.starts_with(h)) {
        let loose = f.0.join("loose");
        let own = bin("loose");
        let above = bin("");
        let dirs: Vec<PathBuf> = std::env::split_paths(&augmented_path(&loose)).collect();
        assert_eq!(dirs.first(), Some(&own), "{dirs:?}");
        assert!(!dirs.contains(&above), "{dirs:?}");
    }
}
#[cfg(unix)]
#[test]
fn child_path_skips_bins_owned_by_another_user() {
    use std::os::unix::fs::MetadataExt;
    // /tmp and similar shared directories are root-owned; a directory there
    // made by this user is fine. A different owner needs root to set up.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let f = Fixture::new();
    fs::create_dir_all(f.0.join(".git")).unwrap();
    let theirs = f.0.join("node_modules/.bin");
    fs::create_dir_all(&theirs).unwrap();
    let nobody = 65534;
    std::os::unix::fs::chown(f.0.join("node_modules"), Some(nobody), None).unwrap();
    assert_eq!(
        fs::metadata(f.0.join("node_modules")).unwrap().uid(),
        nobody
    );
    let dirs: Vec<PathBuf> = std::env::split_paths(&augmented_path(&f.0)).collect();
    assert!(!dirs.contains(&theirs), "{dirs:?}");
}
