//! Exercise the installed CLI layout without opening a real desktop tray.
use std::{
    fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[test]
fn tray_launches_sibling_and_preserves_cli_and_state_paths() {
    let suffix = std::env::consts::EXE_SUFFIX;
    let current = std::env::current_exe().unwrap();
    if current.file_name().unwrap() == format!("porch-tray{suffix}").as_str() {
        let record = serde_json::json!({
            "cli": std::env::var("PORCH_BIN").unwrap(),
            "state": std::env::var("PORCH_STATE_DIR").unwrap(),
        });
        fs::write(
            std::env::var_os("PORCH_TRAY_TEST_RECORD").unwrap(),
            record.to_string(),
        )
        .unwrap();
        return;
    }

    let temp = std::env::temp_dir().join(format!(
        "porch-tray-launch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&temp).unwrap();
    let cli = temp.join(format!("porch{suffix}"));
    let tray = temp.join(format!("porch-tray{suffix}"));
    let state = temp.join("state");
    let record = temp.join("launched.json");
    fs::copy(env!("CARGO_BIN_EXE_porch"), &cli).unwrap();
    // The child test binary acts as the tray and records the launch environment.
    fs::copy(current, tray).unwrap();
    let output = Command::new(&cli)
        .arg("tray")
        .env("PORCH_STATE_DIR", &state)
        .env("PORCH_TRAY_TEST_RECORD", &record)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let launched = loop {
        if let Ok(bytes) = fs::read(&record) {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                break value;
            }
        }
        assert!(Instant::now() < deadline, "tray did not record its launch");
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(launched["cli"], cli.to_string_lossy().as_ref());
    assert_eq!(launched["state"], state.to_string_lossy().as_ref());
    // The detached child may still be exiting on Windows; cleanup is best effort.
    let _ = fs::remove_dir_all(temp);
}
