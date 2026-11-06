//! `porch doctor [--fix] [--yes]`: find first-use problems and apply safe fixes.
//!
//! Each issue prints as `problem → fix applied` or `problem → command to run`.
//! Terminating processes (stopped or orphaned launches) always needs
//! confirmation or `--yes`.
use crate::{cli, health::Health, ports, state};
use anyhow::Result;

struct Report {
    fix: bool,
    count: usize,
}
impl Report {
    fn issue(&mut self, problem: impl std::fmt::Display, outcome: impl std::fmt::Display) {
        if self.count == 0 {
            println!("\nIssues:");
        }
        self.count += 1;
        println!("- {problem} → {outcome}");
    }
    fn hint(&self) -> &'static str {
        if self.fix {
            "run `porch doctor --fix --yes`"
        } else {
            "run `porch doctor --fix`"
        }
    }
}

pub async fn run(fix: bool, yes: bool) -> Result<()> {
    let mut report = Report { fix, count: 0 };
    let interactive = crate::setup::interactive(yes);
    let settings = state::proxy_settings()?;

    // Hosts sync on a high port is the legacy split-setting path that produced
    // hosts-sync 403s; high ports imply no-sudo mode.
    if !settings.unprivileged
        && settings.port >= ports::FIRST_UNPRIVILEGED_PORT
        && state::env("SYNC_HOSTS").as_deref() != Some("1")
    {
        let problem = format!(
            "port {} is saved with hosts sync enabled (needs sudo for no benefit on .localhost)",
            settings.port
        );
        if fix {
            let mut s = settings.clone();
            s.unprivileged = true;
            state::save_proxy_settings(&s)?;
            report.issue(problem, "switched to no-sudo mode (saved)");
        } else {
            report.issue(problem, report.hint());
        }
    }

    // Proxy outdated or its port is taken by something else.
    if cli::proxy_outdated() {
        let problem = format!(
            "the running proxy is {} but this porch is {}",
            cli::running_proxy_version().unwrap_or_else(|| "an older version".into()),
            env!("CARGO_PKG_VERSION")
        );
        if fix {
            match cli::restart_outdated_proxy().await {
                Ok(_) => report.issue(problem, "restarted the proxy; routes kept"),
                Err(e) => report.issue(problem, format!("restart failed ({e:#})")),
            }
        } else {
            report.issue(problem, report.hint());
        }
    } else if !cli::proxy_running() {
        let settings = state::proxy_settings()?;
        if ports::has_listener(settings.port) {
            let problem = format!(
                "proxy port {} is used by {}",
                settings.port,
                ports::describe_owner(settings.port)
            );
            if fix {
                match cli::move_off_busy_port(settings, false) {
                    Ok(s) => report.issue(
                        problem,
                        format!(
                            "moved Porch to port {} (saved); it starts with your next `porch run`",
                            s.port
                        ),
                    ),
                    Err(e) => report.issue(problem, format!("{e:#}")),
                }
            } else {
                report.issue(problem, report.hint());
            }
        }
    }

    // Routes: dead ones are pruned automatically; stopped/orphaned need consent.
    let (removed, rows) = crate::health::reconcile()?;
    for name in removed {
        report.issue(
            format!("route {name}: its process exited"),
            "removed the stale route",
        );
    }
    let mut stopped = Vec::new();
    let mut orphaned = Vec::new();
    for row in &rows {
        let name = &row.route.name;
        match row.health {
            Health::Ok => {}
            Health::NotListening => report.issue(
                format!("route {name}: nothing is listening on {}", row.route.target),
                "make sure the app uses the PORT env var and check its logs; then run `porch list`",
            ),
            Health::Stopped => stopped.push((
                name.clone(),
                row.stopped_for.map(|d| d.as_secs()).unwrap_or(0),
            )),
            Health::Dead { orphaned: true } => orphaned.push(name.clone()),
            Health::Dead { orphaned: false } => {}
        }
    }
    for (name, secs) in stopped {
        let problem =
            format!("route {name}: its processes are stopped (frozen by job control) for {secs}s");
        if fix
            && (yes
                || crate::setup::confirm(
                    &format!("Terminate the stopped launch {name} and remove its route?"),
                    false,
                    interactive,
                ))
        {
            match cli::terminate_routes(std::slice::from_ref(&name)).await {
                Ok(()) => report.issue(problem, "terminated the launch and removed its route"),
                Err(e) => report.issue(
                    problem,
                    format!("could not terminate ({e:#}); run `porch doctor --fix --yes` again"),
                ),
            }
        } else {
            report.issue(problem, report.hint());
        }
    }
    if !orphaned.is_empty() {
        let problem = format!(
            "{}: launcher exited but app processes remain",
            orphaned.join(", ")
        );
        if fix
            && (yes
                || crate::setup::confirm(
                    "Terminate those orphaned app processes and remove their routes?",
                    false,
                    interactive,
                ))
        {
            cli::prune(true).await?;
            report.issue(problem, "terminated them and removed their routes");
        } else {
            report.issue(problem, report.hint());
        }
    }

    if report.count == 0 {
        println!("\nNo problems found.");
    }
    Ok(())
}
