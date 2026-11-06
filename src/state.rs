use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub routes: Vec<Route>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub name: String,
    pub target: String,
    pub pid: Option<u32>,
}
pub fn dir() -> PathBuf {
    std::env::var_os("PORCH_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".porch")
        })
}
fn prepare() -> Result<()> {
    let d = dir();
    if let Ok(m) = fs::symlink_metadata(&d) {
        if m.file_type().is_symlink() {
            bail!("state directory cannot be a symlink")
        }
    }
    fs::create_dir_all(&d)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn load() -> Result<State> {
    prepare()?;
    match fs::read(dir().join("state.json")) {
        Ok(b) => Ok(serde_json::from_slice(&b).context("invalid route state")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
    }
}
pub fn mutate(f: impl FnOnce(&mut State)) -> Result<()> {
    prepare()?;
    let mut o = OpenOptions::new();
    o.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = o.open(dir().join("state.lock"))?;
    lock.lock_exclusive()?;
    let mut s = load()?;
    f(&mut s);
    let p = dir().join(format!("state.{}.tmp", std::process::id()));
    let mut file = o.create_new(true).open(&p)?;
    file.write_all(&serde_json::to_vec_pretty(&s)?)?;
    file.sync_all()?;
    fs::rename(p, dir().join("state.json"))?;
    std::fs::File::open(dir())?.sync_all()?;
    Ok(())
}
pub fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
