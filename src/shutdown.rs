//! Graceful stop requests between processes.
//!
//! Unix uses SIGTERM for this. Windows has no equivalent that reaches a
//! windowless console process from another console: `taskkill` without `/F`
//! only closes windows. So every stoppable Porch process (proxy, discovery
//! agent, `porch run` launcher) creates a named event, and a stop request sets
//! it. The process then shuts down through its normal cleanup path.
//!
//! Children Porch launches are not Porch processes. They run in their own
//! console process group, so the launcher, which shares their console, can
//! send them Ctrl+Break.

#[cfg(windows)]
mod imp {
    use std::sync::Mutex;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT},
            Threading::{
                CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
                INFINITE,
            },
        },
    };

    /// `Local\` is the caller's logon session. A proxy started as SYSTEM by
    /// Task Scheduler lives elsewhere and is stopped by the forced fallback,
    /// which needs elevation there anyway.
    fn name(pid: u32) -> Vec<u16> {
        format!("Local\\porch-stop-{pid}\0")
            .encode_utf16()
            .collect()
    }

    /// The handle stays open for the life of the process: a waiter may be
    /// blocked on it at any time, and closing it under the waiter is undefined.
    static EVENT: Mutex<Option<usize>> = Mutex::new(None);

    fn event() -> std::io::Result<usize> {
        let mut slot = EVENT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = *slot {
            return Ok(h);
        }
        let name = name(std::process::id());
        // Manual reset: once set, every current and later wait returns.
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        *slot = Some(handle as usize);
        Ok(handle as usize)
    }

    pub fn listen() -> std::io::Result<()> {
        event().map(|_| ())
    }

    pub async fn requested() {
        let Ok(handle) = event() else {
            return std::future::pending().await;
        };
        let waited = tokio::task::spawn_blocking(move || unsafe {
            WaitForSingleObject(handle as HANDLE, INFINITE)
        })
        .await;
        if waited.is_err() {
            std::future::pending::<()>().await;
        }
    }

    pub fn request(pid: u32) -> bool {
        let name = name(pid);
        unsafe {
            let handle = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
            if handle.is_null() {
                return false;
            }
            let ok = SetEvent(handle) != 0;
            CloseHandle(handle);
            ok
        }
    }

    /// `group` must be a process the caller started with
    /// CREATE_NEW_PROCESS_GROUP (callers guarantee that). The event then
    /// reaches only that group's members. Measured on Windows Server 2025: an
    /// id whose group has no live member makes the call fail and reaches no
    /// one, and the id of a live process that leads no group reaches no one
    /// either; only group 0 reaches the whole console, so it is never passed.
    /// A leader exiting between the check below and the call is therefore
    /// harmless. The check skips a pointless call, and the handle held across
    /// it keeps the id from naming a new process meanwhile.
    pub fn kill_tree(root: u32) {
        use windows_sys::Win32::{
            Foundation::{FILETIME, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
            System::{
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                    TH32CS_SNAPPROCESS,
                },
                Threading::{
                    GetProcessTimes, OpenProcess, TerminateProcess,
                    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
                },
            },
        };
        /// A handle to `pid` and its creation time.
        fn open(pid: u32) -> Option<(HANDLE, u64)> {
            let handle = unsafe {
                OpenProcess(
                    PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    pid,
                )
            };
            if handle.is_null() {
                return None;
            }
            let (mut created, mut exited, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
                == 0
            {
                unsafe { CloseHandle(handle) };
                return None;
            }
            let created = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
            Some((handle, created))
        }
        /// (pid, recorded parent pid) of every process right now.
        fn snapshot() -> Vec<(u32, u32)> {
            let mut table = Vec::new();
            unsafe {
                let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snapshot != INVALID_HANDLE_VALUE {
                    let mut entry = PROCESSENTRY32W {
                        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                        ..Default::default()
                    };
                    let mut more = Process32FirstW(snapshot, &mut entry) != 0;
                    while more {
                        table.push((entry.th32ProcessID, entry.th32ParentProcessID));
                        more = Process32NextW(snapshot, &mut entry) != 0;
                    }
                    CloseHandle(snapshot);
                }
            }
            table
        }
        if root == 0 {
            return;
        }
        let Some((handle, created)) = open(root) else {
            return;
        };
        // Every member found so far: id, handle (which keeps the id from being
        // reused), creation time, and whether it is known to have exited.
        let mut members = vec![(root, handle, created, false)];
        // Rounds end when every member has exited and a snapshot taken after
        // that finds no new one: an exited process cannot start another. The
        // cap only guards against a member that will not exit.
        for _ in 0..64 {
            for &(_, handle, _, exited) in &members {
                if !exited {
                    unsafe { TerminateProcess(handle, 1) };
                }
            }
            for member in members.iter_mut().filter(|m| !m.3) {
                // A watchdog only: termination completes unless the process
                // is stuck in the kernel; it is retried next round.
                member.3 = unsafe { WaitForSingleObject(member.1, 10_000) } == WAIT_OBJECT_0;
            }
            // Children any member started before it exited, including after
            // the previous snapshot. A child must be created after the parent
            // it names; an older process naming a member's id lost its real
            // parent long ago.
            let found = members.len();
            let table = snapshot();
            let mut next = 0;
            while next < members.len() {
                let (parent, _, parent_created, _) = members[next];
                next += 1;
                for &(pid, recorded_parent) in &table {
                    if recorded_parent != parent
                        || pid == parent
                        || pid == 0
                        || members.iter().any(|m| m.0 == pid)
                    {
                        continue;
                    }
                    let Some((handle, created)) = open(pid) else {
                        continue;
                    };
                    if created < parent_created {
                        unsafe { CloseHandle(handle) };
                        continue;
                    }
                    members.push((pid, handle, created, false));
                }
            }
            if members.len() == found && members.iter().all(|m| m.3) {
                break;
            }
        }
        for (_, handle, _, exited) in members {
            unsafe {
                if !exited {
                    TerminateProcess(handle, 1);
                }
                CloseHandle(handle);
            }
        }
    }

    pub fn interrupt_group(group: u32) -> bool {
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;
        if group == 0 {
            return false;
        }
        unsafe {
            let leader = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, group);
            if leader.is_null() {
                return false;
            }
            let mut code = 0;
            let running = GetExitCodeProcess(leader, &mut code) != 0 && code == STILL_ACTIVE;
            let sent = running && GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) != 0;
            CloseHandle(leader);
            sent
        }
    }
}

/// Start accepting stop requests. Call early, before the process records
/// itself as running, so a stopper never finds it unreachable.
pub fn listen() -> std::io::Result<()> {
    #[cfg(windows)]
    return imp::listen();
    #[cfg(not(windows))]
    Ok(())
}

/// Resolves when another process asked this one to stop. Never resolves on
/// Unix, where SIGTERM carries the same request.
pub async fn requested() {
    #[cfg(windows)]
    imp::requested().await;
    #[cfg(not(windows))]
    std::future::pending::<()>().await;
}

/// Ask a Porch process to stop cleanly. False when it is not listening (not
/// a Porch process, already gone, or in another logon session).
#[cfg(windows)]
pub fn request(pid: u32) -> bool {
    imp::request(pid)
}

/// Send Ctrl+Break to a console process group. Only works when the caller
/// shares the group's console, which is the case for a launcher and its child.
#[cfg(windows)]
pub fn interrupt_group(group: u32) -> bool {
    imp::interrupt_group(group)
}

/// End a process tree without asking.
///
/// Not `taskkill /T`: Windows never updates a process's recorded parent id
/// when the parent exits, so `/T` also kills unrelated processes whose dead
/// parent's id now belongs to the target. If one of those is the console
/// host of the user's terminal (or of a CI job), the whole console closes.
/// Here a child belongs to the tree only if it was created after the process
/// it names as parent, which a reused id's "children" never are. Members are
/// ended in rounds until a snapshot taken after all of them exited finds no
/// new child, and held handles keep their ids from being reused meanwhile.
///
/// A descendant whose parent already exited before the first snapshot has no
/// remaining link to the tree, so no process walk can find it (`taskkill /T`
/// cannot either). Complete containment needs a job object created before
/// the children start, which is how Porch runs apps: each launch's tree is in
/// its launcher's job (`process::TreeGuard`, kill-on-close), and forced stops
/// also end the owning launcher, closing that job. This function is the
/// fallback for processes outside such a job.
#[cfg(windows)]
pub fn kill_tree(pid: u32) {
    imp::kill_tree(pid)
}

#[cfg(all(test, windows))]
mod tests {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        },
    };

    fn children_of(parent: u32) -> Vec<u32> {
        let mut found = Vec::new();
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            assert_ne!(snapshot, INVALID_HANDLE_VALUE);
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut more = Process32FirstW(snapshot, &mut entry) != 0;
            while more {
                if entry.th32ParentProcessID == parent {
                    found.push(entry.th32ProcessID);
                }
                more = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
        found
    }

    #[test]
    fn kill_tree_ends_the_process_and_its_children() {
        let mut shell = std::process::Command::new("cmd")
            .args(["/c", "ping -n 120 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let child = loop {
            if let Some(&pid) = children_of(shell.id()).first() {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cmd never started ping"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // Held before the kill, so the id cannot name another process later.
        let watch = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, child) };
        assert!(!watch.is_null());
        super::kill_tree(shell.id());
        shell.wait().unwrap();
        assert_eq!(unsafe { WaitForSingleObject(watch, 60_000) }, WAIT_OBJECT_0);
        unsafe { CloseHandle(watch) };
    }
}
