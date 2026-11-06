use std::{
    path::PathBuf,
    process::{Command, Output},
};
struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "porch-cli-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_porch"))
            .args(args)
            .env("PORCH_STATE_DIR", &self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn alias_round_trip_and_replacement_are_persistent() {
    let s = Sandbox::new();
    for target in ["http://127.0.0.1:43121", "http://127.0.0.1:43122"] {
        assert!(s.run(&["alias", "demo", target]).status.success());
        let o = s.run(&["get", "demo"]);
        assert!(o.status.success());
        assert_eq!(String::from_utf8(o.stdout).unwrap().trim(), target);
    }
    let o = s.run(&["list"]);
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["routes"].as_array().unwrap().len(), 1);
    assert!(s.run(&["remove", "demo"]).status.success());
    assert!(!s.run(&["get", "demo"]).status.success());
}
#[test]
fn rejected_alias_preserves_existing_state() {
    let s = Sandbox::new();
    assert!(s
        .run(&["alias", "valid", "http://127.0.0.1:43121"])
        .status
        .success());
    assert!(!s
        .run(&["alias", "bad/name", "http://127.0.0.1:43122"])
        .status
        .success());
    assert!(!s
        .run(&["alias", "valid", "invalid target"])
        .status
        .success());
    assert_eq!(
        String::from_utf8(s.run(&["get", "valid"]).stdout)
            .unwrap()
            .trim(),
        "http://127.0.0.1:43121"
    );
}
#[test]
fn concurrent_writers_preserve_every_route() {
    let s = Sandbox::new();
    let mut children = Vec::new();
    for i in 0..24 {
        children.push(
            Command::new(env!("CARGO_BIN_EXE_porch"))
                .args(["alias", &format!("route{i}"), "http://127.0.0.1:1234"])
                .env("PORCH_STATE_DIR", &s.0)
                .spawn()
                .unwrap(),
        );
    }
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let v: serde_json::Value = serde_json::from_slice(&s.run(&["list"]).stdout).unwrap();
    assert_eq!(v["routes"].as_array().unwrap().len(), 24);
}
