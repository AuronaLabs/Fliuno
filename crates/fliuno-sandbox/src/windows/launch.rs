//! The shared native creation boundary. Policy prepares program, environment,
//! optional AppContainer and Job; this module transfers exactly three handles.
use super::{
    Attributes, Profile, check, command_line, inheritance::Inheritance, own, path_wide,
    stdio::Pipes, wide,
};
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, OwnedHandle},
    path::Path,
};
use windows_sys::Win32::{
    Security::SECURITY_CAPABILITIES,
    System::{JobObjects::IsProcessInJob, Threading::*},
};

pub(super) struct Spec<'a> {
    pub program: &'a Path,
    pub args: &'a [String],
    pub env: &'a BTreeMap<String, String>,
    pub cwd: Option<&'a Path>,
    pub profile: Option<&'a Profile>,
    pub job: Option<&'a OwnedHandle>,
    pub new_process_group: bool,
}
pub(super) struct Started {
    pub process: OwnedHandle,
    pub pid: u32,
    pub pipes: (File, File, File),
}
pub(super) fn start(spec: Spec<'_>, pipes: Pipes) -> io::Result<Started> {
    let program = path_wide(spec.program)?;
    let mut command = wide(&command_line(
        spec.program.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "native path must be UTF-8")
        })?,
        spec.args,
    ))?;
    if command.len() > 32_767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command line exceeds Windows limit",
        ));
    }
    let cwd = spec.cwd.map(path_wide).transpose()?;
    let mut pairs = spec.env.iter().collect::<Vec<_>>();
    pairs.sort_by_key(|(key, _)| key.to_ascii_uppercase());
    let mut environment = Vec::new();
    for (key, value) in pairs {
        environment.extend(wide(&format!("{key}={value}"))?);
    }
    environment.push(0);
    if environment.len() == 1 {
        environment.push(0);
    }
    let inheritance = Inheritance::new(pipes.child_handles())?;
    let handles = inheritance.handles();
    let caps = [SECURITY_CAPABILITIES {
        AppContainerSid: spec.profile.map_or(std::ptr::null_mut(), Profile::sid),
        Capabilities: std::ptr::null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    }];
    let jobs = [spec
        .job
        .map_or(std::ptr::null_mut(), AsRawHandle::as_raw_handle)];
    let mut attributes =
        Attributes::new(2 + u32::from(spec.profile.is_some()) + u32::from(spec.job.is_some()))?;
    inheritance.set_attributes(&mut attributes)?;
    if spec.profile.is_some() {
        attributes.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &caps)?;
    }
    if spec.job.is_some() {
        attributes.set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
    }
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: size_of::<STARTUPINFOEXW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: handles[0],
            hStdOutput: handles[1],
            hStdError: handles[2],
            ..Default::default()
        },
        lpAttributeList: attributes.ptr(),
    };
    let mut info = PROCESS_INFORMATION::default();
    let group = if spec.new_process_group {
        CREATE_NEW_PROCESS_GROUP
    } else {
        0
    };
    // SAFETY: all attribute backing values and encoded buffers remain alive.
    // Inheritance comes only from the surrogate; the entry point stays paused.
    check(unsafe {
        CreateProcessW(
            program.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_NO_WINDOW
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT
                | group,
            environment.as_ptr().cast(),
            cwd.as_ref().map_or(std::ptr::null(), |cwd| cwd.as_ptr()),
            &startup.StartupInfo,
            &mut info,
        )
    })?;
    // SAFETY: successful creation returned distinct uniquely owned handles.
    let process = unsafe { own(info.hProcess) };
    // SAFETY: unique initial-thread handle from that same creation.
    let thread = unsafe { own(info.hThread) };
    drop(attributes);
    let setup = (|| {
        inheritance.verify_child(&process)?;
        drop(inheritance);
        if let Some(job) = spec.job {
            let mut inside = 0;
            // SAFETY: read-only check of a live process and live workload Job.
            check(unsafe {
                IsProcessInJob(process.as_raw_handle(), job.as_raw_handle(), &mut inside)
            })?;
            if inside == 0 {
                return Err(io::Error::other("child is not contained in workload job"));
            }
        }
        // SAFETY: unique initial thread, suspended exactly once until now.
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if let Err(error) = setup {
        // SAFETY: setup failed before entry; never abandon a suspended child.
        unsafe {
            TerminateProcess(process.as_raw_handle(), 1);
            WaitForSingleObject(process.as_raw_handle(), 5000);
        }
        return Err(error);
    }
    Ok(Started {
        process,
        pid: info.dwProcessId,
        pipes: pipes.into_parent(),
    })
}
