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

    pub fn interrupt_group(group: u32) -> bool {
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) != 0 }
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
#[cfg(windows)]
pub fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
