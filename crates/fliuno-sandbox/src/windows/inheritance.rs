//! Keep inheritable handles out of the multithreaded embedding host entirely.
//!
//! HANDLE_LIST restricts what our child inherits, but cannot stop another
//! library's concurrent CreateProcess(TRUE) from taking our temporary handles.
//! Duplicate them into a suspended surrogate and select that process as the
//! parent. No surrogate user code ever runs. Atomic JOB_LIST assignment and
//! kill-on-close cover host crash; silent breakaway keeps the real child out
//! of this temporary job (enclosing non-breakaway jobs still apply).
use super::{Attributes, check, command_line, own, path_wide, wide};
use std::{
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, OwnedHandle},
};
use windows_sys::Win32::{
    Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0},
    System::{JobObjects::*, Threading::*},
};

pub(super) struct Inheritance {
    process: OwnedHandle,
    job: OwnedHandle,
    parent: [HANDLE; 1],
    handles: [HANDLE; 3],
}
impl Inheritance {
    pub(super) fn new(sources: [HANDLE; 3]) -> io::Result<Self> {
        // SAFETY: unnamed job with a non-inheritable returned handle.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful creation, unique ownership.
        let job = unsafe { own(raw) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        // SAFETY: live job and correctly sized initialized limits.
        check(unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        })?;
        let executable = std::env::current_exe()?;
        let program = path_wide(&executable)?;
        let mut command = wide(&command_line(
            executable.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "surrogate path must be UTF-8")
            })?,
            &[],
        ))?;
        let jobs = [job.as_raw_handle()];
        let mut attributes = Attributes::new(1)?;
        attributes.set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                ..Default::default()
            },
            lpAttributeList: attributes.ptr(),
        };
        let mut info = PROCESS_INFORMATION::default();
        let environment = [0u16; 2];
        // SAFETY: live buffers and attribute storage. No handles are inherited;
        // the initial thread never resumes. JOB_LIST closes the crash window
        // between creation and a subsequent AssignProcessToJobObject call.
        check(unsafe {
            CreateProcessW(
                program.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_SUSPENDED
                    | CREATE_NO_WINDOW
                    | CREATE_UNICODE_ENVIRONMENT
                    | EXTENDED_STARTUPINFO_PRESENT,
                environment.as_ptr().cast(),
                std::ptr::null(),
                &startup.StartupInfo,
                &mut info,
            )
        })?;
        // SAFETY: successful creation returned distinct owned handles.
        let process = unsafe { own(info.hProcess) };
        // SAFETY: unique initial-thread handle; closing it never resumes it.
        let _thread = unsafe { own(info.hThread) };
        let mut owner = Self {
            parent: [process.as_raw_handle()],
            process,
            job,
            handles: [std::ptr::null_mut(); 3],
        };
        for (source, destination) in sources.into_iter().zip(&mut owner.handles) {
            // SAFETY: sources are borrowed live pipe handles. The returned
            // handle belongs to the surrogate, so never CloseHandle it here;
            // terminating that process closes its entire private handle table.
            check(unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    source,
                    owner.process.as_raw_handle(),
                    destination,
                    0,
                    1,
                    DUPLICATE_SAME_ACCESS,
                )
            })?;
        }
        Ok(owner)
    }
    pub(super) fn handles(&self) -> [HANDLE; 3] {
        self.handles
    }
    pub(super) fn set_attributes(&self, attributes: &mut Attributes) -> io::Result<()> {
        // Keep these backing fields alive through CreateProcessW. HANDLE_LIST
        // refers to values in the selected parent's handle table, not ours.
        attributes.set(PROC_THREAD_ATTRIBUTE_PARENT_PROCESS, &self.parent)?;
        attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &self.handles)
    }
    pub(super) fn verify_child(&self, child: &OwnedHandle) -> io::Result<()> {
        let mut inside = 0;
        // SAFETY: both handles remain live; inside is a writable BOOL.
        check(unsafe {
            IsProcessInJob(child.as_raw_handle(), self.job.as_raw_handle(), &mut inside)
        })?;
        if inside != 0 {
            return Err(io::Error::other(
                "child unexpectedly retained temporary inheritance job",
            ));
        }
        Ok(())
    }
}
impl Drop for Inheritance {
    fn drop(&mut self) {
        // SAFETY: this job owns only the never-resumed surrogate. Real children
        // are verified outside it before their entry point is allowed to run.
        let stopped = unsafe { TerminateJobObject(self.job.as_raw_handle(), 0) };
        // SAFETY: bounded wait on our own still-owned process observation handle.
        let exited = unsafe { WaitForSingleObject(self.process.as_raw_handle(), 5000) };
        if stopped == 0 || exited != WAIT_OBJECT_0 {
            eprintln!("fliuno: inheritance surrogate cleanup failed");
        }
        // Closing the job is a final kill-on-close safeguard on every path.
    }
}

#[cfg(test)]
mod tests;
