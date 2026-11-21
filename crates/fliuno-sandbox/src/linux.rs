//! Linux Landlock and seccomp boundary. Unsafe code is limited to syscalls in
//! the post-fork callback; policy construction and path handling happen first.
mod cgroup;
mod guardian;

use super::{Child, LaunchSpec};
use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreatedAttr, Scope,
};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child as NativeChild, Command, ExitStatus, Stdio},
};

const REQUIRED_ABI: ABI = ABI::V6;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const CLONE_NEW_NAMESPACE_FLAGS: u32 = 0x7e02_0080;
const SECCOMP_ARGS_OFFSET: u32 = 16;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;
#[cfg(target_arch = "x86")]
const AUDIT_ARCH: u32 = 0x4000_0003;
#[cfg(target_arch = "arm")]
const AUDIT_ARCH: u32 = 0x4000_0028;
#[cfg(target_arch = "riscv64")]
const AUDIT_ARCH: u32 = 0xc000_00f3;
#[cfg(target_arch = "powerpc64")]
const AUDIT_ARCH: u32 = if cfg!(target_endian = "little") {
    0xc000_0015
} else {
    0x8000_0015
};
#[cfg(target_arch = "s390x")]
const AUDIT_ARCH: u32 = 0x8000_0016;

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "x86",
    target_arch = "arm",
    target_arch = "riscv64",
    target_arch = "powerpc64",
    target_arch = "s390x"
)))]
compile_error!("Linux sandbox seccomp policy does not recognize this syscall ABI");

struct PrivateDirectory {
    path: PathBuf,
    cleanup: bool,
}

impl PrivateDirectory {
    fn create() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("fliuno-linux-{}", uuid::Uuid::new_v4()));
        super::create_private_directory(&path)?;
        Ok(Self {
            path,
            cleanup: true,
        })
    }

    fn preserve(&mut self) {
        self.cleanup = false;
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        if self.cleanup
            && let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("fliuno: Linux sandbox directory cleanup failed: {error}");
        }
    }
}

pub(super) struct Process {
    // Drop the owner before the directory so workers cannot outlive their files.
    owner: OwnedProcess,
    directory: PrivateDirectory,
}

impl Process {
    pub(super) fn usage(&self) -> crate::ProcessUsage {
        self.owner.usage()
    }

    pub(super) fn id(&self) -> u32 {
        self.owner.id()
    }

    pub(super) fn directory(&self) -> &Path {
        &self.directory.path
    }

    pub(super) fn uses_cgroup_v2(&self) -> bool {
        self.owner.cgroup.is_some()
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.owner.try_wait()
    }

    pub(super) fn kill(&mut self) -> io::Result<()> {
        self.owner.kill().inspect_err(|_| self.directory.preserve())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Err(error) = self.kill() {
            eprintln!("fliuno: Linux sandbox process cleanup failed: {error}");
        }
    }
}

// Keep the root PID reserved until group cleanup, including after natural exit.
pub(super) struct OwnedProcess {
    child: NativeChild,
    cgroup: Option<cgroup::CgroupV2>,
    status: Option<ExitStatus>,
    reaped: bool,
}

impl OwnedProcess {
    pub(super) fn usage(&self) -> crate::ProcessUsage {
        self.cgroup
            .as_ref()
            .map(cgroup::CgroupV2::usage)
            .unwrap_or_default()
    }

    fn new(child: NativeChild, cgroup: Option<cgroup::CgroupV2>) -> Self {
        Self {
            child,
            cgroup,
            status: None,
            reaped: false,
        }
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.reaped {
            return Ok(self.status);
        }
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        loop {
            // SAFETY: waitid writes a siginfo_t to this valid buffer for our child.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id() as libc::id_t,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        // SAFETY: waitid returned success and initialized the siginfo_t.
        let info = unsafe { info.assume_init() };
        // SAFETY: waitid initialized the corresponding siginfo_t union fields.
        let (pid, status) = unsafe { (info.si_pid(), info.si_status()) };
        if pid == 0 {
            return Ok(None);
        }
        let raw_status = match info.si_code {
            libc::CLD_EXITED => status << 8,
            libc::CLD_DUMPED => (status & 0x7f) | 0x80,
            _ => status & 0x7f,
        };
        let status = ExitStatus::from_raw(raw_status);
        self.status = Some(status);
        Ok(Some(status))
    }

    pub(super) fn kill(&mut self) -> io::Result<()> {
        if self.reaped {
            return Ok(());
        }
        let mut cleanup_error = self
            .cgroup
            .as_mut()
            .and_then(|group| group.kill_and_wait(std::time::Duration::from_secs(5)).err());
        // SAFETY: this PID is the process-group ID created for this owned child.
        let result = unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
        let group_error = if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                None
            } else {
                Some(error)
            }
        } else {
            None
        };
        if cleanup_error.is_none() {
            cleanup_error = group_error;
        }
        match self.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(error) = self.child.kill()
                    && error.kind() != io::ErrorKind::InvalidInput
                {
                    cleanup_error.get_or_insert(error);
                }
            }
            Err(error) => {
                cleanup_error.get_or_insert(error);
                if let Err(error) = self.child.kill()
                    && error.kind() != io::ErrorKind::InvalidInput
                {
                    cleanup_error.get_or_insert(error);
                }
            }
        }
        self.status = Some(self.child.wait()?);
        self.reaped = true;
        if let Some(error) = cleanup_error {
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if let Err(error) = self.kill() {
            eprintln!("fliuno: Linux process cleanup failed: {error}");
        }
    }
}

/// Owns a trusted service worker and, when available, its delegated process tree.
pub(super) struct ServiceProcess {
    child: NativeChild,
    cgroup: Option<cgroup::CgroupV2>,
    reaped: bool,
}

impl ServiceProcess {
    pub(super) fn wait(&mut self) -> io::Result<()> {
        wait_for_exit_without_reaping(self.child.id())?;
        let cleanup_error = self.kill_descendants();
        let wait_error = self.child.wait().err();
        self.reaped = wait_error.is_none();
        match (cleanup_error, wait_error) {
            (Some(error), _) | (None, Some(error)) => Err(error),
            (None, None) => Ok(()),
        }
    }

    fn kill_descendants(&mut self) -> Option<io::Error> {
        let mut error = self
            .cgroup
            .as_mut()
            .and_then(|group| group.kill_and_wait(std::time::Duration::from_secs(5)).err());
        if (self.cgroup.is_none() || error.is_some())
            && let Err(group_error) = signal_service_group(self.child.id())
        {
            error.get_or_insert(group_error);
        }
        error
    }

    fn terminate_and_reap(&mut self) -> io::Result<()> {
        if self.reaped {
            return Ok(());
        }
        let mut cleanup_error = self.kill_descendants();
        if let Err(error) = self.child.kill()
            && error.kind() != io::ErrorKind::InvalidInput
        {
            cleanup_error.get_or_insert(error);
        }
        let wait_error = self.child.wait().err();
        self.reaped = wait_error.is_none();
        match (cleanup_error, wait_error) {
            (Some(error), _) | (None, Some(error)) => Err(error),
            (None, None) => Ok(()),
        }
    }
}

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        if let Err(error) = self.terminate_and_reap() {
            eprintln!("fliuno: Linux service process cleanup failed: {error}");
        }
    }
}

fn wait_for_exit_without_reaping(pid: u32) -> io::Result<()> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    loop {
        // SAFETY: waitid writes a siginfo_t to this valid buffer for our child.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            // SAFETY: waitid returned success and initialized the siginfo_t.
            let info = unsafe { info.assume_init() };
            // SAFETY: waitid initialized the corresponding siginfo_t union field.
            if unsafe { info.si_pid() } == 0 {
                return Err(io::Error::other("service worker exit was not observed"));
            }
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn signal_service_group(pid: u32) -> io::Result<()> {
    // The unreaped worker keeps its process-group ID from being recycled.
    // SAFETY: this group was created for the owned service worker at spawn.
    if unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

fn prepare_child_process(
    command: &mut Command,
    cgroup: Option<&cgroup::CgroupV2>,
) -> io::Result<()> {
    let cgroup_join = cgroup
        .map(cgroup::CgroupV2::duplicate_join_handle)
        .transpose()?;
    let guardian_control = cgroup
        .map(cgroup::CgroupV2::duplicate_guardian_control)
        .transpose()?;
    // SAFETY: only pre-opened handles and async-signal-safe syscalls are used
    // after fork. CLOEXEC preserves Rust's exec-error pipe until exec succeeds.
    unsafe {
        command.pre_exec(move || {
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 1u32 << 2) == -1 {
                return Err(io::Error::last_os_error());
            }
            if let Some(join) = &cgroup_join {
                cgroup::attach_current_process(join.as_raw_fd())?;
            }
            if let Some(control) = &guardian_control {
                guardian::attach(control.as_raw_fd())?;
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(feature = "async-io")]
pub(super) fn spawn_host(
    spec: &crate::HostLaunchSpec,
) -> io::Result<(OwnedProcess, (File, File, File))> {
    let cgroup = cgroup::CgroupV2::try_create(
        spec.linux_cgroup_parent.as_deref(),
        &spec.resources,
        guardian::Lifetime::Host,
    )?;
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    prepare_child_process(&mut command, cgroup.as_ref())?;
    let mut owner = OwnedProcess::new(command.spawn()?, cgroup);
    let missing_pipe = || io::Error::other("host stdio pipe missing");
    let input = owner.child.stdin.take().ok_or_else(missing_pipe)?;
    let output = owner.child.stdout.take().ok_or_else(missing_pipe)?;
    let error = owner.child.stderr.take().ok_or_else(missing_pipe)?;
    Ok((
        owner,
        (
            File::from(OwnedFd::from(input)),
            File::from(OwnedFd::from(output)),
            File::from(OwnedFd::from(error)),
        ),
    ))
}

pub(super) fn spawn_service(spec: &LaunchSpec) -> io::Result<super::ServiceProcess> {
    let cgroup = cgroup::CgroupV2::try_create(
        spec.linux_cgroup_parent.as_deref(),
        &spec.resources,
        guardian::Lifetime::Service,
    )?;
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(&spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);

    prepare_child_process(&mut command, cgroup.as_ref())?;

    let mut child = command.spawn()?;
    drop(command);
    Ok(super::ServiceProcess {
        stdin: child
            .stdin
            .take()
            .map(|input| File::from(OwnedFd::from(input))),
        inner: ServiceProcess {
            child,
            cgroup,
            reaped: false,
        },
    })
}

pub(super) fn spawn(spec: &LaunchSpec, state: Option<super::PrivateState>) -> io::Result<Child> {
    let directory = PrivateDirectory::create()?;
    let staged = directory.path.join("program");
    crate::program::stage(&spec.program, &staged, spec.program_sha256.as_deref())?;
    crate::assets::stage(&directory.path, &spec.assets)?;
    let state = state
        .map(|snapshot| snapshot.restore(&directory.path))
        .transpose()?;
    let home = state.as_ref().map(|state| state.home.as_path());

    let cgroup = cgroup::CgroupV2::try_create(
        spec.linux_cgroup_parent.as_deref(),
        &spec.resources,
        guardian::Lifetime::Host,
    )?;
    if state.is_some() && cgroup.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private state requires a delegated Linux cgroup v2",
        ));
    }
    let ruleset = create_ruleset(&directory.path)?;
    let seccomp = sandbox_filter();
    let mut command = Command::new(&staged);
    command
        .args(&spec.args)
        .current_dir(&directory.path)
        .env_clear()
        .envs(&spec.env)
        .env("HOME", home.unwrap_or(&directory.path))
        .env("TMPDIR", &directory.path)
        .env("TEMP", &directory.path)
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    if let Some(home) = &home {
        command.env("XDG_CONFIG_HOME", home.join("config"));
        command.env("XDG_DATA_HOME", home.join("data"));
        command.env("XDG_STATE_HOME", home.join("state"));
        command.env("XDG_CACHE_HOME", directory.path.join("cache"));
    }

    prepare_child_process(&mut command, cgroup.as_ref())?;

    // The callback uses only async-signal-safe syscalls and prebuilt data.
    // SAFETY: no allocation, locks, or non-syscall Rust APIs are used after fork.
    unsafe {
        command.pre_exec(move || {
            let program = libc::sock_fprog {
                len: seccomp.len() as u16,
                filter: seccomp.as_ptr() as *mut libc::sock_filter,
            };
            let fd = ruleset.as_raw_fd();

            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            ) == -1
            {
                return Err(io::Error::last_os_error());
            }
            let header = CapabilityHeader {
                version: LINUX_CAPABILITY_VERSION_3,
                pid: 0,
            };
            let empty_capabilities = [
                CapabilityData {
                    effective: 0,
                    permitted: 0,
                    inheritable: 0,
                },
                CapabilityData {
                    effective: 0,
                    permitted: 0,
                    inheritable: 0,
                },
            ];
            if libc::syscall(
                libc::SYS_capset,
                &header as *const CapabilityHeader,
                empty_capabilities.as_ptr(),
            ) == -1
            {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            ) == -1
            {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    drop(command);
    let stdin = child
        .stdin
        .take()
        .map(|input| File::from(OwnedFd::from(input)));
    let stdout = child
        .stdout
        .take()
        .map(|output| File::from(OwnedFd::from(output)));
    let stderr = child
        .stderr
        .take()
        .map(|errors| File::from(OwnedFd::from(errors)));
    Ok(Child {
        stdin,
        stdout,
        stderr,
        state: state.map(Box::new),
        inner: Process {
            owner: OwnedProcess::new(child, cgroup),
            directory,
        },
    })
}

fn create_ruleset(directory: &Path) -> io::Result<OwnedFd> {
    let handled_fs = AccessFs::from_all(REQUIRED_ABI);
    let handled_net = AccessNet::from_all(REQUIRED_ABI);
    let scopes = Scope::from_all(REQUIRED_ABI);
    let mut created = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(handled_fs)
        .and_then(|ruleset| ruleset.handle_access(handled_net))
        .and_then(|ruleset| ruleset.scope(scopes))
        .map_err(io::Error::other)?
        .create()
        .map_err(io::Error::other)?
        .add_rule(PathBeneath::new(
            PathFd::new(directory).map_err(io::Error::other)?,
            AccessFs::from_all(REQUIRED_ABI),
        ))
        .map_err(io::Error::other)?;

    let runtime_access = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
    let mut runtime_paths = BTreeSet::new();
    for path in [
        "/usr/lib",
        "/usr/lib64",
        "/lib",
        "/lib64",
        "/usr/bin",
        "/usr/sbin",
        "/bin",
        "/sbin",
    ] {
        let path = Path::new(path);
        if path.is_dir() {
            let path = fs::canonicalize(path)?;
            if runtime_paths.insert(path.clone()) {
                created = created
                    .add_rule(PathBeneath::new(
                        PathFd::new(path).map_err(io::Error::other)?,
                        runtime_access,
                    ))
                    .map_err(io::Error::other)?;
            }
        }
    }

    let loader_cache = Path::new("/etc/ld.so.cache");
    if loader_cache.is_file() {
        created = created
            .add_rule(PathBeneath::new(
                PathFd::new(loader_cache).map_err(io::Error::other)?,
                AccessFs::ReadFile,
            ))
            .map_err(io::Error::other)?;
    }

    let fd: Option<OwnedFd> = created.into();
    fd.ok_or_else(|| io::Error::other("Landlock did not return a ruleset descriptor"))
}

fn sandbox_filter() -> Vec<libc::sock_filter> {
    fn instruction(code: u16, jump_true: u8, jump_false: u8, value: u32) -> libc::sock_filter {
        libc::sock_filter {
            code,
            jt: jump_true,
            jf: jump_false,
            k: value,
        }
    }

    fn statement(code: u16, value: u32) -> libc::sock_filter {
        instruction(code, 0, 0, value)
    }

    fn blocked_syscalls() -> Vec<libc::c_long> {
        let mut calls = vec![
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_recvfrom,
            libc::SYS_recvmsg,
            libc::SYS_shutdown,
            libc::SYS_setsockopt,
            libc::SYS_getsockopt,
            libc::SYS_sendmmsg,
            libc::SYS_recvmmsg,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_chroot,
            libc::SYS_open_tree,
            libc::SYS_move_mount,
            libc::SYS_fsopen,
            libc::SYS_fsconfig,
            libc::SYS_fsmount,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_pidfd_getfd,
        ];
        #[cfg(target_arch = "x86")]
        calls.push(libc::SYS_socketcall);
        calls.sort_unstable();
        calls.dedup();
        calls
    }

    let mut filter = vec![
        statement((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 4),
        instruction(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            1,
            0,
            AUDIT_ARCH,
        ),
        statement(
            (libc::BPF_RET | libc::BPF_K) as u16,
            SECCOMP_RET_KILL_PROCESS,
        ),
        statement((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 0),
    ];

    #[cfg(target_arch = "x86_64")]
    filter.extend([
        statement(
            (libc::BPF_ALU | libc::BPF_AND | libc::BPF_K) as u16,
            X32_SYSCALL_BIT,
        ),
        instruction(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            1,
            0,
            0,
        ),
        statement(
            (libc::BPF_RET | libc::BPF_K) as u16,
            SECCOMP_RET_KILL_PROCESS,
        ),
        statement((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 0),
    ]);

    filter.extend([
        instruction(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            0,
            4,
            libc::SYS_clone as u32,
        ),
        statement(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_ARGS_OFFSET,
        ),
        statement(
            (libc::BPF_ALU | libc::BPF_AND | libc::BPF_K) as u16,
            CLONE_NEW_NAMESPACE_FLAGS,
        ),
        instruction(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            1,
            0,
            0,
        ),
        statement(
            (libc::BPF_RET | libc::BPF_K) as u16,
            SECCOMP_RET_ERRNO | libc::EPERM as u32,
        ),
        statement((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 0),
        instruction(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            0,
            1,
            libc::SYS_clone3 as u32,
        ),
        statement(
            (libc::BPF_RET | libc::BPF_K) as u16,
            SECCOMP_RET_ERRNO | libc::ENOSYS as u32,
        ),
    ]);

    for syscall in blocked_syscalls() {
        filter.push(instruction(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            0,
            1,
            syscall as u32,
        ));
        filter.push(statement(
            (libc::BPF_RET | libc::BPF_K) as u16,
            SECCOMP_RET_ERRNO | libc::EPERM as u32,
        ));
    }
    filter.push(statement(
        (libc::BPF_RET | libc::BPF_K) as u16,
        SECCOMP_RET_ALLOW,
    ));
    filter
}

pub(super) fn current_process_is_isolated() -> io::Result<bool> {
    // SAFETY: these prctl queries take scalar arguments and do not retain pointers.
    let seccomp = unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) };
    if seccomp == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this prctl query takes scalar arguments and does not retain pointers.
    let no_new_privs = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
    if no_new_privs == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(seccomp == libc::SECCOMP_MODE_FILTER as i32 && no_new_privs == 1)
}
