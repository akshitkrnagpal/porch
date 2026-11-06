//! Acceptance tests must attach Porch to a controlling terminal: ordinary piped
//! subprocess tests cannot detect SIGTTIN/SIGTTOU stops in interactive children.
#![cfg(unix)]

#[test]
fn terminal_children_and_shutdown() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/terminal_acceptance.py"
        ))
        .arg(env!("CARGO_BIN_EXE_porch"))
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
