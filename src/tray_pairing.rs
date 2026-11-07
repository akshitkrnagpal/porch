//! Pairing state shown in the menu. Pure: updates come from the worker, the
//! menu is a function of this state, and both are tested without a display.
use super::{Action, Entry};
use crate::discovery::types::{
    Candidate, JoinFailure, Machine, PairShown, PairState, PairStatusReport, Source,
};
use std::time::{Duration, Instant};

/// "Paired with X" stays in the menu this long.
pub const SUCCESS_LINGER: Duration = Duration::from_secs(60);

/// The machine showing a code (A).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum HostView {
    #[default]
    Idle,
    /// `code` is `None` after a tray restart: the agent keeps only a hash.
    Showing {
        id: String,
        code: Option<String>,
        until: String,
        address: Option<String>,
    },
    Accepted {
        id: String,
        peer: Machine,
        overdue: bool,
    },
    Paired {
        peer: String,
        at: Instant,
    },
    Ended {
        message: String,
    },
}
/// The machine entering a code (B).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum JoinView {
    #[default]
    Idle,
    Joining {
        target: Option<String>,
    },
    Failed {
        failure: JoinFailure,
        attempt: JoinAttempt,
    },
    Paired {
        peer: String,
        at: Instant,
    },
}
/// What the user asked to join, so Try again can reopen it prefilled.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct JoinAttempt {
    pub code: String,
    /// `--host` value (address or name), when targeted.
    pub host: Option<String>,
    /// Display name of the target, when known.
    pub name: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingUpdate {
    Shown(PairShown),
    /// The tray restarted while a code it showed is still waiting.
    Restored {
        id: String,
        expires: u64,
    },
    ShowFailed(String),
    Status(PairStatusReport),
    /// Accepted for a minute without the other machine connecting.
    Overdue(String),
    Joining(JoinAttempt),
    Joined(Machine),
    JoinFailed(JoinFailure, JoinAttempt),
    JoinCancelled,
    DismissHost,
    DismissJoin,
    /// No dialog could be shown; offer the terminal command instead.
    DialogUnavailable,
}
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PairingView {
    pub host: HostView,
    pub join: JoinView,
    pub dialog_unavailable: bool,
}
impl PairingView {
    /// The code currently shown or awaiting confirmation, if any.
    pub fn active_id(&self) -> Option<&str> {
        match &self.host {
            HostView::Showing { id, .. } | HostView::Accepted { id, .. } => Some(id),
            _ => None,
        }
    }
    pub fn apply(&mut self, update: PairingUpdate, now: Instant) {
        match update {
            PairingUpdate::Shown(shown) => {
                self.host = HostView::Showing {
                    until: clock(shown.expires),
                    address: host_port(&shown.endpoint),
                    code: Some(shown.code),
                    id: shown.id,
                }
            }
            PairingUpdate::Restored { id, expires } => {
                if self.active_id().is_none() {
                    self.host = HostView::Showing {
                        id,
                        code: None,
                        until: clock(expires),
                        address: None,
                    }
                }
            }
            PairingUpdate::ShowFailed(message) => self.host = HostView::Ended { message },
            PairingUpdate::Status(report) => {
                if self.active_id() != Some(report.id.as_str()) {
                    return;
                }
                let peer = report.peer.clone().unwrap_or(Machine {
                    id: String::new(),
                    name: "the other machine".into(),
                });
                match report.state {
                    PairState::Waiting => {}
                    PairState::Accepted => {
                        let overdue = matches!(self.host, HostView::Accepted { overdue: true, .. });
                        self.host = HostView::Accepted {
                            id: report.id,
                            peer,
                            overdue,
                        }
                    }
                    PairState::Paired => {
                        self.host = HostView::Paired {
                            peer: peer.name,
                            at: now,
                        }
                    }
                    state => {
                        self.host = HostView::Ended {
                            message: ended_message(state, report.reason.as_deref()),
                        }
                    }
                }
            }
            PairingUpdate::Overdue(id) => {
                if let HostView::Accepted {
                    id: current,
                    overdue,
                    ..
                } = &mut self.host
                {
                    if *current == id {
                        *overdue = true;
                    }
                }
            }
            PairingUpdate::Joining(attempt) => {
                self.join = JoinView::Joining {
                    target: attempt.name.or(attempt.host),
                }
            }
            PairingUpdate::Joined(machine) => {
                self.join = JoinView::Paired {
                    peer: machine.name,
                    at: now,
                }
            }
            PairingUpdate::JoinFailed(failure, attempt) => {
                self.join = JoinView::Failed { failure, attempt }
            }
            PairingUpdate::JoinCancelled | PairingUpdate::DismissJoin => self.join = JoinView::Idle,
            PairingUpdate::DismissHost => self.host = HostView::Idle,
            PairingUpdate::DialogUnavailable => self.dialog_unavailable = true,
        }
    }
    /// Drop success lines once they have been visible long enough.
    pub fn expire(&mut self, now: Instant) -> bool {
        let old = |at: &Instant| now.duration_since(*at) >= SUCCESS_LINGER;
        let mut changed = false;
        if matches!(&self.host, HostView::Paired { at, .. } if old(at)) {
            self.host = HostView::Idle;
            changed = true;
        }
        if matches!(&self.join, JoinView::Paired { at, .. } if old(at)) {
            self.join = JoinView::Idle;
            changed = true;
        }
        changed
    }
}
fn ended_message(state: PairState, reason: Option<&str>) -> String {
    match (state, reason) {
        (PairState::Expired, _) => "Code expired".into(),
        (PairState::Cancelled, Some("too_many_failures")) => {
            "Code cancelled after 5 wrong attempts — someone on this network may be guessing".into()
        }
        (PairState::Cancelled, Some("host_full")) => {
            "Couldn't save the other machine. Remove an unused machine, then show a new code".into()
        }
        (PairState::Cancelled, _) => "Code cancelled".into(),
        (PairState::Replaced, _) => "Another pairing code was started".into(),
        _ => "Pairing ended".into(),
    }
}
/// `https://192.168.1.20:7463/` → `192.168.1.20:7463` (IPv6 keeps brackets).
pub fn host_port(endpoint: &str) -> Option<String> {
    let url = url::Url::parse(endpoint).ok()?;
    let host = match url.host()? {
        url::Host::Ipv6(ip) => format!("[{ip}]"),
        host => host.to_string(),
    };
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}
/// Local wall-clock time of a Unix timestamp, e.g. `14:32`.
pub fn clock(epoch: u64) -> String {
    #[cfg(unix)]
    {
        let time = epoch as libc::time_t;
        // SAFETY: localtime_r writes only into the zeroed struct we own.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            if !libc::localtime_r(&time, &mut tm).is_null() {
                return format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
            }
        }
    }
    let _ = epoch;
    "in 5 minutes".into()
}
/// Accept `718977`, `718 977` or `718-977`; keep leading zeros.
pub fn parse_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    (code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())).then_some(code)
}
fn spaced(code: &str) -> String {
    format!("{} {}", &code[..3], &code[3..])
}
/// Menu lines for whatever pairing is in progress; empty when idle.
pub fn status_entries(view: &PairingView) -> Vec<Entry> {
    let mut out = Vec::new();
    match &view.host {
        HostView::Idle => {}
        HostView::Showing {
            id,
            code: Some(code),
            until,
            address,
        } => {
            out.push(Entry::text(format!(
                "Pairing code {} · until {until}",
                spaced(code)
            )));
            out.push(Entry::text(
                "Other Mac: Porch ▸ Pair a device ▸ Enter a code",
            ));
            out.push(Entry::text(format!("Terminal: porch pair {code}")));
            out.push(Entry::text("Paired machines can open all your dev apps"));
            out.push(Entry::action(
                "Copy code",
                Action::CopyText {
                    text: code.clone(),
                    what: "Code".into(),
                },
                true,
            ));
            let command = match address {
                Some(address) => format!("porch pair {code} --host {address}"),
                None => format!("porch pair {code}"),
            };
            out.push(Entry::action(
                "Copy terminal command",
                Action::CopyText {
                    text: command,
                    what: "Command".into(),
                },
                true,
            ));
            out.push(Entry::action(
                "Cancel pairing",
                Action::CancelCode { id: id.clone() },
                true,
            ));
        }
        HostView::Showing { id, code: None, .. } => {
            out.push(Entry::text("A pairing code is active (shown earlier)"));
            out.push(Entry::action(
                "Cancel and show a new code",
                Action::ShowCode {
                    replace: Some(id.clone()),
                },
                true,
            ));
            out.push(Entry::action(
                "Cancel pairing",
                Action::CancelCode { id: id.clone() },
                true,
            ));
        }
        HostView::Accepted {
            peer,
            overdue: false,
            ..
        } => out.push(Entry::text(format!(
            "Code accepted by {} · confirming…",
            peer.name
        ))),
        HostView::Accepted {
            peer,
            overdue: true,
            ..
        } => {
            out.push(Entry::text(format!(
                "{} accepted the code but hasn't connected yet",
                peer.name
            )));
            if !peer.id.is_empty() {
                out.push(Entry::action(
                    format!("Unpair {}…", peer.name),
                    Action::Unpair {
                        peer: peer.id.clone(),
                        name: peer.name.clone(),
                    },
                    true,
                ));
            }
            out.push(Entry::action("Dismiss", Action::DismissHost, true));
        }
        HostView::Paired { peer, .. } => out.push(Entry::text(format!("Paired with {peer}"))),
        HostView::Ended { message } => {
            out.push(Entry::text(message));
            out.push(Entry::action(
                "Show a new code",
                Action::ShowCode { replace: None },
                true,
            ));
            out.push(Entry::action("Dismiss", Action::DismissHost, true));
        }
    }
    match &view.join {
        JoinView::Idle => {}
        JoinView::Joining { target } => {
            out.push(Entry::text(match target {
                Some(name) => format!("Pairing with {name}…"),
                None => "Finding the machine…".into(),
            }));
            out.push(Entry::action("Cancel", Action::CancelJoin, true));
        }
        JoinView::Paired { peer, .. } => out.push(Entry::text(format!("Paired with {peer}"))),
        JoinView::Failed { failure, attempt } => {
            // One sentence per line keeps the menu narrow.
            for sentence in failure.message.split_inclusive(". ") {
                out.push(Entry::text(sentence.trim()));
            }
            for candidate in failure.candidates.iter().take(10) {
                out.push(Entry::action(
                    format!(
                        "Enter code for {} · {}…",
                        candidate.name,
                        host_port(&candidate.endpoint).unwrap_or_default()
                    ),
                    Action::EnterCode {
                        host: host_port(&candidate.endpoint),
                        name: Some(candidate.name.clone()),
                        prefill: attempt.code.clone(),
                    },
                    true,
                ));
            }
            if failure.code == "no_host" {
                out.push(Entry::action(
                    "Enter a code with an address…",
                    Action::EnterCodeWithAddress {
                        prefill: attempt.code.clone(),
                    },
                    true,
                ));
            } else if failure.retry && failure.code != "multiple_hosts" {
                out.push(Entry::action(
                    "Try again…",
                    Action::EnterCode {
                        host: attempt.host.clone(),
                        name: attempt.name.clone(),
                        prefill: attempt.code.clone(),
                    },
                    true,
                ));
            }
            out.push(Entry::action("Dismiss", Action::DismissJoin, true));
        }
    }
    if view.dialog_unavailable {
        out.push(Entry::text("Run: porch pair CODE"));
    }
    out
}
/// The "Pair a device" submenu.
pub fn pair_menu(view: &PairingView) -> Entry {
    let showing = view.active_id().is_some();
    let joining = matches!(view.join, JoinView::Joining { .. });
    Entry::group(
        "Pair a device",
        vec![
            Entry::action(
                "Show a pairing code",
                Action::ShowCode { replace: None },
                !showing,
            ),
            Entry::action(
                "Enter a code…",
                Action::EnterCode {
                    host: None,
                    name: None,
                    prefill: String::new(),
                },
                !joining,
            ),
            Entry::action(
                "Enter a code with an address…",
                Action::EnterCodeWithAddress {
                    prefill: String::new(),
                },
                !joining,
            ),
        ],
    )
}
fn source_label(source: &Source) -> &'static str {
    match source {
        Source::Local => "local",
        Source::Lan => "LAN",
        Source::Tailscale => "Tailscale",
        Source::Explicit => "address",
    }
}
/// "Found nearby (unverified)": mDNS and tailnet names are hints, so each
/// entry only targets the code at that address.
pub fn nearby_menu(candidates: &[Candidate], joining: bool) -> Option<Entry> {
    if candidates.is_empty() {
        return None;
    }
    let found = candidates
        .iter()
        .take(30)
        .map(|candidate| {
            let address = host_port(&candidate.endpoint);
            let ip = address
                .as_deref()
                .map(|a| a.rsplit_once(':').map_or(a, |(h, _)| h))
                .unwrap_or_default()
                .to_owned();
            Entry::group(
                format!(
                    "{} · {ip} ({})",
                    candidate.name,
                    source_label(&candidate.source)
                ),
                vec![Entry::action(
                    "Enter code for this machine…",
                    Action::EnterCode {
                        host: address,
                        name: Some(candidate.name.clone()),
                        prefill: String::new(),
                    },
                    !joining,
                )],
            )
        })
        .collect();
    Some(Entry::group("Found nearby (unverified)", found))
}
/// `ab12…ef` for menus; the full value is copied.
pub fn short_fingerprint(pin: &str) -> String {
    if pin.len() < 8 || !pin.is_ascii() {
        return pin.into();
    }
    format!("{}…{}", &pin[..4], &pin[pin.len() - 2..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::types::PROTOCOL_VERSION;
    fn shown(id: &str) -> PairShown {
        PairShown {
            version: PROTOCOL_VERSION,
            id: id.into(),
            code: "071897".into(),
            expires: 1_900_000_000,
            expires_in: 300,
            machine: Machine {
                id: "a".into(),
                name: "A".into(),
            },
            endpoint: "https://192.168.1.20:7463/".into(),
        }
    }
    fn report(id: &str, state: PairState, reason: Option<&str>) -> PairStatusReport {
        PairStatusReport {
            version: PROTOCOL_VERSION,
            id: id.into(),
            state,
            reason: reason.map(Into::into),
            expires: None,
            expires_in: None,
            peer: Some(Machine {
                id: "mini-id".into(),
                name: "Akshits-Mac-mini".into(),
            }),
        }
    }
    fn labels(view: &PairingView) -> Vec<String> {
        status_entries(view).into_iter().map(|e| e.label).collect()
    }
    #[test]
    fn showing_a_code_lists_short_lines_and_copy_actions() {
        let mut view = PairingView::default();
        view.apply(PairingUpdate::Shown(shown("one")), Instant::now());
        let entries = status_entries(&view);
        assert!(entries[0]
            .label
            .starts_with("Pairing code 071 897 · until "));
        assert!(entries.iter().all(|e| e.label.chars().count() <= 50));
        assert!(entries
            .iter()
            .any(|e| e.label == "Terminal: porch pair 071897"));
        let copy: Vec<_> = entries
            .iter()
            .filter_map(|e| match &e.action {
                Some(Action::CopyText { text, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            copy,
            vec!["071897", "porch pair 071897 --host 192.168.1.20:7463"]
        );
        assert_eq!(
            entries.last().unwrap().action,
            Some(Action::CancelCode { id: "one".into() })
        );
        // Show code is disabled while one is active.
        assert!(!pair_menu(&view).children[0].enabled);
    }
    #[test]
    fn host_status_moves_through_accepted_paired_and_clears() {
        let start = Instant::now();
        let mut view = PairingView::default();
        view.apply(PairingUpdate::Shown(shown("one")), start);
        // Reports for another code are ignored.
        view.apply(
            PairingUpdate::Status(report("old", PairState::Expired, None)),
            start,
        );
        assert!(matches!(view.host, HostView::Showing { .. }));
        view.apply(
            PairingUpdate::Status(report("one", PairState::Accepted, None)),
            start,
        );
        assert_eq!(
            labels(&view),
            vec!["Code accepted by Akshits-Mac-mini · confirming…"]
        );
        view.apply(PairingUpdate::Overdue("one".into()), start);
        let entries = status_entries(&view);
        assert!(entries[0].label.contains("hasn't connected yet"));
        assert_eq!(
            entries[1].action,
            Some(Action::Unpair {
                peer: "mini-id".into(),
                name: "Akshits-Mac-mini".into()
            })
        );
        view.apply(
            PairingUpdate::Status(report("one", PairState::Paired, None)),
            start,
        );
        assert_eq!(labels(&view), vec!["Paired with Akshits-Mac-mini"]);
        assert!(!view.expire(start + Duration::from_secs(5)));
        assert!(view.expire(start + SUCCESS_LINGER));
        assert!(labels(&view).is_empty());
    }
    #[test]
    fn ended_codes_explain_and_offer_a_new_one() {
        for (state, reason, text) in [
            (PairState::Expired, None, "Code expired"),
            (PairState::Cancelled, Some("user"), "Code cancelled"),
            (
                PairState::Cancelled,
                Some("too_many_failures"),
                "Code cancelled after 5 wrong attempts — someone on this network may be guessing",
            ),
            (
                PairState::Replaced,
                None,
                "Another pairing code was started",
            ),
        ] {
            let mut view = PairingView::default();
            view.apply(PairingUpdate::Shown(shown("one")), Instant::now());
            view.apply(
                PairingUpdate::Status(report("one", state, reason)),
                Instant::now(),
            );
            let entries = status_entries(&view);
            assert_eq!(entries[0].label, text);
            assert_eq!(entries[1].action, Some(Action::ShowCode { replace: None }));
            assert_eq!(entries[2].action, Some(Action::DismissHost));
            assert!(pair_menu(&view).children[0].enabled);
        }
    }
    #[test]
    fn a_restarted_tray_never_invents_the_code() {
        let mut view = PairingView::default();
        view.apply(
            PairingUpdate::Restored {
                id: "one".into(),
                expires: 1_900_000_000,
            },
            Instant::now(),
        );
        let entries = status_entries(&view);
        assert_eq!(entries[0].label, "A pairing code is active (shown earlier)");
        assert_eq!(
            entries[1].action,
            Some(Action::ShowCode {
                replace: Some("one".into())
            })
        );
        assert!(!entries.iter().any(|e| e.label.contains("porch pair")));
    }
    #[test]
    fn join_failures_offer_the_right_next_step() {
        let attempt = JoinAttempt {
            code: "012345".into(),
            host: Some("192.168.1.20:7463".into()),
            name: Some("mini".into()),
        };
        let mut view = PairingView::default();
        view.apply(PairingUpdate::Joining(attempt.clone()), Instant::now());
        assert_eq!(labels(&view), vec!["Pairing with mini…", "Cancel"]);
        assert!(!pair_menu(&view).children[1].enabled);
        let failed = |code: &str| {
            let mut failure = crate::discovery::pairing::failure(code);
            if code == "multiple_hosts" {
                failure.candidates = vec![crate::discovery::types::JoinCandidate {
                    name: "rogue".into(),
                    endpoint: "https://192.168.1.66:7463/".into(),
                    source: Source::Lan,
                }];
            }
            let mut view = PairingView::default();
            view.apply(
                PairingUpdate::JoinFailed(failure, attempt.clone()),
                Instant::now(),
            );
            status_entries(&view)
        };
        let wrong = failed("wrong_code");
        assert_eq!(wrong[0].label, "That code didn't work.");
        assert_eq!(wrong[1].label, "Check the code on the other machine.");
        assert_eq!(
            wrong[2].action,
            Some(Action::EnterCode {
                host: attempt.host.clone(),
                name: attempt.name.clone(),
                prefill: "012345".into()
            })
        );
        let mismatch = failed("certificate_mismatch");
        assert!(
            !mismatch
                .iter()
                .any(|e| matches!(e.action, Some(Action::EnterCode { .. }))),
            "no bypass after a certificate mismatch"
        );
        let none = failed("no_host");
        assert!(none.iter().any(|e| e.action
            == Some(Action::EnterCodeWithAddress {
                prefill: "012345".into()
            })));
        let many = failed("multiple_hosts");
        assert!(many.iter().any(|e| e.action
            == Some(Action::EnterCode {
                host: Some("192.168.1.66:7463".into()),
                name: Some("rogue".into()),
                prefill: "012345".into()
            })));
        assert_eq!(many.last().unwrap().action, Some(Action::DismissJoin));
    }
    #[test]
    fn codes_parse_like_the_cli_and_nearby_targets_the_address() {
        assert_eq!(parse_code(" 012 345 ").as_deref(), Some("012345"));
        assert_eq!(parse_code("012-345").as_deref(), Some("012345"));
        assert_eq!(parse_code("12345"), None);
        assert_eq!(parse_code("12345a"), None);
        assert_eq!(
            host_port("https://[fe80::1]:7463/").as_deref(),
            Some("[fe80::1]:7463")
        );
        let nearby = nearby_menu(
            &[Candidate {
                machine_id: None,
                name: "Akshits-Mac-mini".into(),
                endpoint: "https://192.168.1.20:7463/".into(),
                source: Source::Lan,
                last_seen: 0,
                node: None,
            }],
            false,
        )
        .unwrap();
        assert_eq!(nearby.label, "Found nearby (unverified)");
        let machine = &nearby.children[0];
        assert_eq!(machine.label, "Akshits-Mac-mini · 192.168.1.20 (LAN)");
        assert_eq!(
            machine.children[0].action,
            Some(Action::EnterCode {
                host: Some("192.168.1.20:7463".into()),
                name: Some("Akshits-Mac-mini".into()),
                prefill: String::new()
            })
        );
        assert_eq!(short_fingerprint(&"ab12".repeat(16)), "ab12…12");
    }
}
