//! Build the owned workload Job before native process creation. JOB_LIST makes
//! membership atomic with CreateProcess, including host death before Resume.
use super::{check, own};
use crate::ResourceLimits;
use std::{
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, OwnedHandle},
};
use windows_sys::Win32::System::JobObjects::*;

pub(super) fn create(resources: ResourceLimits) -> io::Result<OwnedHandle> {
    let memory =
        usize::try_from(resources.memory_max_bytes.unwrap_or(512 * 1024 * 1024)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "memory limit exceeds Windows Job range",
            )
        })?;
    let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_JOB_MEMORY,
            ActiveProcessLimit: resources.pids_max.unwrap_or(16),
            ..Default::default()
        },
        JobMemoryLimit: memory,
        ..Default::default()
    };
    allocate(limits)
}

#[cfg(feature = "async-io")]
pub(super) fn create_host() -> io::Result<OwnedHandle> {
    // Trusted host mode retains the caller's full authority and has no implicit
    // scratch memory/PID defaults. The Job only owns the process tree lifetime.
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    allocate(limits)
}

fn allocate(limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> io::Result<OwnedHandle> {
    // SAFETY: no attributes/name; return checked before adopting ownership.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh successful job handle, uniquely owned.
    let job = unsafe { own(raw) };
    // SAFETY: pointer and size identify a fully initialized limits structure.
    check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    })?;
    Ok(job)
}
