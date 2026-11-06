//! Lifetime ownership for a launched process and its descendants.
//!
//! Call `configure` before spawning and construct a `TreeGuard` immediately after
//! spawning. Keep the guard until child output and its exit status are handled.
use anyhow::{Context, Result};
use tokio::process::{Child, Command};

/// Put each application in an independent process tree before it starts.
pub fn configure(command: &mut Command) {
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
}

/// Dropping this guard terminates remaining processes in the owned tree.
#[derive(Debug)]
pub struct TreeGuard {
    #[cfg(unix)]
    group: libc::pid_t,
    // Windows HANDLE is a pointer but kernel handles may be passed between
    // threads. Store its integer representation so this owner remains Send.
    #[cfg(windows)]
    job: usize,
}

impl TreeGuard {
    /// Give a successfully daemonized server independent lifetime ownership.
    /// The caller must first validate the listener's process identity and route
    /// transfer. Failure to change Windows limits keeps the original guard active.
    pub fn release(mut self) -> Result<()> {
        #[cfg(unix)]
        {
            self.group = 0;
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::{
                JobObjectExtendedLimitInformation, SetInformationJobObject,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            };
            let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            let handle = self.job as *mut std::ffi::c_void;
            if unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error()).context("releasing child process job");
            }
            if unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("closing released child process job");
            }
            self.job = 0;
        }
        Ok(())
    }

    pub fn new(child: &Child) -> Result<Self> {
        #[cfg(unix)]
        {
            let group = child.id().context("child has already been reaped")?;
            let group = libc::pid_t::try_from(group).context("invalid child process group")?;
            anyhow::ensure!(group > 0, "invalid child process group");
            Ok(Self { group })
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            let process = child
                .raw_handle()
                .context("child has already been reaped")?;
            // An unnamed, non-inheritable handle belongs solely to this guard.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(std::io::Error::last_os_error()).context("creating child process job");
            }
            let guard = Self { job: job as usize };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: limits is initialized, its size matches the indicated
            // information class, and the job handle stays live in guard.
            let configured = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            };
            if configured == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("configuring child process job");
            }
            if unsafe { AssignProcessToJobObject(job, process.cast()) } == 0 {
                return Err(std::io::Error::last_os_error()).context("assigning child process job");
            }
            Ok(guard)
        }
    }
}

impl Drop for TreeGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // A negative PID targets the application process group, including
            // descendants that outlive their original parent.
            if self.group > 0 {
                unsafe { libc::kill(-self.group, libc::SIGKILL) };
            }
        }
        #[cfg(windows)]
        {
            // Closing the only job handle activates KILL_ON_JOB_CLOSE even
            // when the immediate child has already exited.
            if self.job != 0 {
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(self.job as *mut std::ffi::c_void)
                };
            }
        }
    }
}

/// Resolve Windows package-manager batch shims explicitly so Rust's process
/// launcher applies its native batch-file quoting. Unix uses ordinary PATH lookup.
pub fn resolve_program(program: &str, paths: &std::ffi::OsStr) -> std::ffi::OsString {
    #[cfg(windows)]
    {
        let program_path = std::path::Path::new(program);
        if program_path.extension().is_some() {
            return program.into();
        }
        let explicit = program_path.is_absolute() || program_path.components().count() > 1;
        if explicit {
            for extension in ["exe", "cmd", "bat"] {
                let candidate = program_path.with_extension(extension);
                if candidate.is_file() {
                    return candidate.into_os_string();
                }
            }
        } else {
            for directory in std::env::split_paths(paths) {
                for extension in ["exe", "cmd", "bat"] {
                    let candidate = directory.join(program_path).with_extension(extension);
                    if candidate.is_file() {
                        return candidate.into_os_string();
                    }
                }
            }
        }
        program.into()
    }
    #[cfg(not(windows))]
    {
        let _ = paths;
        program.into()
    }
}

/// Find a TCP listener belonging to the state directory's user. The caller must
/// capture and validate a native process identity before transferring routes.
pub fn listening_process(port: u16) -> Option<u32> {
    if port == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        linux_listener(port).or_else(|| lsof_listener(port))
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        lsof_listener(port)
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("netstat.exe")
            .args(["-ano", "-p", "TCP"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        netstat_listeners(&String::from_utf8_lossy(&output.stdout), port)
            .into_iter()
            .find(|pid| windows_state_owner(*pid))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(unix)]
fn state_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(crate::state::dir())
        .map(|m| m.uid())
        .unwrap_or_else(|_| {
            std::env::var("SUDO_UID")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| unsafe { libc::geteuid() })
        })
}

#[cfg(unix)]
fn lsof_listener(port: u16) -> Option<u32> {
    let uid = state_uid();
    let output = std::process::Command::new("lsof")
        .args([
            "-nP",
            "-a",
            &format!("-u{uid}"),
            &format!("-iTCP:{port}"),
            "-sTCP:LISTEN",
            "-Fpu",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    lsof_owner(&String::from_utf8_lossy(&output.stdout), uid)
}

#[cfg(unix)]
fn lsof_owner(output: &str, uid: u32) -> Option<u32> {
    let mut pid = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse::<u32>().ok().filter(|p| *p > 0);
        } else if line.strip_prefix('u').and_then(|v| v.parse::<u32>().ok()) == Some(uid)
            && pid.is_some()
        {
            return pid;
        }
    }
    None
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn linux_tcp_inodes(table: &str, port: u16, uid: u32) -> std::collections::BTreeSet<String> {
    table
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 10 || fields[3] != "0A" || fields[7].parse::<u32>().ok() != Some(uid)
            {
                return None;
            }
            let address_port = u16::from_str_radix(fields[1].rsplit_once(':')?.1, 16).ok()?;
            (address_port == port).then(|| fields[9].to_owned())
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn linux_listener(port: u16) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let uid = state_uid();
    let mut inodes = std::collections::BTreeSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(raw) = std::fs::read_to_string(table) {
            inodes.extend(linux_tcp_inodes(&raw, port, uid));
        }
    }
    if inodes.is_empty() {
        return None;
    }
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|p| *p > 0)
        else {
            continue;
        };
        if std::fs::metadata(entry.path())
            .ok()
            .is_none_or(|m| m.uid() != uid)
        {
            continue;
        }
        let Ok(descriptors) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in descriptors.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let name = link.to_string_lossy();
            if name
                .strip_prefix("socket:[")
                .and_then(|s| s.strip_suffix(']'))
                .is_some_and(|inode| inodes.contains(inode))
            {
                return Some(pid);
            }
        }
    }
    None
}

#[cfg(any(windows, test))]
fn netstat_listeners(output: &str, port: u16) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() != 5
                || !fields[0].eq_ignore_ascii_case("TCP")
                || !fields[3].eq_ignore_ascii_case("LISTENING")
            {
                return None;
            }
            let local_port = fields[1].rsplit_once(':')?.1.parse::<u16>().ok()?;
            if local_port != port {
                return None;
            }
            fields[4].parse::<u32>().ok().filter(|pid| *pid > 0)
        })
        .collect()
}

#[cfg(windows)]
fn windows_state_owner(pid: u32) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{
            EqualSid, GetFileSecurityW, GetSecurityDescriptorOwner, GetTokenInformation, TokenUser,
            OWNER_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER,
        },
        System::Threading::{OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let state = crate::state::dir()
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut required = 0u32;
    unsafe {
        GetFileSecurityW(
            state.as_ptr(),
            OWNER_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut required,
        );
    }
    if required == 0 {
        return false;
    }
    // usize storage aligns SECURITY_DESCRIPTOR and TOKEN_USER on both targets.
    let mut descriptor = vec![0usize; (required as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetFileSecurityW(
            state.as_ptr(),
            OWNER_SECURITY_INFORMATION,
            descriptor.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return false;
    }
    let mut owner = std::ptr::null_mut();
    let mut defaulted = 0;
    if unsafe {
        GetSecurityDescriptorOwner(descriptor.as_mut_ptr().cast(), &mut owner, &mut defaulted)
    } == 0
        || owner.is_null()
    {
        return false;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let process = Handle(process);
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = Handle(token);
    required = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return false;
    }
    let mut user = vec![0usize; (required as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            user.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return false;
    }
    let token_user = unsafe { &*user.as_ptr().cast::<TOKEN_USER>() };
    unsafe { EqualSid(owner, token_user.User.Sid) != 0 }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn dropping_guard_terminates_parent_and_grandchild() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 60 & echo $!; wait"])
            .stdout(Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().unwrap();
        let guard = TreeGuard::new(&child).unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();
        assert_eq!(unsafe { libc::getpgid(grandchild as i32) }, guard.group);
        drop(guard);
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let output = Command::new("ps")
                    .args(["-p", &grandchild.to_string(), "-o", "stat="])
                    .output()
                    .await
                    .unwrap();
                let state = String::from_utf8_lossy(&output.stdout);
                if !output.status.success() || state.trim().starts_with('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn guard_is_send_and_handles_an_already_exited_leader() {
        fn assert_send<T: Send>() {}
        assert_send::<TreeGuard>();
        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);
        configure(&mut command);
        let mut child = command.spawn().unwrap();
        let _guard = TreeGuard::new(&child).unwrap();
        assert_eq!(child.wait().await.unwrap().code(), Some(7));
    }

    #[tokio::test]
    async fn release_preserves_process_until_explicit_cleanup() {
        let mut command = Command::new("sleep");
        command.arg("60");
        configure(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        TreeGuard::new(&child).unwrap().release().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn discovers_python_tcp_listener_for_state_user() {
        let reserve = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reserve.local_addr().unwrap().port();
        drop(reserve);
        let mut command = Command::new("python3");
        command
            .args([
                "-c",
                "import socket,sys,time; s=socket.socket(); s.bind(('127.0.0.1',int(sys.argv[1]))); s.listen(); time.sleep(60)",
                &port.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let guard = TreeGuard::new(&child).unwrap();
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    return Err(format!("Python exited: {status}"));
                }
                if tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_ok()
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if !matches!(ready, Ok(Ok(()))) {
            drop(guard);
            let output = child.wait_with_output().await.unwrap();
            panic!(
                "Python TCP listener failed to bind: {ready:?}; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(listening_process(port), Some(pid));
        drop(guard);
        child.wait().await.unwrap();
    }

    #[test]
    fn listener_tables_reject_other_users_ports_and_connection_states() {
        assert_eq!(lsof_owner("p41\nu501\np42\nu502\n", 502), Some(42));
        assert_eq!(lsof_owner("p41\nu501\n", 502), None);
        let table="  sl local_address rem_address st tx_queue tr retrnsmt uid timeout inode\n 0: 0100007F:1234 00000000:0000 0A 00000000:00000000 00:00000000 00000000 501 0 12345\n 1: 0100007F:1234 00000000:0000 01 00000000:00000000 00:00000000 00000000 501 0 12346\n 2: 0100007F:1234 00000000:0000 0A 00000000:00000000 00:00000000 00000000 502 0 12347\n";
        assert_eq!(
            linux_tcp_inodes(table, 0x1234, 501),
            std::collections::BTreeSet::from(["12345".into()])
        );
        assert!(linux_tcp_inodes(table, 0x1235, 501).is_empty());
        assert_eq!(netstat_listeners("TCP 127.0.0.1:4660 0.0.0.0:0 LISTENING 41\nTCP [::1]:4660 [::]:0 LISTENING 42\nTCP 127.0.0.1:4661 0.0.0.0:0 LISTENING 43\nTCP 127.0.0.1:4660 0.0.0.0:0 ESTABLISHED 44\n",4660),vec![41,42]);
    }
}
