//! Maps a private virtual cwd onto host-owned workspace capabilities. Actual IO
//! runs on a bounded set of blocking jobs after durable consent/start records.
use fliuno_acp::{Client, HostRequest, HostResponse, schema::SessionId};
use fliuno_core::{
    AgentObserver, AgentToolObservation, CancellationToken, PreparedToolExecution, ToolError,
    ToolOutput,
};
use fliuno_protocol::ToolCall;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tokio::task::JoinSet;

const MAX_OUTSTANDING: usize = 8;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Preparing,
    AwaitingApproval,
    Running,
}
struct Entry {
    stage: Stage,
    read: bool,
}

pub(crate) enum Job {
    Prepared {
        id: String,
        expires_at: u64,
        result: Result<fliuno_tools::WriteProposal, ToolError>,
    },
    Executed {
        id: String,
        result: Result<ToolOutput, ToolError>,
    },
}
pub(crate) enum RequestError {
    Rejected,
    Fatal,
}

pub(crate) struct WorkspaceBroker {
    enabled: bool,
    root: PathBuf,
    virtual_root: PathBuf,
    entries: HashMap<String, Entry>,
    pub jobs: JoinSet<Job>,
    cancellation: CancellationToken,
}

impl WorkspaceBroker {
    pub fn new(
        enabled: bool,
        root: PathBuf,
        virtual_root: PathBuf,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            enabled,
            root,
            virtual_root,
            entries: HashMap::new(),
            jobs: JoinSet::new(),
            cancellation,
        }
    }

    fn relative(&self, path: &Path) -> Result<String, ()> {
        if !self.enabled || !path.is_absolute() {
            return Err(());
        }
        let relative = path.strip_prefix(&self.virtual_root).map_err(|_| ())?;
        if !relative
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            return Err(());
        }
        let path = relative.to_str().ok_or(())?.replace('\\', "/");
        if path.is_empty() {
            return Err(());
        }
        Ok(path)
    }

    pub fn request(
        &mut self,
        id: String,
        request: HostRequest,
        expires_at: u64,
        observer: &dyn AgentObserver,
    ) -> Result<(), RequestError> {
        if self.entries.len() >= MAX_OUTSTANDING || self.jobs.len() >= MAX_OUTSTANDING {
            return Err(RequestError::Rejected);
        }
        match request {
            HostRequest::Read(request) => {
                let path = self
                    .relative(&request.path)
                    .map_err(|_| RequestError::Rejected)?;
                let mut arguments = json!({"path":path});
                if let Some(line) = request.line {
                    arguments["line"] = json!(line);
                }
                if let Some(limit) = request.limit {
                    arguments["limit"] = json!(limit);
                }
                let call = ToolCall {
                    id: id.clone(),
                    name: "workspace_read_text".into(),
                    arguments,
                };
                observer
                    .observe_tool(AgentToolObservation::Requested {
                        request_id: id.clone(),
                        call,
                        preview: Value::Null,
                        expires_at_unix_ms: expires_at,
                    })
                    .map_err(|_| RequestError::Fatal)?;
                self.entries.insert(
                    id,
                    Entry {
                        stage: Stage::AwaitingApproval,
                        read: true,
                    },
                );
            }
            HostRequest::Write(request) => {
                let path = self
                    .relative(&request.path)
                    .map_err(|_| RequestError::Rejected)?;
                let root = self.root.clone();
                let cancel = self.cancellation.clone();
                self.entries.insert(
                    id.clone(),
                    Entry {
                        stage: Stage::Preparing,
                        read: false,
                    },
                );
                self.jobs.spawn(async move {
                    let call_id = id.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        fliuno_tools::prepare_workspace_write(
                            &root,
                            &call_id,
                            &path,
                            &request.content,
                            &cancel,
                        )
                    })
                    .await
                    .unwrap_or_else(|_| Err(ToolError::Failed("proposal worker failed".into())));
                    Job::Prepared {
                        id,
                        expires_at,
                        result,
                    }
                });
            }
        }
        Ok(())
    }

    pub fn command(
        &mut self,
        id: String,
        execution: Option<Box<PreparedToolExecution>>,
        observer: &dyn AgentObserver,
        client: &Client,
        session: &SessionId,
    ) -> Result<(), ()> {
        let Some(entry) = self.entries.get_mut(&id) else {
            return Err(());
        };
        if entry.stage != Stage::AwaitingApproval || self.cancellation.is_cancelled() {
            return Err(());
        }
        if let Some(execution) = execution {
            if self.jobs.len() >= MAX_OUTSTANDING || execution.call().id != id {
                return Err(());
            }
            // Anchor the deadline before persistence. A slow observer must not
            // turn the remaining request budget into a fresh execution budget.
            let checked_at = std::time::Instant::now();
            let remaining = client
                .host_request_remaining(session, &id)
                .map_err(|_| ())?;
            let deadline = checked_at + remaining;
            observer
                .observe_tool(AgentToolObservation::Started {
                    request_id: id.clone(),
                })
                .map_err(|_| ())?;
            entry.stage = Stage::Running;
            let cancel = self.cancellation.child_token();
            self.jobs.spawn(async move {
                let _cancel_guard = cancel.clone().drop_guard();
                let execution = tokio::task::spawn_blocking(move || {
                    if std::time::Instant::now() >= deadline {
                        return Err(ToolError::Failed(
                            "host request expired before execution".into(),
                        ));
                    }
                    execution.execute(&cancel)
                });
                let result = match tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    execution,
                )
                .await
                {
                    Ok(Ok(result)) => result,
                    _ => Err(ToolError::OutcomeUnknown(
                        "host execution timed out or worker failed".into(),
                    )),
                };
                Job::Executed { id, result }
            });
        } else {
            client
                .respond_host_request(session, &id, HostResponse::Denied)
                .map_err(|_| ())?;
            self.entries.remove(&id);
        }
        Ok(())
    }

    pub fn completed(
        &mut self,
        job: Job,
        observer: &dyn AgentObserver,
        client: &Client,
        session: &SessionId,
    ) -> Result<(), ()> {
        match job {
            Job::Prepared {
                id,
                expires_at,
                result,
            } => {
                let Some(entry) = self.entries.get_mut(&id) else {
                    return Ok(());
                };
                if self.cancellation.is_cancelled() {
                    return Err(());
                }
                match result {
                    Ok(proposal) => {
                        observer
                            .observe_tool(AgentToolObservation::Requested {
                                request_id: id,
                                call: proposal.call,
                                preview: proposal.preview,
                                expires_at_unix_ms: expires_at,
                            })
                            .map_err(|_| ())?;
                        entry.stage = Stage::AwaitingApproval;
                    }
                    Err(_) => {
                        // Reject preparation without leaking host paths/error data.
                        client
                            .respond_host_request(session, &id, HostResponse::Failed)
                            .map_err(|_| ())?;
                        self.entries.remove(&id);
                    }
                }
            }
            Job::Executed { id, result } => {
                let entry = self.entries.remove(&id).ok_or(())?;
                let response = match &result {
                    Ok(output) if entry.read => HostResponse::Read(
                        output
                            .output
                            .get("content")
                            .and_then(Value::as_str)
                            .ok_or(())?
                            .into(),
                    ),
                    Ok(_) => HostResponse::Written,
                    Err(_) => HostResponse::Failed,
                };
                // A stored result is required before the peer receives file data
                // or a success acknowledgement. Unknown effects close the run.
                observer
                    .observe_tool(AgentToolObservation::Completed {
                        request_id: id.clone(),
                        result,
                    })
                    .map_err(|_| ())?;
                client
                    .respond_host_request(session, &id, response)
                    .map_err(|_| ())?;
            }
        }
        Ok(())
    }

    pub fn closed(&mut self, id: String, observer: &dyn AgentObserver) -> Result<(), ()> {
        let Some(entry) = self.entries.remove(&id) else {
            return Ok(());
        };
        observer
            .observe_tool(AgentToolObservation::Closed { request_id: id })
            .map_err(|_| ())?;
        if entry.stage == Stage::Running {
            return Err(());
        }
        Ok(())
    }
}
