//! Opt-in login startup for the discovery agent, so sharing survives restarts.
//! A per-user LaunchAgent on macOS, a systemd user unit on Linux and a
//! scheduled task that runs as the user at logon on Windows: no elevation,
//! nothing system-wide. Only ever installed by an explicit command or by
//! `porch setup` when both start-at-login and sharing are chosen.
use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub const LABEL: &str = "dev.porch.discovery";
const UNIT: &str = "porch-discovery.service";
const TASK: &str = "PorchDiscovery";

fn platform() -> String {
    std::env::var("PORCH_SERVICE_PLATFORM").unwrap_or_else(|_| std::env::consts::OS.into())
}

fn file(platform: &str) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("PORCH_DISCOVERY_AUTOSTART_FILE") {
        return Ok(p.into());
    }
    // Task Scheduler keeps its own copy; this one marks what Porch installed.
    if platform == "windows" {
        return std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(|d| PathBuf::from(d).join("Porch").join(format!("{LABEL}.xml")))
            .context("LOCALAPPDATA is not set");
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")?;
    Ok(match platform {
        "macos" => home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")),
        "linux" => std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("systemd/user")
            .join(UNIT),
        _ => bail!("discovery autostart supports macOS, Linux and Windows"),
    })
}

/// A quoted systemd `Environment=` assignment. Unlike command lines, systemd
/// does not expand `$` there, so it must not be doubled; `%` specifiers are.
fn systemd_env(assignment: &str) -> Result<String> {
    if assignment.contains(['\n', '\r', '\0']) {
        bail!("invalid service environment");
    }
    Ok(format!(
        "\"{}\"",
        assignment
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

/// The line that ties a startup file to one Porch state directory.
fn state_marker(platform: &str, state: &Path) -> Result<String> {
    use crate::integrations::{windows_quote, xml};
    Ok(match platform {
        "macos" => format!(
            "<key>PORCH_STATE_DIR</key><string>{}</string>",
            xml(&state.to_string_lossy())
        ),
        "windows" => format!(
            "<Arguments>--state-dir {} ",
            xml(&windows_quote(&state.to_string_lossy()))
        ),
        _ => format!(
            "Environment={}",
            systemd_env(&format!("PORCH_STATE_DIR={}", state.display()))?
        ),
    })
}
/// The startup file exists and starts the agent for this state directory. An
/// isolated `PORCH_STATE_DIR` never sees, or removes, the real one.
fn installed_here(platform: &str, path: &Path) -> bool {
    let state = crate::state::dir();
    match (fs::read_to_string(path), state_marker(platform, &state)) {
        (Ok(body), Ok(marker)) => body.contains(LABEL) && body.contains(&marker),
        _ => false,
    }
}
/// The program a startup file runs, as written by [`render`].
fn recorded_program(platform: &str, body: &str) -> Option<PathBuf> {
    fn unxml(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }
    let between = |start: &str, end: &str| {
        let rest = &body[body.find(start)? + start.len()..];
        Some(unxml(&rest[..rest.find(end)?]))
    };
    let program = match platform {
        "macos" => between("<key>ProgramArguments</key><array><string>", "</string>")?,
        "windows" => between("<Command>", "</Command>")?,
        _ => {
            let line = body.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
            let quoted = line.strip_prefix('"')?;
            let mut out = String::new();
            let mut chars = quoted.chars();
            loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => out.push(chars.next()?),
                    '%' | '$' => {
                        let c = chars.next()?;
                        out.push(c);
                    }
                    c => out.push(c),
                }
            }
            out
        }
    };
    Some(PathBuf::from(program))
}
/// The program an installed startup file runs, when it no longer exists
/// (a package manager removed that version): login startup is then broken.
pub fn missing_program() -> Option<PathBuf> {
    let platform = platform();
    let path = file(&platform).ok()?;
    if !installed_here(&platform, &path) {
        return None;
    }
    let program = recorded_program(&platform, &fs::read_to_string(&path).ok()?)?;
    (!program.exists()).then_some(program)
}
/// Whether working login startup is installed for this user and state
/// directory. A startup file whose program is gone does not count.
pub fn installed() -> bool {
    let platform = platform();
    file(&platform).is_ok_and(|p| installed_here(&platform, &p)) && missing_program().is_none()
}
/// One line for `porch discovery autostart status`.
pub fn status() -> String {
    match missing_program() {
        Some(program) => format!(
            "broken: {} no longer exists; run `porch discovery autostart enable` again",
            program.display()
        ),
        None if installed() => "enabled".into(),
        None => "disabled".into(),
    }
}

/// Refuse to touch a file at our path that Porch did not write.
fn check_owned(path: &Path) -> Result<()> {
    match fs::read_to_string(path) {
        Ok(body) if !body.contains(LABEL) => bail!(
            "refusing to modify unrelated startup file: {}",
            path.display()
        ),
        _ => Ok(()),
    }
}

/// The path to record in the startup file. Package managers (Nix, Homebrew,
/// version managers) install each version at its own path and point a stable
/// link at the current one, and upgrades delete the old version: keep the
/// link Porch was started through rather than the file it resolves to.
fn program() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let invoked = std::env::args_os()
        .next()
        .map(PathBuf::from)
        .and_then(|argv0| {
            if argv0.components().count() > 1 {
                std::env::current_dir().ok().map(|cwd| cwd.join(argv0))
            } else {
                std::env::split_paths(&std::env::var_os("PATH")?)
                    .map(|dir| dir.join(&argv0))
                    .find(|p| p.is_file())
            }
        });
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    let chosen = match invoked {
        Some(p) if p.is_absolute() && same(&p, &exe) => p,
        _ => exe,
    };
    Ok(stable_exe(
        &chosen.canonicalize().unwrap_or(chosen.clone()),
        &chosen,
    ))
}

/// Homebrew upgrades delete the versioned Cellar path, so point the startup
/// file at the stable `<prefix>/bin/porch` link when one exists. Otherwise
/// `fallback`, the path Porch was started through.
pub(super) fn stable_exe(exe: &Path, fallback: &Path) -> PathBuf {
    let parts: Vec<_> = exe.components().collect();
    if let Some(i) = parts
        .windows(2)
        .position(|w| w[0].as_os_str() == "Cellar" && w[1].as_os_str() == "porch")
    {
        let link = parts[..i].iter().collect::<PathBuf>().join("bin/porch");
        if link.exists() {
            return link;
        }
    }
    fallback.to_path_buf()
}

pub(super) fn render(platform: &str, exe: &Path, state: &Path) -> Result<String> {
    use crate::integrations::{systemd_quote, xml};
    let exe = exe.to_string_lossy();
    let log = state.join("discovery.log");
    match platform {
        // KeepAlive only on failure: a crash, or an interface (such as a
        // Tailscale IP) that is not up yet at login, is retried every 30s.
        // A clean exit (already running, sharing off) stays stopped.
        "macos" => Ok(format!(
            concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
                "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
                "<plist version=\"1.0\"><dict>",
                "<key>Label</key><string>{label}</string>",
                "<key>ProgramArguments</key><array><string>{exe}</string>",
                "<string>discovery</string><string>serve</string><string>--autostart</string></array>",
                "<key>EnvironmentVariables</key><dict><key>PORCH_STATE_DIR</key><string>{state}</string></dict>",
                "<key>StandardOutPath</key><string>{log}</string>",
                "<key>StandardErrorPath</key><string>{log}</string>",
                "<key>RunAtLoad</key><true/>",
                "<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>",
                "<key>ThrottleInterval</key><integer>30</integer>",
                "<key>ProcessType</key><string>Background</string>",
                "</dict></plist>\n"
            ),
            label = LABEL,
            exe = xml(&exe),
            state = xml(&state.to_string_lossy()),
            log = xml(&log.to_string_lossy()),
        )),
        "linux" => Ok(format!(
            "# {LABEL}: managed by `porch discovery autostart`\n[Unit]\nDescription=Porch discovery agent\nAfter=network-online.target\n[Service]\nExecStart={} discovery serve --autostart\nEnvironment={}\nRestart=on-failure\nRestartSec=30\n[Install]\nWantedBy=default.target\n",
            systemd_quote(&exe)?,
            systemd_env(&format!("PORCH_STATE_DIR={}", state.display()))?,
        )),
        "windows" => render_task(&exe, state, &windows_user()?),
        _ => bail!("discovery autostart supports macOS, Linux and Windows"),
    }
}

/// `DOMAIN\user` of the current account, which the logon trigger is bound to.
fn windows_user() -> Result<String> {
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|v| !v.is_empty())
        .context("USERNAME is not set")?;
    Ok(match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    })
}

/// A per-user task: starts at this user's logon, runs as them without
/// elevation, retries a failed start every minute (Task Scheduler's minimum).
fn render_task(exe: &str, state: &Path, user: &str) -> Result<String> {
    use crate::integrations::{windows_quote, xml};
    Ok(format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">",
            "<RegistrationInfo><Description>{label}: Porch discovery agent, managed by `porch discovery autostart`</Description></RegistrationInfo>",
            "<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>",
            "<Principals><Principal id=\"Author\"><UserId>{user}</UserId>",
            "<LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>",
            "<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure></Settings>",
            "<Actions Context=\"Author\"><Exec><Command>{exe}</Command>",
            "<Arguments>--state-dir {state} discovery serve --autostart</Arguments></Exec></Actions></Task>\n"
        ),
        label = LABEL,
        user = xml(user),
        exe = xml(exe),
        state = xml(&windows_quote(&state.to_string_lossy())),
    ))
}

#[cfg(unix)]
fn gui_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}
#[cfg(not(unix))]
fn gui_domain() -> String {
    String::new()
}

fn write_atomic(path: &Path, body: &str) -> Result<()> {
    let parent = path.parent().context("startup file has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{LABEL}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = fs::File::create(&temporary)?;
        // launchd refuses agents that others can modify; do not inherit a
        // permissive umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o644))?;
        }
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

/// Unload and delete the startup file. Stops a supervised agent as a side effect.
fn unload(platform: &str, path: &Path) -> Result<()> {
    use crate::integrations::exec;
    match platform {
        "macos" => {
            // Not loaded is fine; the goal is "not loaded".
            let _ = exec(
                "launchctl",
                &["bootout", &format!("{}/{LABEL}", gui_domain())],
            );
        }
        "linux" => {
            let _ = exec("systemctl", &["--user", "disable", "--now", UNIT]);
        }
        "windows" => {
            let _ = exec("schtasks", &["/End", "/TN", TASK]);
            let _ = exec("schtasks", &["/Delete", "/TN", TASK, "/F"]);
        }
        _ => {}
    }
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if platform == "linux" {
        let _ = exec("systemctl", &["--user", "daemon-reload"]);
    }
    Ok(())
}

/// Called when sharing is turned off: nothing should start at the next login.
pub(super) fn remove() -> Result<()> {
    let platform = platform();
    let Ok(path) = file(&platform) else {
        return Ok(());
    };
    if !installed_here(&platform, &path) {
        return Ok(());
    }
    unload(&platform, &path)
}

/// Install login startup and hand the agent to launchd/systemd now. Returns
/// whether the supervised agent became ready (it may be waiting for its
/// interface, which launchd/systemd keeps retrying).
pub async fn enable() -> Result<bool> {
    let _lock = super::lock("discovery.start.lock")?;
    if super::server::config()?.is_none() {
        bail!("sharing is off; turn it on with `porch pair` (or `porch discovery enable --bind IP`) first")
    }
    let platform = platform();
    let path = file(&platform)?;
    check_owned(&path)?;
    if path.exists() && !installed_here(&platform, &path) {
        bail!(
            "{} already starts Porch discovery for another state directory; run `porch discovery autostart disable` with that PORCH_STATE_DIR first",
            path.display()
        )
    }
    let exe = program()?;
    let state = crate::state::dir();
    let body = render(&platform, &exe, &state)?;
    // launchd/systemd append to the log; keep it private like every other agent log.
    drop(super::private_file(state.join("discovery.log"))?);
    write_atomic(&path, &body)?;
    // From here on launchd/systemd supervise the agent: stop a hand-started one.
    if let Some(identity) = super::process_identity("discovery") {
        if let Err(e) = super::stop_identity(&identity).await {
            // Nothing was loaded: leave no file that says otherwise.
            let _ = fs::remove_file(&path);
            return Err(e);
        }
    }
    let p = path.to_string_lossy();
    let started = match platform.as_str() {
        "macos" => reload_launch_agent(&p).await,
        "linux" => crate::integrations::exec("systemctl", &["--user", "daemon-reload"])
            .and_then(|_| crate::integrations::exec("systemctl", &["--user", "enable", UNIT]))
            .and_then(|_| crate::integrations::exec("systemctl", &["--user", "restart", UNIT])),
        "windows" => {
            crate::integrations::exec("schtasks", &["/Create", "/TN", TASK, "/XML", &p, "/F"])
                .and_then(|_| crate::integrations::exec("schtasks", &["/Run", "/TN", TASK]))
        }
        _ => unreachable!("file() rejects other platforms"),
    };
    if let Err(e) = started {
        // Do not leave a half-installed startup file or sharing switched off.
        let _ = fs::remove_file(&path);
        let _ = super::spawn_agent().await;
        return Err(e.context("loading the login startup agent"));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        if super::running() {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(false)
}

/// Replace a loaded LaunchAgent with the file at `plist`. `bootout` returns
/// before the old job is gone, and `bootstrap` fails (error 5) until it is,
/// so wait for it, and retry briefly in case it lingers.
async fn reload_launch_agent(plist: &str) -> Result<String> {
    use crate::integrations::exec;
    let target = format!("{}/{LABEL}", gui_domain());
    let _ = exec("launchctl", &["bootout", &target]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while exec("launchctl", &["print", &target]).is_ok() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut result = exec("launchctl", &["bootstrap", &gui_domain(), plist]);
    for _ in 0..5 {
        if result.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        result = exec("launchctl", &["bootstrap", &gui_domain(), plist]);
    }
    result
}

/// Remove login startup. Sharing stays on: the agent keeps running unsupervised.
pub async fn disable() -> Result<()> {
    let _lock = super::lock("discovery.start.lock")?;
    let platform = platform();
    let path = file(&platform)?;
    if !installed_here(&platform, &path) {
        return Ok(());
    }
    // The supervised agent being unloaded. Unloading only asks the service
    // manager to stop it, so it can still look like the running agent right
    // after: wait until it is gone before deciding whether to start a
    // replacement, or sharing would silently end when it exits.
    let supervised = super::process_identity("discovery");
    unload(&platform, &path)?;
    if let Some(identity) = supervised {
        super::stop_identity(&identity).await?;
    }
    if super::server::config()?.is_some() && !super::running() {
        super::spawn_agent().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_agent_runs_serve_autostart_with_escaped_paths() {
        let body = render(
            "macos",
            Path::new("/Users/a&b/.local/bin/porch"),
            Path::new("/Users/a&b/state"),
        )
        .unwrap();
        assert!(body.contains("<string>dev.porch.discovery</string>"));
        assert!(body.contains("<string>/Users/a&amp;b/.local/bin/porch</string><string>discovery</string><string>serve</string><string>--autostart</string>"));
        assert!(body.contains("<key>PORCH_STATE_DIR</key><string>/Users/a&amp;b/state</string>"));
        let log = Path::new("/Users/a&b/state").join("discovery.log");
        let log = crate::integrations::xml(&log.to_string_lossy());
        assert!(body.contains(&format!("<string>{log}</string>")));
        assert!(body.contains("<key>SuccessfulExit</key><false/>"));
        assert!(body.contains("<key>RunAtLoad</key><true/>"));
    }

    #[test]
    fn systemd_user_unit_quotes_and_restarts_only_on_failure() {
        let body = render(
            "linux",
            Path::new("/home/me/my bin/porch"),
            Path::new("/home/me/.porch"),
        )
        .unwrap();
        assert!(body.starts_with("# dev.porch.discovery"));
        assert!(body.contains("ExecStart=\"/home/me/my bin/porch\" discovery serve --autostart"));
        assert!(body.contains("Environment=\"PORCH_STATE_DIR=/home/me/.porch\""));
        // systemd expands `$` in ExecStart= but not in Environment=.
        let body = render(
            "linux",
            Path::new("/home/me/$bin/porch"),
            Path::new("/home/me/$state 100%"),
        )
        .unwrap();
        assert!(body.contains("ExecStart=\"/home/me/$$bin/porch\""));
        assert!(body.contains("Environment=\"PORCH_STATE_DIR=/home/me/$state 100%%\""));
        assert!(body.contains("Restart=on-failure"));
        assert!(body.contains("WantedBy=default.target"));
        assert!(render("freebsd", Path::new("p"), Path::new("s")).is_err());
    }

    #[test]
    fn windows_task_runs_as_the_user_at_logon_for_this_state_dir() {
        let body = render_task(
            r"C:\Program Files\Porch\porch.exe",
            Path::new(r"C:\Users\a&b\.porch"),
            r"PC\a&b",
        )
        .unwrap();
        assert!(body.contains(LABEL));
        assert!(body.contains("<LogonTrigger><Enabled>true</Enabled><UserId>PC\\a&amp;b</UserId>"));
        assert!(body.contains(
            "<LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel>"
        ));
        assert!(body.contains(r"<Command>C:\Program Files\Porch\porch.exe</Command>"));
        let marker = state_marker("windows", Path::new(r"C:\Users\a&b\.porch")).unwrap();
        assert!(body.contains(&marker), "{marker}");
        assert!(body.contains("discovery serve --autostart</Arguments>"));
    }

    #[test]
    fn the_recorded_program_is_read_back_from_every_format() {
        for (platform, exe) in [
            ("macos", r"/Users/a&b/my bin/porch"),
            ("linux", r#"/home/me/$x "q" \ 100%/porch"#),
        ] {
            let body = render(platform, Path::new(exe), Path::new("/s")).unwrap();
            assert_eq!(recorded_program(platform, &body), Some(PathBuf::from(exe)));
        }
        let body = render_task(r"C:\P&Q\porch.exe", Path::new(r"C:\s"), "u").unwrap();
        assert_eq!(
            recorded_program("windows", &body),
            Some(PathBuf::from(r"C:\P&Q\porch.exe"))
        );
    }

    #[test]
    fn homebrew_cellar_binaries_use_the_stable_prefix_link() {
        let root = std::env::temp_dir().join(format!("porch-autostart-{}", std::process::id()));
        let cellar = root.join("Cellar/porch/0.3.0/bin/porch");
        fs::create_dir_all(cellar.parent().unwrap()).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(&cellar, "").unwrap();
        // No link yet: keep the path Porch was started through.
        assert_eq!(stable_exe(&cellar, &cellar), cellar);
        fs::write(root.join("bin/porch"), "").unwrap();
        assert_eq!(stable_exe(&cellar, &cellar), root.join("bin/porch"));
        // Elsewhere (a Nix profile link to a store path) the link is kept.
        let store = Path::new("/nix/store/abc-porch-0.5.2/bin/porch");
        let profile = Path::new("/home/me/.nix-profile/bin/porch");
        assert_eq!(stable_exe(store, profile), profile);
        let _ = fs::remove_dir_all(root);
    }
}
