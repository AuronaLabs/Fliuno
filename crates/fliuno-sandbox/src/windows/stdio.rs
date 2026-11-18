use super::{check, own, wide};
use std::{
    fs::File,
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, OwnedHandle},
};
use windows_sys::Win32::{
    Foundation::{HANDLE, LocalFree},
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    },
    System::Pipes::CreatePipe,
};

pub(super) struct Pipes {
    input: (OwnedHandle, OwnedHandle),
    output: (OwnedHandle, OwnedHandle),
    error: (OwnedHandle, OwnedHandle),
}
impl Pipes {
    pub(super) fn create(sid: &str) -> io::Result<Self> {
        let descriptor = Descriptor::new(sid)?;
        // No inheritable handles may exist in the embedding host: an unrelated
        // std::Command can otherwise inherit them concurrently. The inheritance
        // broker duplicates only the child ends into its own handle table.
        let attrs = descriptor.attributes(false);
        fn pair(attrs: &SECURITY_ATTRIBUTES) -> io::Result<(OwnedHandle, OwnedHandle)> {
            let mut read = std::ptr::null_mut();
            let mut write = std::ptr::null_mut();
            // SAFETY: valid outputs, sized attributes and live security descriptor.
            check(unsafe { CreatePipe(&mut read, &mut write, attrs, 0) })?;
            // SAFETY: successful creation returned two distinct owning handles.
            let read = unsafe { own(read) };
            // SAFETY: the other distinct handle from the same successful creation.
            let write = unsafe { own(write) };
            Ok((read, write))
        }
        Ok(Self {
            input: pair(&attrs)?,
            output: pair(&attrs)?,
            error: pair(&attrs)?,
        })
    }

    #[cfg(feature = "async-io")]
    pub(super) fn create_async(sid: &str) -> io::Result<Self> {
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
            Storage::FileSystem::{
                CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
                PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND, SECURITY_SQOS_PRESENT,
            },
            System::Pipes::{CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS},
        };
        let descriptor = Descriptor::new(sid)?;
        fn pair(
            descriptor: &Descriptor,
            parent_is_reader: bool,
        ) -> io::Result<(OwnedHandle, OwnedHandle)> {
            let name = wide(&format!(r"\\.\pipe\fliuno-stdio-{}", uuid::Uuid::new_v4()))?;
            let mode = if parent_is_reader {
                PIPE_ACCESS_INBOUND
            } else {
                PIPE_ACCESS_OUTBOUND
            };
            let parent_attrs = descriptor.attributes(false);
            // SAFETY: terminated private name and live descriptor. Only this
            // first, local byte-stream instance is allowed; parent is not inherited.
            let raw = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    mode | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    16 * 1024,
                    16 * 1024,
                    0,
                    &parent_attrs,
                )
            };
            if raw == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: freshly returned successful owning server handle.
            let parent = unsafe { own(raw) };
            let child_attrs = descriptor.attributes(false);
            // Open the synchronous child end while still trusted. It needs no
            // name lookup after lowering its token; only this handle is inherited.
            // SAFETY: server and descriptor remain alive throughout this open.
            let raw = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    if parent_is_reader {
                        GENERIC_WRITE
                    } else {
                        GENERIC_READ
                    },
                    0,
                    &child_attrs,
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT,
                    std::ptr::null_mut(),
                )
            };
            if raw == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: separately returned successful owning client handle.
            let child = unsafe { own(raw) };
            Ok(if parent_is_reader {
                (parent, child)
            } else {
                (child, parent)
            })
        }
        Ok(Self {
            input: pair(&descriptor, false)?,
            output: pair(&descriptor, true)?,
            error: pair(&descriptor, true)?,
        })
    }
    pub(super) fn child_handles(&self) -> [HANDLE; 3] {
        [
            self.input.0.as_raw_handle(),
            self.output.1.as_raw_handle(),
            self.error.1.as_raw_handle(),
        ]
    }
    pub(super) fn into_parent(self) -> (File, File, File) {
        (
            self.input.1.into(),
            self.output.0.into(),
            self.error.0.into(),
        )
    }
}
struct Descriptor(*mut std::ffi::c_void);
impl Descriptor {
    fn new(sid: &str) -> io::Result<Self> {
        // Low mandatory label permits low-integrity writes; only the exact
        // package SID and owner receive access, never all application packages.
        let sddl = wide(&format!("D:(A;;GA;;;{sid})(A;;GA;;;OW)S:(ML;;NW;;;LW)"))?;
        let mut raw_sd = std::ptr::null_mut();
        // SAFETY: terminated SDDL input and valid descriptor output pointer.
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut raw_sd,
                std::ptr::null_mut(),
            )
        })?;
        Ok(Self(raw_sd))
    }
    fn attributes(&self, inherit: bool) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: i32::from(inherit),
        }
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: matching allocator for the SDDL conversion, once after pipe setup.
        unsafe {
            LocalFree(self.0);
        }
    }
}

#[cfg(all(test, feature = "async-io"))]
mod tests {
    use super::*;
    use std::{io::Write, process::Stdio, time::Duration};
    use tokio::io::AsyncReadExt;

    // A second copy of this test binary is a controllable unrelated process.
    // Its stdin keeps it alive until the parent drops that endpoint.
    #[test]
    fn unrelated_process_fixture() {
        if std::env::var_os("FLIUNO_INHERITANCE_FIXTURE").is_none() {
            return;
        }
        std::io::stdout().write_all(b"ready\n").unwrap();
        std::io::stdout().flush().unwrap();
        std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).unwrap();
    }

    #[test]
    fn unrelated_std_spawn_cannot_retain_private_pipe_endpoints() {
        let pipes = Pipes::create_async("OW").unwrap();
        let inheritance =
            super::super::inheritance::Inheritance::new(pipes.child_handles()).unwrap();
        let mut unrelated = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::stdio::tests::unrelated_process_fixture",
                "--nocapture",
            ])
            .env("FLIUNO_INHERITANCE_FIXTURE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        drop(inheritance);
        let (input, output, error) = pipes.into_parent();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let pipes = super::super::async_io::register(input, output, error).unwrap();
            drop(pipes.stdin);
            let drain = |mut stream: crate::PipeReader| async move {
                let mut bytes = Vec::new();
                match stream.read_to_end(&mut bytes).await {
                    Ok(_) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                    Err(error) => Err(error),
                }
            };
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::try_join!(drain(pipes.stdout), drain(pipes.stderr))
            })
            .await
        });
        let alive = unrelated.try_wait().unwrap().is_none();
        // Clean up even when the regression is present, so a failure never
        // leaves a child holding the test process's stdin/stdout handles.
        drop(unrelated.stdin.take());
        unrelated.wait().unwrap();
        assert!(
            alive,
            "unrelated process exit masked inherited pipe handles"
        );
        assert!(
            result.is_ok(),
            "an unrelated std::Command retained private pipes"
        );
        result.unwrap().unwrap();
    }
}
