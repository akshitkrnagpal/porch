//! Lifetime ownership for a launched process and its descendants.
//!
//! Call `configure` before spawning and construct a `TreeGuard` immediately after
//! spawning. Keep the guard until child output and its exit status are handled.
use anyhow::{Context, Result};
use tokio::process::{Child, Command};

/// Stop child processes from inheriting this process's own stdin, stdout and
/// stderr by accident. Call once at startup.
///
/// Windows hands every inheritable handle to every child, not only the three
/// it is given. A daemon started with its output in a log file would still
/// hold the caller's stdout pipe open, so a script reading `porch` output
/// never sees it end. Children meant to share the terminal are unaffected:
/// `Stdio::inherit` gives each one its own inheritable duplicate.
pub fn keep_std_handles_private() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT},
            System::Console::{
                GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
            },
        };
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = unsafe { GetStdHandle(id) };
            // Null or invalid means no such stream; nothing to protect.
            if !handle.is_null() && handle as isize != -1 {
                unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
            }
        }
    }
}

/// Put each application in an independent process tree before it starts.
///
/// On Windows the child starts suspended: `TreeGuard::new` puts it in its job
/// and then lets it run, so nothing it starts can be outside the job. Always
/// create the guard right after spawning (a child dropped before that is
/// killed, `kill_on_drop`).
pub fn configure(command: &mut Command) {
    command.kill_on_drop(true);
    #[cfg(unix)]
    // A new process group inside our controlling session is a background group:
    // reads and terminal-mode changes can stop it with SIGTTIN/SIGTTOU. A new
    // session keeps inherited terminal descriptors usable without job-control
    // stops, matching Node's detached launch semantics. setsid also makes the
    // child its process-group leader, so TreeGuard and forwarded signals still
    // address the complete application group.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(windows)]
    command.creation_flags(
        windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP
            | windows_sys::Win32::System::Threading::CREATE_SUSPENDED,
    );
}

/// Let a child created suspended (see `configure`) run: resume its threads.
#[cfg(windows)]
fn resume(pid: u32) -> std::io::Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };
    let mut resumed = 0;
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    if ResumeThread(thread) != u32::MAX {
                        resumed += 1;
                    }
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    if resumed == 0 {
        return Err(std::io::Error::other(format!(
            "could not resume process {pid}"
        )));
    }
    Ok(())
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

    /// Stop what is left of the tree: ask the group to exit (SIGTERM), give
    /// it up to `grace` to finish, then drop the guard (SIGKILL whatever
    /// remains). A server behind a package-manager wrapper gets time to clean
    /// up after the wrapper exits.
    ///
    /// Call it while the launched command is still unreaped (see `wait_exit`).
    pub async fn terminate(self, grace: std::time::Duration) {
        #[cfg(unix)]
        if self.group > 0 && unreaped(self.group) {
            unsafe { libc::kill(-self.group, libc::SIGTERM) };
            let group = self.group as u32;
            let deadline = tokio::time::Instant::now() + grace;
            let mut delay = std::time::Duration::from_millis(10);
            while tokio::time::Instant::now() < deadline
                && group_members(group).is_some_and(|m| !m.is_empty())
            {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_millis(200));
            }
        }
        #[cfg(windows)]
        let _ = grace;
        drop(self);
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
            // Only now may it run (it was created suspended): everything it
            // starts is in the job from its first instruction.
            resume(child.id().context("child has already been reaped")?)
                .context("starting child process")?;
            Ok(guard)
        }
    }
}

/// Whether `pid` is a child of this process that has not been reaped yet
/// (running, or exited and waiting to be reaped).
#[cfg(unix)]
fn unreaped(pid: libc::pid_t) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return true;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
}

/// Wait for a child to exit without reaping it, and return its status.
///
/// A process group's id is the leader's pid. Until the leader is reaped, even
/// as a zombie, that id cannot name another process or group, so signaling
/// the group reaches only the leader's own tree. Once it is reaped, the id
/// can be reused at once (a machine whose pid counter has wrapped reuses
/// freed pids within milliseconds), and the same signal would reach an
/// unrelated process group. Callers clean up the group after this returns
/// and reap the child last, with `Child::wait`.
pub async fn wait_exit(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let Some(pid) = child.id() else {
            // Already reaped: its status is cached.
            return child.wait().await;
        };
        let raw = tokio::task::spawn_blocking(move || -> std::io::Result<i32> {
            let pid = pid as libc::pid_t;
            let info = loop {
                if let Some(info) = exited(pid)? {
                    break info;
                }
                block_until_exit(pid)?;
            };
            #[cfg(target_os = "linux")]
            let status = unsafe { info.si_status() };
            #[cfg(not(target_os = "linux"))]
            let status = info.si_status;
            // Encode it like a wait status: exit code in the second byte,
            // terminating signal in the low bits.
            Ok(match info.si_code {
                libc::CLD_EXITED => (status & 0xff) << 8,
                libc::CLD_DUMPED => status | 0x80,
                _ => status,
            })
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok(std::process::ExitStatus::from_raw(raw))
    }
    #[cfg(windows)]
    {
        // Job objects, not reusable ids, own the tree on Windows.
        child.wait().await
    }
}

/// The exit report of child `pid` if it has exited (not merely stopped),
/// leaving it unreaped.
#[cfg(unix)]
fn exited(pid: libc::pid_t) -> std::io::Result<Option<libc::siginfo_t>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
    #[cfg(target_os = "linux")]
    let reported = unsafe { info.si_pid() } == pid;
    #[cfg(not(target_os = "linux"))]
    let reported = info.si_pid == pid;
    // macOS also reports a stopped child here, despite asking only for exits.
    let exit = matches!(
        info.si_code,
        libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
    );
    Ok((reported && exit).then_some(info))
}

/// Block until child `pid` exits (or something changes that may be an exit).
#[cfg(target_os = "linux")]
fn block_until_exit(pid: libc::pid_t) -> std::io::Result<()> {
    // Linux reports only exits for WEXITED.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    let error = std::io::Error::last_os_error();
    if result == 0 || error.raw_os_error() == Some(libc::EINTR) {
        Ok(())
    } else {
        Err(error)
    }
}

/// Block until child `pid` exits. kqueue's exit note, unlike waitid, ignores
/// a child that is only stopped.
#[cfg(all(unix, not(target_os = "linux")))]
fn block_until_exit(pid: libc::pid_t) -> std::io::Result<()> {
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::kevent(queue, &change, 1, &mut event, 1, std::ptr::null()) };
    let error = std::io::Error::last_os_error();
    unsafe { libc::close(queue) };
    // ESRCH: it exited before the note was registered; the caller re-checks.
    if result >= 0 || matches!(error.raw_os_error(), Some(libc::EINTR | libc::ESRCH)) {
        Ok(())
    } else {
        Err(error)
    }
}

impl Drop for TreeGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // A negative PID targets the application process group, including
            // descendants that outlive their original parent. Only while the
            // leader is unreaped does the id still name that group (see
            // `wait_exit`); after an early reap it may name someone else's.
            if self.group > 0 && unreaped(self.group) {
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

/// Live (not zombie) members of process group `group`.
///
/// Linux reads `/proc` and macOS asks the kernel for the group's members, so
/// polling this in a wait loop never spawns a process. `None` when the
/// process table can't be read.
#[cfg(unix)]
pub fn group_members(group: u32) -> Option<Vec<u32>> {
    if group == 0 || group > i32::MAX as u32 {
        return Some(Vec::new());
    }
    #[cfg(target_os = "linux")]
    {
        let mut members = Vec::new();
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let Some(fields) = linux_stat(pid) else {
                continue;
            };
            // Fields after the command: state, ppid, pgrp, session, ...
            if !matches!(fields.first().map(String::as_str), Some("Z" | "X"))
                && fields.get(2).and_then(|g| g.parse::<u32>().ok()) == Some(group)
            {
                members.push(pid);
            }
        }
        Some(members)
    }
    #[cfg(target_os = "macos")]
    {
        const PROC_PGRP_ONLY: u32 = 2;
        let size = std::mem::size_of::<libc::pid_t>();
        let mut pids: Vec<libc::pid_t> = vec![0; 64];
        loop {
            let bytes = (pids.len() * size) as libc::c_int;
            let n = unsafe {
                libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), bytes)
            };
            if n < 0 {
                return None;
            }
            let count = n as usize / size;
            if count < pids.len() {
                pids.truncate(count);
                break;
            }
            // A full buffer may have cut the list short.
            pids.resize(pids.len() * 2, 0);
        }
        Some(
            pids.into_iter()
                .filter(|p| *p > 0)
                .map(|p| p as u32)
                // `identity` is None for zombies and processes that just exited.
                .filter(|p| crate::state::identity(*p).is_some())
                .collect(),
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let out = std::process::Command::new("ps")
            .args(["-axo", "pid=,pgid=,stat="])
            .output()
            .ok()?;
        Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| {
                    let mut p = line.split_whitespace();
                    let pid = p.next()?.parse().ok()?;
                    let pgid: u32 = p.next()?.parse().ok()?;
                    let status = p.next()?;
                    (pgid == group && !status.starts_with('Z')).then_some(pid)
                })
                .collect(),
        )
    }
}

/// The fields of `/proc/PID/stat` after the command name.
#[cfg(target_os = "linux")]
fn linux_stat(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(
        stat.rsplit_once(") ")?
            .1
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    )
}

/// The session `pid` belongs to.
#[cfg(unix)]
pub fn session(pid: u32) -> Option<u32> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let sid = unsafe { libc::getsid(pid as libc::pid_t) };
    (sid > 0).then_some(sid as u32)
}

/// The parent of `pid`.
#[cfg(unix)]
fn parent(pid: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        linux_stat(pid)?.get(1)?.parse().ok()
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        (n == size).then(|| unsafe { info.assume_init() }.pbi_ppid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

/// A start time recorded by [`crate::state::identity`] as a comparable key:
/// seconds and microseconds on macOS, clock ticks since boot on Linux, a
/// FILETIME on Windows. `None` for formats that don't compare (`ps lstart`).
pub fn start_key(started: &str) -> Option<(u64, u64)> {
    match started.split_once(':') {
        Some((secs, micros)) => Some((secs.parse().ok()?, micros.parse().ok()?)),
        None => Some((started.parse().ok()?, 0)),
    }
}

/// Whether a process that started at `started` may have started during the
/// current boot. `None` when the platform can't tell.
///
/// A route recorded before a reboot names process ids that now belong to
/// whatever happens to run with them.
pub fn started_this_boot(started: &str) -> Option<bool> {
    let (value, _) = start_key(started)?;
    #[cfg(target_os = "macos")]
    {
        let mut boot = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut size = std::mem::size_of::<libc::timeval>();
        let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut boot as *mut libc::timeval).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } == 0;
        // Seconds since the epoch; allow for the boot time's own rounding.
        (ok && boot.tv_sec > 0).then(|| value + 1 >= boot.tv_sec as u64)
    }
    #[cfg(target_os = "linux")]
    {
        // Clock ticks since boot: a start later than the current uptime
        // belongs to an earlier boot.
        let uptime: f64 = std::fs::read_to_string("/proc/uptime")
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        (hz > 0).then_some(value as f64 <= (uptime + 1.0) * hz as f64)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = value;
        None
    }
}

/// How a launcher may take on the listener that answers on its app port
/// after the launched command exited successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adoption {
    /// The listener is still in the launched command's own process group,
    /// which the route keeps owning.
    Group,
    /// A server the launched command started outside its group (a daemon
    /// that called `setsid`). Only that process is recorded and signalled,
    /// never the group it is in.
    Process,
}

/// Whether the launched command `child` (started at `child_started`) started
/// `listener`.
///
/// A command can also exit 0 after asking a server that was already running
/// to listen on the port (`docker run -p` with Docker Desktop, `pm2 start`
/// with its daemon up). That server isn't the app's and is never adopted:
/// stopping the route would stop it, and everything else it runs.
pub fn adoption(listener: u32, child: u32, child_started: Option<&str>) -> Option<Adoption> {
    if listener == 0 || listener == child {
        return None;
    }
    #[cfg(unix)]
    {
        let pgid = unsafe { libc::getpgid(listener as libc::pid_t) };
        if pgid > 0 && pgid as u32 == child {
            return Some(Adoption::Group);
        }
        // The launched command's session: it called setsid, so its session
        // id is its pid.
        if session(listener) == Some(child) {
            return Some(Adoption::Process);
        }
        // Still below the launched command in the process tree.
        let mut pid = listener;
        for _ in 0..64 {
            match parent(pid) {
                Some(p) if p == child => return Some(Adoption::Process),
                Some(p) if p > 1 && p != pid => pid = p,
                _ => break,
            }
        }
    }
    // A daemon that detached and was reparented: it can only be the app's if
    // it started after the launched command did.
    let listener_started = crate::state::identity(listener)?.started;
    let (Some(listener_started), Some(child_started)) = (
        start_key(&listener_started),
        child_started.and_then(start_key),
    ) else {
        return None;
    };
    (listener_started >= child_started).then_some(Adoption::Process)
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

    fn scratch(tag: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "porch-process-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn terminate_gives_survivors_time_to_clean_up_before_sigkill() {
        // The leader (a package-manager wrapper) exits at once; the server it
        // started needs a moment after SIGTERM to clean up.
        let dir = scratch("grace");
        let done = dir.join("cleaned");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "(trap 'sleep 0.3; echo ok > \"$1\"; exit 0' TERM; echo ready; while :; do sleep 0.05; done) & read _",
                "sh",
            ])
            .arg(&done)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().unwrap();
        let guard = TreeGuard::new(&child).unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        drop(child.stdin.take());
        wait_exit(&mut child).await.unwrap();
        guard.terminate(Duration::from_secs(5)).await;
        child.wait().await.unwrap();
        let cleaned = done.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            cleaned,
            "the survivor was killed before it finished cleaning up"
        );
    }

    #[test]
    fn start_times_from_an_earlier_boot_never_match() {
        let me = crate::state::identity(std::process::id()).unwrap();
        assert!(start_key(&me.started).is_some());
        assert_ne!(started_this_boot(&me.started), Some(false));
        // Seconds since 1970 on macOS; on Linux, far more ticks than this
        // machine has been up.
        let ancient = if cfg!(target_os = "macos") {
            "1:0"
        } else {
            "18446744073709551"
        };
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert_eq!(started_this_boot(ancient), Some(false));
        }
    }

    /// An orphaned group (leader gone, members alive) is a route's only when
    /// its members are in the session the launch created.
    #[tokio::test]
    async fn orphaned_group_matches_only_inside_its_own_session() {
        for own_session in [true, false] {
            let mut command = Command::new("sh");
            command
                .args(["-c", "sleep 30 & echo $!; read _"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            if own_session {
                configure(&mut command);
            } else {
                // A new group inside the test's session, as a shell's job
                // control would create, not the session Porch's apps get.
                command.process_group(0);
            }
            let mut child = command.spawn().unwrap();
            let leader = child.id().unwrap();
            let started = crate::state::identity(leader).unwrap().started;
            let mut line = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .await
                .unwrap();
            let member: u32 = line.trim().parse().unwrap();
            drop(child.stdin.take());
            child.wait().await.unwrap();
            let matched = crate::state::group_matches(leader, Some(&started));
            let other_start = crate::state::group_matches(leader, Some("1:0"));
            unsafe { libc::kill(member as i32, libc::SIGKILL) };
            assert_eq!(matched, own_session, "own session: {own_session}");
            assert!(
                !other_start,
                "matched a leader start time from another boot"
            );
        }
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
        // Python picks its own port and reports it only once it is listening.
        // Reserving a port, freeing it and probing it with connect() was racy:
        // under parallel tests the OS reassigned the freed port, so the probe
        // reached (and disturbed) another test's listener while Python's bind
        // failed or came later, and the lookup saw no Python listener.
        let mut command = Command::new("python3");
        command
            .args([
                "-c",
                "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); print(s.getsockname()[1],flush=True); time.sleep(60)",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let guard = TreeGuard::new(&child).unwrap();
        let mut line = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(30),
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut line),
        )
        .await;
        let Ok(port) = line.trim().parse::<u16>() else {
            drop(guard);
            let output = child.wait_with_output().await.unwrap();
            panic!(
                "Python TCP listener did not report its port: {read:?} {line:?}; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
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
