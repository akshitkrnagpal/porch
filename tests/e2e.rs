//! Real-framework acceptance: real dev servers (Vite, Next.js, Python's
//! http.server) run under `porch run`, and their pages and HMR sockets are
//! checked through the proxy the way a browser uses them. The npm-based tests
//! need network access, so they run only with `PORCH_E2E=1` (CI sets it).
mod common;
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Launch {
    child: Child,
    root: PathBuf,
    state: PathBuf,
}
impl Drop for Launch {
    fn drop(&mut self) {
        // Let Porch stop its app (Vite) the way Ctrl+C would, then insist.
        #[cfg(unix)]
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new(env!("CARGO_BIN_EXE_porch"))
            .args(["proxy", "stop"])
            .env("PORCH_STATE_DIR", &self.state)
            .output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn vite_page_and_hmr_websocket_work_through_porch() {
    if std::env::var("PORCH_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set PORCH_E2E=1 to install Vite from npm and run it");
        return;
    }
    let root = std::env::temp_dir().join(format!("porch-e2e-vite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"name":"e2e-vite","private":true,"type":"module","scripts":{"dev":"vite"}}"#,
    )
    .unwrap();
    std::fs::write(
        app.join("index.html"),
        "<!doctype html><title>porch e2e</title><script type=\"module\" src=\"/main.js\"></script>",
    )
    .unwrap();
    std::fs::write(app.join("main.js"), "document.title = 'porch e2e ready';\n").unwrap();
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let install = Command::new(npm)
        .args(["install", "--no-audit", "--no-fund", "--silent", "vite@6"])
        .current_dir(&app)
        .status()
        .expect("npm is required for PORCH_E2E");
    assert!(install.success(), "npm install vite failed");

    let port = common::free_port();
    let state = root.join("state");
    let mut command = Command::new(env!("CARGO_BIN_EXE_porch"));
    command
        .args(["run", "--name", "e2e", "--prefix", "", "npm", "run", "dev"])
        .current_dir(&app)
        .env("PORCH_STATE_DIR", &state)
        .env("PORCH_PORT", port.to_string())
        .env("PORCH_APP_PORTS", common::app_ports(port))
        .env("PORCH_CLEAN_URLS", "0")
        .env("PORCH_HTTPS", "0")
        .env("PORCH_SYNC_HOSTS", "0")
        .env("PORCH_SKIP_TRUST", "1")
        .env("CI", "1")
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let launch = Launch {
        child: command.spawn().unwrap(),
        root: root.clone(),
        state,
    };

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        // The page, served by Vite through the proxy under its Porch name.
        let page = loop {
            if let Ok(r) = client
                .get(format!("http://127.0.0.1:{port}/"))
                .header("host", "e2e.localhost")
                .header("accept", "text/html")
                .send()
                .await
            {
                if r.status().is_success() {
                    let text = r.text().await.unwrap();
                    if text.contains("/@vite/client") {
                        break text;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "Vite never answered through Porch"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        assert!(page.contains("porch e2e"), "{page}");
        // Vite only serves its client module when the Host is allowed, which
        // Porch arranges through __VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS.
        let module = client
            .get(format!("http://127.0.0.1:{port}/main.js"))
            .header("host", "e2e.localhost")
            .send()
            .await
            .unwrap();
        assert!(module.status().is_success(), "{}", module.status());
        // HMR: Vite's WebSocket upgrade through the proxy answers "connected".
        // Like the browser, take the HMR token from Vite's client module.
        let vite_client = client
            .get(format!("http://127.0.0.1:{port}/@vite/client"))
            .header("host", "e2e.localhost")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let token = vite_client
            .split("wsToken = \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_default()
            .to_owned();
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
        let mut request = format!("ws://127.0.0.1:{port}/?token={token}")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("host", "e2e.localhost".parse().unwrap());
        request
            .headers_mut()
            .insert("sec-websocket-protocol", "vite-hmr".parse().unwrap());
        request.headers_mut().insert(
            "origin",
            format!("http://e2e.localhost:{port}").parse().unwrap(),
        );
        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(response.status(), 101);
        let first = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("no HMR message")
            .unwrap()
            .unwrap();
        let Message::Text(text) = first else {
            panic!("unexpected HMR frame {first:?}")
        };
        assert!(text.contains("\"connected\""), "{text}");
        // An edit reaches the browser as an HMR update over the same socket.
        std::fs::write(
            app.join("main.js"),
            "document.title = 'porch e2e edited';\n",
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let frame = tokio::time::timeout(remaining, socket.next())
                .await
                .expect("no HMR update after editing main.js")
                .unwrap()
                .unwrap();
            if let Message::Text(text) = frame {
                if text.contains("update") || text.contains("full-reload") {
                    break;
                }
            }
        }
    });
    drop(launch);
}

/// Start `porch run` in `dir` with isolated state on its own proxy port.
fn porch_run(root: &std::path::Path, dir: &std::path::Path, port: u16, args: &[&str]) -> Launch {
    let state = root.join("state");
    let mut command = Command::new(env!("CARGO_BIN_EXE_porch"));
    command
        .args(args)
        .current_dir(dir)
        .env("PORCH_STATE_DIR", &state)
        .env("PORCH_PORT", port.to_string())
        .env("PORCH_APP_PORTS", common::app_ports(port))
        .env("PORCH_CLEAN_URLS", "0")
        .env("PORCH_HTTPS", "0")
        .env("PORCH_SYNC_HOSTS", "0")
        .env("PORCH_SKIP_TRUST", "1")
        .env("NEXT_TELEMETRY_DISABLED", "1")
        .env("CI", "1")
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    Launch {
        child: command.spawn().unwrap(),
        root: root.to_path_buf(),
        state,
    }
}

/// GET `path` on `host` through the proxy until the body contains `needle`.
async fn wait_for_page(port: u16, host: &str, needle: &str, within: Duration) -> String {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + within;
    loop {
        if let Ok(r) = client
            .get(format!("http://127.0.0.1:{port}/"))
            .header("host", host)
            .header("accept", "text/html")
            .send()
            .await
        {
            if r.status().is_success() {
                let text = r.text().await.unwrap_or_default();
                if text.contains(needle) {
                    return text;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "{host} never served {needle:?} through Porch"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// A non-JavaScript server that ignores PORT: Porch adds the port and bind
/// address to `python3 -m http.server` itself. Needs only Python, so it
/// always runs.
#[test]
fn python_http_server_gets_its_port_from_porch() {
    let root = std::env::temp_dir().join(format!("porch-e2e-py-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let site = root.join("site");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(
        site.join("index.html"),
        "<h1>served by python through porch</h1>",
    )
    .unwrap();
    let port = common::free_port();
    let launch = porch_run(
        &root,
        &site,
        port,
        &[
            "run",
            "--name",
            "py",
            "--prefix",
            "",
            "python3",
            "-m",
            "http.server",
        ],
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(wait_for_page(
        port,
        "py.localhost",
        "served by python through porch",
        Duration::from_secs(30),
    ));
    drop(launch);
}

/// Next.js reads PORT on its own; its dev server and HMR socket must work
/// under the Porch name.
#[test]
fn nextjs_page_and_hmr_socket_work_through_porch() {
    if std::env::var("PORCH_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set PORCH_E2E=1 to install Next.js from npm and run it");
        return;
    }
    let root = std::env::temp_dir().join(format!("porch-e2e-next-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("app");
    std::fs::create_dir_all(app.join("app")).unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"name":"e2e-next","private":true,"scripts":{"dev":"next dev"}}"#,
    )
    .unwrap();
    std::fs::write(
        app.join("app/layout.js"),
        "export default function Layout({ children }) { return <html><body>{children}</body></html>; }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("app/page.js"),
        "export default function Page() { return <h1>next through porch</h1>; }\n",
    )
    .unwrap();
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let install = Command::new(npm)
        .args([
            "install",
            "--no-audit",
            "--no-fund",
            "--silent",
            "next@15",
            "react@19",
            "react-dom@19",
        ])
        .current_dir(&app)
        .status()
        .expect("npm is required for PORCH_E2E");
    assert!(install.success(), "npm install next failed");
    let port = common::free_port();
    let launch = porch_run(
        &root,
        &app,
        port,
        &["run", "--name", "next", "--prefix", "", "npm", "run", "dev"],
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        // The first compile can be slow on a cold CI runner.
        let page = wait_for_page(
            port,
            "next.localhost",
            "next through porch",
            Duration::from_secs(180),
        )
        .await;
        assert!(page.contains("/_next/"), "{page}");
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://127.0.0.1:{port}/_next/webpack-hmr")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("host", "next.localhost".parse().unwrap());
        request.headers_mut().insert(
            "origin",
            format!("http://next.localhost:{port}").parse().unwrap(),
        );
        let (_socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(response.status(), 101);
    });
    drop(launch);
}

/// Astro ignores PORT: Porch must add `--port` and `--host` to `astro dev`.
#[test]
fn astro_gets_its_port_flags_from_porch() {
    if std::env::var("PORCH_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set PORCH_E2E=1 to install Astro from npm and run it");
        return;
    }
    let root = std::env::temp_dir().join(format!("porch-e2e-astro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("app");
    std::fs::create_dir_all(app.join("src/pages")).unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"name":"e2e-astro","private":true,"type":"module","scripts":{"dev":"astro dev"}}"#,
    )
    .unwrap();
    std::fs::write(
        app.join("src/pages/index.astro"),
        "---\n---\n<html><body><h1>astro through porch</h1></body></html>\n",
    )
    .unwrap();
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let install = Command::new(npm)
        .args(["install", "--no-audit", "--no-fund", "--silent", "astro@5"])
        .current_dir(&app)
        .status()
        .expect("npm is required for PORCH_E2E");
    assert!(install.success(), "npm install astro failed");
    let port = common::free_port();
    let launch = porch_run(
        &root,
        &app,
        port,
        &[
            "run", "--name", "astro", "--prefix", "", "npm", "run", "dev",
        ],
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(wait_for_page(
        port,
        "astro.localhost",
        "astro through porch",
        Duration::from_secs(120),
    ));
    drop(launch);
}
