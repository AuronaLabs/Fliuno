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
        JobObjects::{
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, TerminateJobObject,
        },
        Threading::{GetExitCodeProcess, WaitForSingleObject},
    },
};

pub(crate) struct JobProcess {
    pub(super) process: OwnedHandle,
    pub(super) job: OwnedHandle,
    pub(super) pid: u32,
}
impl JobProcess {
    pub(crate) fn containment(&self) -> crate::ProcessContainment {
        let limits = self.read_limits().map(|limits| {
            let basic = limits.BasicLimitInformation;
            crate::ResourceLimits {
                memory_max_bytes: (basic.LimitFlags & JOB_OBJECT_LIMIT_JOB_MEMORY != 0)
                    .then_some(limits.JobMemoryLimit as u64),
                pids_max: (basic.LimitFlags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS != 0)
                    .then_some(basic.ActiveProcessLimit),
                cpu_quota_micros: None,
            }
        });
        crate::ProcessContainment {
            tree_backend: crate::TreeBackend::WindowsJob,
            limits,
        }
    }

    fn read_limits(&self) -> Option<JOBOBJECT_EXTENDED_LIMIT_INFORMATION> {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: the live Job's information class matches the initialized output buffer.
        let ok = unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(limits)
    }

    pub(crate) fn usage(&self) -> crate::ProcessUsage {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: the live Job's information class matches the initialized output buffer.
        let has_accounting = unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            ) != 0
        };
        let cpu_ms = has_accounting
            .then(|| {
                let units = accounting
                    .TotalKernelTime
                    .saturating_add(accounting.TotalUserTime);
                u64::try_from(units).ok().map(|units| units / 10_000)
            })
            .flatten();
        crate::ProcessUsage {
            cpu_ms,
            peak_memory_bytes: self
                .read_limits()
                .map(|limits| limits.PeakJobMemoryUsed as u64),
        }
    }

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

    pub(crate) fn wait_for_empty_job(&self) -> io::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: the handle is owned and the output buffer matches the information class.
            check(unsafe {
                QueryInformationJobObject(
                    self.job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            })?;
            if accounting.ActiveProcesses == 0 {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned Job did not drain",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
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
