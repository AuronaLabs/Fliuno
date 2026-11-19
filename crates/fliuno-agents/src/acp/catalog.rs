//! Query a catalog on a fresh ACP connection without opening a session.
use super::*;
use fliuno_acp::Error;
use fliuno_protocol::{AgentSessionDescriptor, AgentSessionListFailure, AgentSessionPage};

const MAX_READ_TIME: Duration = Duration::from_secs(15);

impl AcpBackend {
    pub(super) fn read_catalog(
        &self,
        cursor: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<AgentSessionPage, AgentSessionListFailure> {
        use AgentSessionListFailure::{InvalidPage, Rejected, Unavailable, Unsupported};
        let Some(cwd) = self.trusted_cwd.as_deref().filter(|_| self.session_catalog) else {
            return Err(Unsupported);
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Unavailable)?;
        runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + MAX_READ_TIME;
            let (client, _events) = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(Unavailable),
                connection = tokio::time::timeout_at(deadline, self.connect()) => {
                    connection.map_err(|_| Unavailable)?.map_err(|_| Unavailable)?
                }
            };
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(Unavailable),
                page = tokio::time::timeout_at(deadline, client.list_sessions(Some(cwd), cursor)) => {
                    page.map_err(|_| Unavailable).and_then(|result| result.map_err(|error| match error {
                        Error::Unsupported(_) => Unsupported,
                        Error::Remote(_) => Rejected,
                        Error::Protocol(_) => InvalidPage,
                        _ => Unavailable,
                    }))
                }
            };
            client.close().await;
            let page = result?;
            let sessions = page.sessions.into_iter().map(|session| {
                Ok(AgentSessionDescriptor {
                    session_id: session.session_id.0.to_string(),
                    cwd: session.cwd.into_os_string().into_string().map_err(|_| InvalidPage)?,
                    additional_directories: session.additional_directories.into_iter().map(|path| {
                        path.into_os_string().into_string().map_err(|_| InvalidPage)
                    }).collect::<Result<_, _>>()?,
                    title: session.title,
                    updated_at: session.updated_at,
                })
            }).collect::<Result<_, _>>()?;
            Ok(AgentSessionPage { sessions, next_cursor: page.next_cursor })
        })
    }
}
