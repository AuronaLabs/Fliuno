//! One owned Job and root process; shared by trusted and isolated launchers.
use super::check;
use std::{
    io,
    os::windows::{
        io::{AsRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    process::ExitStatus,
};
use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        JobObjects::TerminateJobObject,
        Threading::{GetExitCodeProcess, WaitForSingleObject},
    },
};

pub(crate) struct JobProcess {
    pub(super) process: OwnedHandle,
    pub(super) job: OwnedHandle,
    pub(super) pid: u32,
}
impl JobProcess {
    pub(crate) fn id(&self) -> u32 {
        self.pid
    }
    pub(crate) fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        // SAFETY: process is an owned live observation handle.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: valid DWORD output and live process handle.
                check(unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) })?;
                Ok(Some(ExitStatus::from_raw(code)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub(crate) fn kill(&self) -> io::Result<()> {
        // SAFETY: the owned job contains only this launch's workload tree.
        check(unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) })?;
        // SAFETY: bounded wait on the still-owned root process handle.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 5000) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "owned process did not exit after kill",
            )),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
impl Drop for JobProcess {
    fn drop(&mut self) {
        if self.kill().is_err() {
            eprintln!("fliuno: owned process cleanup failed");
        }
    }
}
