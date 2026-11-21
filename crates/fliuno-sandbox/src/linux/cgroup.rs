//! Optional cgroup v2 containment for Linux sandbox processes.
use super::guardian::{Guardian, Lifetime};
use crate::ResourceLimits;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::fs::FileExt,
    },
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

pub(super) struct CgroupV2 {
    path: PathBuf,
    join: Option<File>,
    kill_access: Option<File>,
    events: Option<File>,
    guardian: Option<Guardian>,
    cleanup: bool,
    pub(super) limits: ResourceLimits,
}

impl CgroupV2 {
    pub(super) fn try_create(
        configured_parent: Option<&Path>,
        limits: &ResourceLimits,
        lifetime: Lifetime,
    ) -> io::Result<Option<Self>> {
        let required = !limits.is_empty() || configured_parent.is_some();
        let parent = match configured_parent {
            Some(path) => path.to_owned(),
            None => match current_cgroup_directory()? {
                Some(path) => path,
                None if required => return Err(delegation_required()),
                None => return Ok(None),
            },
        };
        if !parent.is_dir() {
            return unavailable_result(required);
        }
        if let Err(error) = OpenOptions::new()
            .write(true)
            .open(parent.join("cgroup.procs"))
        {
            if unavailable(&error) {
                return unavailable_result(required);
            }
            return Err(error);
        }

        let path = parent.join(format!("fliuno-{}", uuid::Uuid::new_v4()));
        if let Err(error) = fs::create_dir(&path) {
            if unavailable(&error) {
                return unavailable_result(required);
            }
            return Err(error);
        }

        let mut group = Self {
            path,
            join: None,
            kill_access: None,
            events: None,
            guardian: None,
            cleanup: true,
            limits: *limits,
        };
        let opened = (|| {
            group.join = Some(
                OpenOptions::new()
                    .write(true)
                    .open(group.path.join("cgroup.procs"))?,
            );
            group.kill_access = Some(
                OpenOptions::new()
                    .write(true)
                    .open(group.path.join("cgroup.kill"))?,
            );
            if fs::read_to_string(group.path.join("cgroup.type"))?.trim() != "domain" {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "delegated cgroup is not a domain cgroup",
                ));
            }
            let events = File::open(group.path.join("cgroup.events"))?;
            read_populated(&events)?;
            group.events = Some(events);
            Ok::<_, io::Error>(())
        })();

        if let Err(error) = opened {
            drop(group);
            if unavailable(&error) {
                return unavailable_result(required);
            }
            return Err(error);
        }
        group.apply_limits(limits)?;
        // Every accepted cgroup has an independent owner before a workload
        // can join. Guardian setup failures never fall back to weaker lifetime.
        group.guardian = Some(Guardian::start(
            &group.path,
            group.kill_access.as_ref().unwrap(),
            lifetime,
        )?);
        Ok(Some(group))
    }

    pub(super) fn usage(&self) -> crate::ProcessUsage {
        // Kernel-provided cgroup attributes. Missing controllers are normal.
        let cpu_ms = fs::read_to_string(self.path.join("cpu.stat"))
            .ok()
            .and_then(|stat| {
                let value = stat
                    .lines()
                    .find_map(|line| line.strip_prefix("usage_usec "))?;
                value
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .map(|micros| micros / 1_000)
            });
        let peak_memory_bytes = fs::read_to_string(self.path.join("memory.peak"))
            .ok()
            .and_then(|value| value.trim().parse().ok());
        crate::ProcessUsage {
            cpu_ms,
            peak_memory_bytes,
        }
    }

    fn apply_limits(&self, limits: &ResourceLimits) -> io::Result<()> {
        if let Some(bytes) = limits.memory_max_bytes {
            self.write_limit("memory.max", &bytes.to_string())?;
        }
        if let Some(count) = limits.pids_max {
            self.write_limit("pids.max", &count.to_string())?;
        }
        if let Some(quota) = limits.cpu_quota_micros {
            self.write_limit("cpu.max", &format!("{quota} 100000"))?;
        }
        Ok(())
    }

    fn write_limit(&self, name: &str, value: &str) -> io::Result<()> {
        let path = self.path.join(name);
        let result = (|| {
            let command = format!("{value}\n");
            OpenOptions::new()
                .write(true)
                .open(&path)?
                .write_all(command.as_bytes())?;
            let observed = fs::read_to_string(&path)?;
            if observed.trim() != value {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("cgroup controller {name} did not retain the requested value"),
                ));
            }
            Ok(())
        })();
        result.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("requested resource limit requires the delegated {name} controller"),
                )
            } else {
                error
            }
        })
    }

    pub(super) fn duplicate_join_handle(&self) -> io::Result<File> {
        self.join
            .as_ref()
            .ok_or_else(|| io::Error::other("cgroup join handle is closed"))?
            .try_clone()
    }

    pub(super) fn duplicate_guardian_control(&self) -> io::Result<OwnedFd> {
        self.guardian
            .as_ref()
            .ok_or_else(|| io::Error::other("cgroup guardian is not ready"))?
            .duplicate_control()
    }

    pub(super) fn kill_and_wait(&mut self, timeout: Duration) -> io::Result<()> {
        // The guardian can finish cleanup first after a service worker exits.
        // Keep the original kill handle; never reopen a possibly replaced path.
        let result = self
            .kill_access
            .as_mut()
            .ok_or_else(|| io::Error::other("cgroup kill handle is closed"))?
            .write_all_at(b"1", 0);
        if let Err(error) = result {
            if matches!(error.raw_os_error(), Some(libc::ENODEV | libc::ENOENT))
                && !self.path.exists()
            {
                return Ok(());
            }
            self.cleanup = false;
            return Err(error);
        }

        let result = self
            .events
            .as_ref()
            .ok_or_else(|| io::Error::other("cgroup events handle is closed"))
            .and_then(|events| wait_for_empty(events, Instant::now() + timeout));
        if result.is_err() {
            self.cleanup = false;
        }
        result
    }
}

fn unavailable_result(required: bool) -> io::Result<Option<CgroupV2>> {
    if required {
        Err(delegation_required())
    } else {
        Ok(None)
    }
}

fn delegation_required() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "requested cgroup features require a writable delegated Linux cgroup v2 parent",
    )
}

impl Drop for CgroupV2 {
    fn drop(&mut self) {
        if self.cleanup
            && self
                .events
                .as_ref()
                .is_some_and(|file| matches!(read_populated(file), Ok(true)))
            && let Err(error) = self.kill_and_wait(Duration::from_secs(5))
        {
            eprintln!("fliuno: Linux cgroup process cleanup failed: {error}");
            return;
        }
        if self.cleanup
            && let Err(error) = fs::remove_dir(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("fliuno: Linux cgroup cleanup failed: {error}");
        }
        drop(self.join.take());
        drop(self.kill_access.take());
        // Closing this capability wakes an ordinary host-bound guardian. An
        // independent service guardian instead follows its pinned worker.
        drop(self.guardian.take());
    }
}

pub(super) fn attach_current_process(fd: RawFd) -> io::Result<()> {
    // The callback runs after fork. Keep PID formatting on the stack and issue a
    // single cgroup.procs write so the child is contained before its first exec.
    // SAFETY: getpid has no pointer arguments or external state to validate.
    let pid = unsafe { libc::getpid() };
    if pid <= 0 {
        return Err(io::Error::other("invalid process id for cgroup attachment"));
    }

    let mut digits = [0u8; 10];
    let mut value = pid as u32;
    let mut length = 0;
    loop {
        digits[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    digits[..length].reverse();
    let mut line = [0u8; 11];
    line[..length].copy_from_slice(&digits[..length]);
    line[length] = b'\n';

    loop {
        // SAFETY: the buffer is valid for `length + 1` bytes and the descriptor
        // refers to the cgroup.procs file opened before fork.
        let written = unsafe { libc::write(fd, line.as_ptr().cast(), length + 1) };
        if written == (length + 1) as isize {
            return Ok(());
        }
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short write while attaching process to cgroup",
        ));
    }
}

fn current_cgroup_directory() -> io::Result<Option<PathBuf>> {
    let membership = match fs::read_to_string("/proc/self/cgroup") {
        Ok(value) => value,
        Err(error) if unavailable(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mountinfo = match fs::read_to_string("/proc/self/mountinfo") {
        Ok(value) => value,
        Err(error) if unavailable(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(cgroup_directory_from(&membership, &mountinfo))
}

fn cgroup_directory_from(membership: &str, mountinfo: &str) -> Option<PathBuf> {
    let cgroup_path = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(PathBuf::from)?;
    if !safe_absolute(&cgroup_path) {
        return None;
    }

    for line in mountinfo.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        if fields.get(separator + 1).copied() != Some("cgroup2") {
            continue;
        }
        let (Some(root), Some(mountpoint)) = (fields.get(3), fields.get(4)) else {
            continue;
        };
        let (Some(root), Some(mountpoint)) =
            (decode_mount_path(root), decode_mount_path(mountpoint))
        else {
            continue;
        };
        if !safe_absolute(&root) || !safe_absolute(&mountpoint) {
            continue;
        }

        if !cgroup_path.starts_with(&root) {
            continue;
        }
        let relative = cgroup_path.strip_prefix(&root).ok()?;
        return Some(mountpoint.join(relative));
    }
    None
}

fn decode_mount_path(encoded: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..index + 4]
                .iter()
                .all(|digit| (b'0'..=b'7').contains(digit))
        {
            let value = u16::from(bytes[index + 1] - b'0') * 64
                + u16::from(bytes[index + 2] - b'0') * 8
                + u16::from(bytes[index + 3] - b'0');
            if value > u16::from(u8::MAX) {
                return None;
            }
            decoded.push(value as u8);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
}

fn safe_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| !matches!(component, Component::ParentDir))
}

fn read_populated(file: &File) -> io::Result<bool> {
    let mut events = [0u8; 256];
    let bytes = loop {
        match file.read_at(&mut events, 0) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(bytes) => break bytes,
            Err(error) => return Err(error),
        }
    };
    let text = std::str::from_utf8(&events[..bytes]).map_err(io::Error::other)?;
    parse_populated(text)
}

fn wait_for_empty(events: &File, deadline: Instant) -> io::Result<()> {
    let mut watch = libc::pollfd {
        fd: events.as_raw_fd(),
        events: libc::POLLPRI,
        revents: 0,
    };
    loop {
        match read_populated(events) {
            Ok(false) => return Ok(()),
            // The service guardian can remove a group after draining it.
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENODEV | libc::ENOENT)) => {
                return Ok(());
            }
            Err(error) => return Err(error),
            Ok(true) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "cgroup remained populated after cgroup.kill",
            ));
        }
        let milliseconds = remaining
            .as_micros()
            .div_ceil(1000)
            .clamp(1, i32::MAX as u128) as i32;
        // SAFETY: watch is a writable pollfd for the live, borrowed events file.
        if unsafe { libc::poll(&mut watch, 1, milliseconds) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

fn unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::ReadOnlyFilesystem
            | io::ErrorKind::Unsupported
    )
}

fn parse_populated(events: &str) -> io::Result<bool> {
    let value = events
        .lines()
        .find_map(|line| line.strip_prefix("populated "))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing cgroup populated state")
        })?;
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cgroup populated state",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_current_cgroup_into_the_visible_v2_mount() {
        let membership = "2:cpu:/legacy\n0::/user.slice/user-1000.slice/session.scope\n";
        let mountinfo = "29 23 0:26 / /sys/fs/cgroup rw,nosuid,nodev,noexec - cgroup2 cgroup rw\n";
        assert_eq!(
            cgroup_directory_from(membership, mountinfo),
            Some(PathBuf::from(
                "/sys/fs/cgroup/user.slice/user-1000.slice/session.scope"
            ))
        );
    }

    #[test]
    fn maps_delegated_mount_roots_and_decodes_mountinfo_paths() {
        let membership = "0::/delegated/my group/fliuno\n";
        let mountinfo = "29 23 0:26 /delegated/my\\040group /sys/fs/cgroup\\040delegated rw - cgroup2 cgroup rw\n";
        assert_eq!(
            cgroup_directory_from(membership, mountinfo),
            Some(PathBuf::from("/sys/fs/cgroup delegated/fliuno"))
        );
    }

    #[test]
    fn rejects_membership_outside_a_delegated_mount_root() {
        assert_eq!(
            cgroup_directory_from(
                "0::/other.slice/session.scope\n",
                "29 23 0:26 /delegated.slice /sys/fs/cgroup/delegated rw - cgroup2 cgroup rw\n"
            ),
            None
        );
    }

    #[test]
    fn rejects_missing_or_unsafe_cgroup_membership() {
        assert_eq!(
            cgroup_directory_from("1:name=systemd:/user.slice\n", ""),
            None
        );
        assert_eq!(
            cgroup_directory_from(
                "0::/../../outside\n",
                "29 23 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n"
            ),
            None
        );
    }

    #[test]
    fn reads_only_a_valid_populated_state() {
        assert!(!parse_populated("populated 0\nfrozen 0\n").unwrap());
        assert!(parse_populated("populated 1\nfrozen 0\n").unwrap());
        assert!(parse_populated("frozen 0\n").is_err());
        assert!(parse_populated("populated yes\n").is_err());
    }
}
