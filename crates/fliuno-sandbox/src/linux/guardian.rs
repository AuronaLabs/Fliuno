//! A small, independent cgroup owner. The forked branch uses only raw syscalls
//! and stack buffers: it must never use the multithreaded host's allocator,
//! locks, logging, destructors or Rust runtime after fork.
//!
//! Before exec, a workload passes its own pidfd and waits for acknowledgment.
//! A normal sandbox follows the host; an independent service follows its worker
//! after this handshake. PID numbers and spawning-thread death are never used
//! as lifetime authority. The guardian stays outside the workload's cgroup.
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
};

#[derive(Clone, Copy)]
pub(super) enum Lifetime {
    Host,
    Service,
}

pub(super) struct Guardian {
    control: OwnedFd,
}

impl Guardian {
    pub(super) fn start(path: &Path, kill: &File, lifetime: Lifetime) -> io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let events = OpenOptions::new()
            .read(true)
            .open(path.join("cgroup.events"))?;
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::other("invalid cgroup guardian path"))?;
        let owner = pidfd_self()?;
        let mut sockets = [-1; 2];
        // SAFETY: sockets points to two writable descriptors. CLOEXEC prevents
        // workloads from retaining either guardian control endpoint after exec.
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                sockets.as_mut_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: socketpair returned two distinct, uniquely owned descriptors.
        let control = unsafe { OwnedFd::from_raw_fd(sockets[0]) };
        // SAFETY: the second socketpair descriptor is separately owned.
        let monitor = unsafe { OwnedFd::from_raw_fd(sockets[1]) };
        let descriptors = [
            owner.as_raw_fd(),
            kill.as_raw_fd(),
            events.as_raw_fd(),
            monitor.as_raw_fd(),
        ];
        // The child must not run a host runtime signal handler before it has
        // reset dispositions. Block on this spawning thread across fork only.
        // SAFETY: both masks are valid stack storage initialized by libc.
        let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        // SAFETY: the mask is fully initialized before pthread_sigmask reads it.
        let blocked = unsafe {
            let mut all: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&mut all);
            libc::pthread_sigmask(libc::SIG_SETMASK, &all, &mut previous)
        };
        if blocked != 0 {
            return Err(io::Error::from_raw_os_error(blocked));
        }
        // SAFETY: the child immediately enters a syscall-only, non-returning
        // branch; all buffers and descriptors were prepared before fork.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            run(descriptors, &path, lifetime);
        }
        let fork_error = (pid < 0).then(io::Error::last_os_error);
        // SAFETY: restore the valid mask saved on this same parent thread.
        let restored =
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };
        if let Some(error) = fork_error {
            return Err(error);
        }
        if restored != 0 {
            // SAFETY: the unreaped guardian is our child; no workload exists.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            reap(pid);
            return Err(io::Error::from_raw_os_error(restored));
        }
        drop(monitor);
        // A dedicated waiter prevents zombies while the host remains alive.
        // After host exit the guardian is adopted normally and remains active.
        if let Err(error) = std::thread::Builder::new()
            .name("cgroup-guardian-reaper".into())
            .spawn(move || reap(pid))
        {
            // No workload has been launched yet. Abort the otherwise unowned
            // guardian and synchronously reap it before failing closed.
            // SAFETY: pid is our unreaped fork child and cannot be reused.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            reap(pid);
            return Err(error);
        }
        if !wait_readable(control.as_raw_fd(), 5_000) || receive_byte(control.as_raw_fd()) != b'R' {
            return Err(io::Error::other("cgroup guardian did not become ready"));
        }
        Ok(Self { control })
    }

    pub(super) fn duplicate_control(&self) -> io::Result<OwnedFd> {
        self.control.try_clone()
    }
}

fn pidfd_self() -> io::Result<OwnedFd> {
    // SAFETY: both syscalls take only scalar arguments; a successful pidfd_open
    // returns a new close-on-exec descriptor referring to this process.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this successful syscall returned a unique descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Called only in pre_exec, after cgroup attachment and before restrictions.
pub(super) fn attach(control: RawFd) -> io::Result<()> {
    let worker = pidfd_self()?;
    if !send_identity(control, worker.as_raw_fd())
        || !wait_readable(control, 5_000)
        || receive_byte(control) != b'G'
    {
        return Err(io::Error::from_raw_os_error(libc::EIO));
    }
    Ok(())
}

// cmsg storage must be suitably aligned on both 32-bit and 64-bit platforms.
type Ancillary = [usize; 4];

fn send_identity(socket: RawFd, pidfd: RawFd) -> bool {
    let mut byte = b'A';
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control: Ancillary = [0; 4];
    // SAFETY: zero is a valid initial msghdr, and every used pointer below
    // refers to an aligned, initialized stack buffer alive for sendmsg.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
        std::ptr::write(libc::CMSG_DATA(header).cast::<RawFd>(), pidfd);
        loop {
            let result = libc::sendmsg(socket, &msg, libc::MSG_NOSIGNAL);
            if result == 1 {
                return true;
            }
            if result >= 0 || errno() != libc::EINTR {
                return false;
            }
        }
    }
}

fn receive_identity(socket: RawFd) -> RawFd {
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control: Ancillary = [0; 4];
    // SAFETY: the msghdr and aligned ancillary/iovec buffers are live and
    // writable for recvmsg; lengths and header shape are checked before access.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control);
        let result = loop {
            let result = libc::recvmsg(socket, &mut msg, libc::MSG_CMSG_CLOEXEC);
            if result >= 0 || errno() != libc::EINTR {
                break result;
            }
        };
        if result != 1 || byte != b'A' || msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        {
            return -1;
        }
        let header = libc::CMSG_FIRSTHDR(&msg);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
            || (*header).cmsg_len != libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize
            || !libc::CMSG_NXTHDR(&msg, header).is_null()
        {
            return -1;
        }
        std::ptr::read(libc::CMSG_DATA(header).cast::<RawFd>())
    }
}

fn run(descriptors: [RawFd; 4], path: &CString, lifetime: Lifetime) -> ! {
    let [owner, kill, events, socket] = descriptors;
    let mut keep = descriptors;
    keep.sort_unstable();
    // SAFETY: setsid detaches the guardian from the host terminal; close_range
    // preserves only the four pre-opened descriptors, including no RPC pipes,
    // database locks or credentials. No library cleanup is run in this child.
    unsafe {
        let mut first = 0u32;
        for fd in keep {
            if first < fd as u32
                && libc::syscall(libc::SYS_close_range, first, fd as u32 - 1, 0) < 0
            {
                libc::_exit(1);
            }
            first = fd as u32 + 1;
        }
        if libc::syscall(libc::SYS_close_range, first, u32::MAX, 0) < 0 || libc::setsid() < 0 {
            libc::_exit(1);
        }
        // Reset inherited runtime handlers and unblock signals without calling
        // those handlers in a process whose other threads no longer exist.
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in 1..=64 {
            if signal != libc::SIGKILL && signal != libc::SIGSTOP {
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        if libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0 {
            libc::_exit(1);
        }
    }
    if send_byte(socket, b'R') && wait_pair(owner, socket) == 1 {
        let worker = receive_identity(socket);
        // A pre-exec child cannot run until its identity is accepted while its
        // creator is still alive. A lost guardian therefore fails closed.
        if worker >= 0 && !wait_readable(owner, 0) && send_byte(socket, b'G') {
            if matches!(lifetime, Lifetime::Service) {
                // A service is independent after handoff. Host exit and pipe
                // closure no longer end it; only its pinned worker identity does.
                let _ = wait_readable(worker, -1);
            } else {
                let _ = wait_pair(owner, socket);
            }
        }
    }
    let success = clean_group(kill, events, path);
    // SAFETY: exit without touching the inherited Rust runtime or destructors.
    unsafe { libc::_exit(if success { 0 } else { 1 }) }
}

// Returns the ready slot, preferring host death over a simultaneous handshake.
fn wait_pair(target: RawFd, socket: RawFd) -> i32 {
    let mut fds = [
        libc::pollfd {
            fd: target,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: socket,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: fds is a valid pair of poll descriptors, including error/hup.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } >= 0 {
            return if fds[0].revents != 0 { 0 } else { 1 };
        }
        if errno() != libc::EINTR {
            return -1;
        }
    }
}

fn wait_readable(fd: RawFd, timeout: i32) -> bool {
    let deadline = if timeout >= 0 {
        let Some(now) = monotonic_millis() else {
            return false;
        };
        Some(now + i64::from(timeout))
    } else {
        None
    };
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let remaining = if let Some(deadline) = deadline {
            let Some(now) = monotonic_millis() else {
                return false;
            };
            (deadline - now).clamp(0, i64::from(i32::MAX)) as i32
        } else {
            -1
        };
        // SAFETY: descriptor is a valid poll buffer of one entry.
        let result = unsafe { libc::poll(&mut descriptor, 1, remaining) };
        if result >= 0 {
            return result > 0;
        }
        if errno() != libc::EINTR {
            return false;
        }
    }
}

// libc time_t/c_long have different widths on the supported Linux ABIs.
#[allow(clippy::useless_conversion)]
fn monotonic_millis() -> Option<i64> {
    // SAFETY: timespec is writable stack storage initialized by clock_gettime.
    unsafe {
        let mut time: libc::timespec = std::mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) != 0 {
            return None;
        }
        Some(i64::from(time.tv_sec) * 1_000 + i64::from(time.tv_nsec) / 1_000_000)
    }
}

fn send_byte(fd: RawFd, byte: u8) -> bool {
    loop {
        // SAFETY: byte is a readable single-byte buffer; suppress SIGPIPE.
        let count = unsafe { libc::send(fd, (&byte as *const u8).cast(), 1, libc::MSG_NOSIGNAL) };
        if count == 1 {
            return true;
        }
        if count >= 0 || errno() != libc::EINTR {
            return false;
        }
    }
}

fn receive_byte(fd: RawFd) -> u8 {
    let mut byte = 0;
    loop {
        // SAFETY: byte is a valid one-byte destination.
        let count = unsafe { libc::recv(fd, (&mut byte as *mut u8).cast(), 1, 0) };
        if count == 1 {
            return byte;
        }
        if count >= 0 || errno() != libc::EINTR {
            return 0;
        }
    }
}

fn clean_group(kill: RawFd, events: RawFd, path: &CString) -> bool {
    // Repeated kill/remove also closes the startup race with a child that was
    // attaching when its host died. No further exec is possible without our ack.
    for _ in 0..500 {
        // SAFETY: the kill handle is pre-opened, and b"1" is a one-byte command.
        let written = unsafe { libc::pwrite(kill, b"1".as_ptr().cast(), 1, 0) };
        if written < 0 && !matches!(errno(), libc::EINTR | libc::ENODEV | libc::ENOENT) {
            return false;
        }
        let mut state = [0u8; 256];
        // SAFETY: state is a writable stack buffer, and pread resets the offset.
        let count = unsafe { libc::pread(events, state.as_mut_ptr().cast(), state.len(), 0) };
        if count > 0
            && state[..count as usize]
                .split(|byte| *byte == b'\n')
                .any(|line| line == b"populated 0")
        {
            // SAFETY: path is a prebuilt NUL-terminated cgroup directory name.
            if unsafe { libc::rmdir(path.as_ptr()) } == 0 || errno() == libc::ENOENT {
                return true;
            }
        } else if count < 0 && matches!(errno(), libc::ENODEV | libc::ENOENT) {
            return true;
        }
        // SAFETY: an empty poll with a finite timeout is a syscall-only sleep.
        unsafe { libc::poll(std::ptr::null_mut(), 0, 10) };
    }
    false
}

fn errno() -> i32 {
    // SAFETY: libc returns this thread's live errno pointer, including after fork.
    unsafe { *libc::__errno_location() }
}

fn reap(pid: libc::pid_t) {
    loop {
        let mut status = 0;
        // SAFETY: pid identifies our owned fork child, status is writable.
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            if status != 0 {
                eprintln!("fliuno: Linux cgroup guardian exited without confirmed cleanup");
            }
            return;
        }
        if result < 0 && errno() != libc::EINTR {
            return;
        }
    }
}
