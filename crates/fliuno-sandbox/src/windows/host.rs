//! Trusted host policy over the common native launcher; no isolation claims.
use super::{JobProcess, job, launch, stdio::Pipes};
use crate::{HostChild, HostLaunchSpec};
use std::{fs::File, io};

pub(crate) fn spawn(spec: &HostLaunchSpec) -> io::Result<(HostChild, (File, File, File))> {
    let job = job::create_host()?;
    let started = launch::start(
        launch::Spec {
            program: &spec.program,
            args: &spec.args,
            env: &spec.env,
            cwd: Some(&spec.cwd),
            profile: None,
            job: Some(&job),
            new_process_group: false,
        },
        Pipes::create_async("OW")?,
    )?;
    Ok((
        HostChild {
            inner: JobProcess {
                process: started.process,
                job,
                pid: started.pid,
            },
        },
        started.pipes,
    ))
}
