//! Every process the client starts (`llama-server`, and `llama-server
//! --list-devices` when checking a runtime) is tied to the client's
//! lifetime: `PR_SET_PDEATHSIG` on Linux, a kill-on-close job object on
//! Windows, where it also gets no console window. Works on
//! `std::process::Command`, so tokio's goes through `as_std_mut`.

#[cfg(target_os = "linux")]
mod platform {
    use std::os::unix::process::CommandExt;

    /// Linux kills the child when the thread that spawned it exits. For
    /// llama-server that's a tokio worker thread (they live as long as the
    /// runtime, i.e. the client); a device check waits for its child on the
    /// thread that spawned it, so it can't outlive it either.
    pub fn before_spawn(cmd: &mut std::process::Command) {
        // SAFETY: getpid is async-signal-safe and runs before the fork.
        let parent = unsafe { libc::getpid() };
        // SAFETY: the closure runs in the forked child before exec and only
        // calls async-signal-safe functions (prctl, getppid, _exit).
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent died before prctl took effect.
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }
}

#[cfg(windows)]
mod platform {
    use log::warn;
    use std::os::windows::process::CommandExt;
    use std::sync::OnceLock;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// No console window flashing up from the tray app.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn before_spawn(cmd: &mut std::process::Command) {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    /// One job object for the life of the client, never closed: Windows
    /// closes it when the client exits, however it exits, and that kills
    /// every process in it.
    fn job() -> Option<HANDLE> {
        static JOB: OnceLock<Option<usize>> = OnceLock::new();
        let raw = JOB.get_or_init(|| {
            // SAFETY: plain Win32 calls; `info` outlives the call that reads it.
            unsafe {
                let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .ok()?;
                Some(job.0 as usize)
            }
        });
        raw.map(|h| HANDLE(h as *mut core::ffi::c_void))
    }

    pub fn contain(process: Option<std::os::windows::io::RawHandle>) {
        let (Some(job), Some(process)) = (job(), process) else {
            warn!(target: "llama", "couldn't put llama-server in a job object; it may outlive a crashed client");
            return;
        };
        // SAFETY: both handles are valid; the child's stays open while it lives.
        if let Err(e) = unsafe { AssignProcessToJobObject(job, HANDLE(process)) } {
            warn!(target: "llama", "couldn't put llama-server in a job object: {e}");
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    pub fn before_spawn(_cmd: &mut std::process::Command) {}
}

pub use platform::before_spawn;

/// After spawning with tokio.
pub fn after_spawn_tokio(child: &tokio::process::Child) {
    #[cfg(windows)]
    platform::contain(child.raw_handle());
    #[cfg(not(windows))]
    let _ = child;
}

/// After spawning with std.
pub fn after_spawn_std(child: &std::process::Child) {
    #[cfg(windows)]
    platform::contain(Some(std::os::windows::io::AsRawHandle::as_raw_handle(child)));
    #[cfg(not(windows))]
    let _ = child;
}
