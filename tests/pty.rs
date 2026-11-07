//! Acceptance tests must attach Porch to a controlling terminal: ordinary piped
//! subprocess tests cannot detect SIGTTIN/SIGTTOU stops in interactive children.
#![cfg(unix)]
mod common;

/// Fixtures `terminal_acceptance.py` starts (two cases, two signals, Turbo),
/// each with its own proxy port slot.
const FIXTURES: usize = 5;

#[test]
fn terminal_children_and_shutdown() {
    let slots: Vec<String> = (0..FIXTURES)
        .map(|_| common::free_port().to_string())
        .collect();
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/terminal_acceptance.py"
        ))
        .arg(env!("CARGO_BIN_EXE_porch"))
        .env("PORCH_TEST_SLOTS", slots.join(","))
        .output()
        .expect("PTY acceptance requires Python 3");
    assert!(
        output.status.success(),
        "PTY acceptance failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprint!("{}", String::from_utf8_lossy(&output.stdout));
}
