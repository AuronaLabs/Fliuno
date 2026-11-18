//! Dedicated supervisor boundaries. No user-controlled security descriptors.
use super::{check, own, path_wide};
use std::{io, mem::size_of, os::windows::io::AsRawHandle, path::Path};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::CreateDirectoryW,
    System::{JobObjects::*, Threading::GetCurrentProcess},
};

pub(crate) struct Detached {
    process: std::os::windows::io::OwnedHandle,
}
impl Detached {
    pub(crate) fn wait(&mut self) -> io::Result<()> {
        // SAFETY: uniquely owned live process handle. This method is used only
        // by the dedicated reaper thread, never by the app RPC event loop.
        let result = unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(
                self.process.as_raw_handle(),
                u32::MAX,
            )
        };
        if result == windows_sys::Win32::Foundation::WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

pub(crate) fn spawn(spec: &crate::LaunchSpec) -> io::Result<crate::ServiceProcess> {
    let started = super::launch::start(
        super::launch::Spec {
            program: &spec.program,
            args: &spec.args,
            env: &spec.env,
            cwd: None,
            profile: None,
            job: None,
            new_process_group: true,
        },
        super::stdio::Pipes::create("OW")?,
    )?;
    let (stdin, stdout, stderr) = started.pipes;
    drop(stdout);
    drop(stderr);
    Ok(crate::ServiceProcess {
        stdin: Some(stdin),
        inner: Detached {
            process: started.process,
        },
    })
}

pub(crate) fn create_private_directory(path: &Path) -> io::Result<()> {
    let path = path_wide(path)?;
    // Protected DACL: the owner and LocalSystem have full access. Inheritance
    // propagates to the descriptor, SQLite database and lock created inside.
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: terminated fixed SDDL and valid writable output pointers.
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    })?;
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: descriptor and path remain alive throughout CreateDirectoryW.
    let result = check(unsafe { CreateDirectoryW(path.as_ptr(), &attrs) });
    // SAFETY: descriptor is a fresh LocalAlloc allocation returned above.
    unsafe {
        LocalFree(descriptor);
    }
    result
}

pub(crate) fn contain_current_process(resources: crate::ResourceLimits) -> io::Result<()> {
    // SAFETY: no name, no inherited handle, default job object security.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owning HANDLE, transferred exactly once.
    let job = unsafe { own(raw) };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = Default::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_MEMORY;
    limits.BasicLimitInformation.ActiveProcessLimit = resources.pids_max.unwrap_or(32);
    limits.JobMemoryLimit = usize::try_from(
        resources.memory_max_bytes.unwrap_or(1024 * 1024 * 1024),
    )
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "memory limit exceeds Windows Job range",
        )
    })?;
    // SAFETY: initialized correctly sized limit structure and valid job handle.
    check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    })?;
    // SAFETY: pseudo handle denotes only this dedicated worker. No child has
    // started yet; subsequent child processes inherit membership atomically.
    check(unsafe { AssignProcessToJobObject(job.as_raw_handle(), GetCurrentProcess()) })?;
    // Deliberate OS-lifetime ownership, not an accidental leak. Rust Drop would
    // terminate its own process while unwinding. Process exit closes the job.
    std::mem::forget(job);
    Ok(())
}

pub(crate) fn process_is_running(pid: u32) -> io::Result<bool> {
    use windows_sys::Win32::{
        Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    // SAFETY: query-only access, no inheritance, caller supplies a numeric PID.
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(false);
        }
        return Err(e);
    }
    // SAFETY: fresh successfully opened owning process handle.
    let handle = unsafe { own(raw) };
    // SAFETY: nonblocking wait against a live owned process handle.
    match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => Ok(false),
        WAIT_TIMEOUT => Ok(true),
        _ => Err(io::Error::last_os_error()),
    }
}
