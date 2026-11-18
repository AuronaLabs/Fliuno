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
    configure(
        Some(resources.memory_max_bytes.unwrap_or(512 * 1024 * 1024)),
        Some(resources.pids_max.unwrap_or(16)),
    )
}

#[cfg(feature = "async-io")]
pub(super) fn create_host(resources: ResourceLimits) -> io::Result<OwnedHandle> {
    // A trusted process keeps OS-user authority. Only host-specified limits apply.
    configure(resources.memory_max_bytes, resources.pids_max)
}

fn configure(memory: Option<u64>, processes: Option<u32>) -> io::Result<OwnedHandle> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if let Some(memory) = memory {
        limits.JobMemoryLimit = usize::try_from(memory).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "memory limit exceeds Windows Job range",
            )
        })?;
        limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
    }
    if let Some(processes) = processes {
        limits.BasicLimitInformation.ActiveProcessLimit = processes;
        limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    }
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
