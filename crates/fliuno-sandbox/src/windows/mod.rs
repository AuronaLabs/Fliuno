//! Auditable Win32 boundary. Raw handles become RAII owners immediately; all
//! process attributes and their backing allocations outlive CreateProcessW.
#[cfg(feature = "async-io")]
pub(super) mod async_io;
#[cfg(feature = "async-io")]
pub(crate) mod host;
mod inheritance;
mod job;
mod launch;
mod process;
pub(crate) use process::JobProcess;
mod profile;
pub(crate) mod service;
mod stdio;

use super::{Child, LaunchSpec, PrivateState};
use profile::Profile;
use std::{
    io,
    mem::{size_of, size_of_val},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    process::ExitStatus,
};
use windows_sys::Win32::{
    Foundation::HANDLE,
    Security::{GetTokenInformation, TOKEN_QUERY, TokenIsAppContainer},
    System::Threading::*,
};

fn check(value: i32) -> io::Result<()> {
    if value == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wide(value: &str) -> io::Result<Vec<u16>> {
    if value.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in native string",
        ));
    }
    Ok(value.encode_utf16().chain(Some(0)).collect())
}

fn path_wide(path: &Path) -> io::Result<Vec<u16>> {
    wide(
        path.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "native path must be UTF-8")
        })?,
    )
}

/// Every successful caller must transfer a freshly returned owning HANDLE once.
unsafe fn own(handle: HANDLE) -> OwnedHandle {
    // SAFETY: callers only pass successful Win32 creations with unique ownership.
    unsafe { OwnedHandle::from_raw_handle(handle) }
}

/// Attributes borrow their values, which must remain alive until process launch.
struct Attributes {
    storage: Vec<usize>,
}
impl Attributes {
    fn new(count: u32) -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: the sizing call writes only bytes; the expected first call fails.
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut bytes);
        }
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        // SAFETY: storage is suitably aligned, sized by the API and remains owned.
        check(unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), count, 0, &mut bytes)
        })?;
        Ok(Self { storage })
    }
    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
    fn set<T>(&mut self, attribute: u32, values: &[T]) -> io::Result<()> {
        // SAFETY: values remain owned by the caller through CreateProcessW.
        check(unsafe {
            UpdateProcThreadAttribute(
                self.ptr(),
                0,
                attribute as usize,
                values.as_ptr().cast(),
                size_of_val(values),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        })
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: the list was initialized over this still-live storage.
        unsafe {
            DeleteProcThreadAttributeList(self.ptr());
        }
    }
}

pub(super) struct Process {
    state: JobProcess,
    profile: Profile,
}
impl Process {
    pub(super) fn usage(&self) -> crate::ProcessUsage {
        self.state.usage()
    }

    pub(super) fn id(&self) -> u32 {
        self.state.id()
    }
    pub(super) fn directory(&self) -> &Path {
        self.profile.directory()
    }
    pub(super) fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        self.state.try_wait()
    }
    pub(super) fn kill(&self) -> io::Result<()> {
        self.state.kill()
    }
    pub(super) fn wait_for_empty_job(&self) -> io::Result<()> {
        self.state.wait_for_empty_job()
    }
}

pub(super) fn spawn(spec: &LaunchSpec, state: Option<PrivateState>) -> io::Result<Child> {
    spawn_with_pipes(spec, state, stdio::Pipes::create)
}

#[cfg(feature = "async-io")]
pub(super) fn spawn_async_pipes(
    spec: &LaunchSpec,
    state: Option<PrivateState>,
) -> io::Result<Child> {
    spawn_with_pipes(spec, state, stdio::Pipes::create_async)
}

fn spawn_with_pipes(
    spec: &LaunchSpec,
    state: Option<PrivateState>,
    create_pipes: fn(&str) -> io::Result<stdio::Pipes>,
) -> io::Result<Child> {
    let profile = Profile::create()?;
    // One native executable and explicit pinned companions. No PATH resolution,
    // shell expansion or implicit host DLL/directory grant.
    if !spec
        .program
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scratch sandbox requires a native .exe",
        ));
    }
    let staged = profile.stage_program(&spec.program, spec.program_sha256.as_deref())?;
    crate::assets::stage(profile.directory(), &spec.assets)?;
    let state = state
        .map(|snapshot| snapshot.restore(profile.directory()))
        .transpose()?;
    let mut env = spec.env.clone();
    // Replace even differently-cased spellings of these reserved locations.
    env.retain(|key, _| {
        !["TEMP", "TMP", "TMPDIR", "USERPROFILE", "LOCALAPPDATA"]
            .iter()
            .any(|name| key.eq_ignore_ascii_case(name))
    });
    for key in ["TEMP", "TMP", "TMPDIR", "USERPROFILE", "LOCALAPPDATA"] {
        env.insert(
            key.into(),
            profile.directory().to_string_lossy().into_owned(),
        );
    }
    if let Some(state) = &state {
        let keys = ["HOME", "USERPROFILE", "LOCALAPPDATA", "APPDATA"];
        env.retain(|key, _| !keys.iter().any(|name| key.eq_ignore_ascii_case(name)));
        for key in keys {
            env.insert(key.into(), state.home.to_string_lossy().into_owned());
        }
    }
    if !env.keys().any(|key| key.eq_ignore_ascii_case("SystemRoot"))
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        env.insert("SystemRoot".into(), root.to_string_lossy().into_owned());
    }
    let job = job::create(spec.resources)?;
    let started = launch::start(
        launch::Spec {
            program: &staged,
            args: &spec.args,
            env: &env,
            cwd: Some(profile.directory()),
            profile: Some(&profile),
            job: Some(&job),
            new_process_group: false,
        },
        create_pipes(profile.sid_string())?,
    )?;
    let (stdin, stdout, stderr) = started.pipes;
    Ok(Child {
        stdin: Some(stdin),
        stdout: Some(stdout),
        stderr: Some(stderr),
        state: state.map(Box::new),
        inner: Process {
            state: JobProcess {
                process: started.process,
                job,
                pid: started.pid,
            },
            profile,
        },
    })
}

/// CRT argument encoding; CreateProcessW receives a separate executable path.
fn command_line(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(|arg| {
            let mut encoded = String::from("\"");
            let mut slashes = 0;
            for ch in arg.chars() {
                if ch == '\\' {
                    slashes += 1;
                    continue;
                }
                if ch == '"' {
                    encoded.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                } else {
                    encoded.extend(std::iter::repeat_n('\\', slashes));
                }
                encoded.push(ch);
                slashes = 0;
            }
            encoded.extend(std::iter::repeat_n('\\', slashes * 2));
            encoded.push('"');
            encoded
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn current_process_is_app_container() -> io::Result<bool> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: current process pseudo handle is borrowed; raw receives a new token.
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) })?;
    // SAFETY: token creation succeeded and returned unique ownership.
    let token = unsafe { own(raw) };
    let mut is_container: u32 = 0;
    let mut returned = 0;
    // SAFETY: is_container has the DWORD size required by TokenIsAppContainer.
    check(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenIsAppContainer,
            (&mut is_container as *mut u32).cast(),
            size_of_val(&is_container) as u32,
            &mut returned,
        )
    })?;
    Ok(is_container != 0)
}
