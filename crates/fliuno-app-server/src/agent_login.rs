//! Login terminal data belongs to the local stdio connection, never the task log.
use super::{Error, RpcError};
use base64::{Engine, engine::general_purpose::STANDARD};
use fliuno_agents::TerminalLogins;
use fliuno_protocol::*;
use fliuno_terminal::login::{Login, Status};
use std::time::Duration;

pub(crate) struct Service {
    pub(crate) registrations: TerminalLogins,
    active: Option<(String, Login)>,
}

impl Service {
    pub(crate) fn new(registrations: TerminalLogins) -> Self {
        Self {
            registrations,
            active: None,
        }
    }

    pub(crate) fn start(
        &mut self,
        request: AgentTerminalLoginStart,
    ) -> Result<AgentTerminalLoginHandle, RpcError> {
        if let Some((_, login)) = &self.active
            && !login.status().finished()
        {
            return Err(Error::Conflict("an ACP terminal login is already open".into()).into());
        }
        let registration = self
            .registrations
            .get(&request.agent_id)
            .ok_or_else(unconfigured)?;
        let login = registration
            .preset
            .start(request.rows, request.cols)
            .map_err(Error::Invalid)?;
        let login_id = uuid::Uuid::new_v4().to_string();
        self.active = Some((login_id.clone(), login));
        Ok(AgentTerminalLoginHandle { login_id })
    }

    fn login(&mut self, id: &str) -> Result<&mut Login, RpcError> {
        self.active
            .as_mut()
            .filter(|(active, _)| active == id)
            .map(|(_, login)| login)
            .ok_or_else(|| Error::NotFound("ACP terminal login".into()).into())
    }

    pub(crate) fn close(&mut self, request: AgentTerminalLoginHandle) -> Result<(), RpcError> {
        self.login(&request.login_id)?.close();
        Ok(())
    }

    pub(crate) fn input(
        &mut self,
        request: AgentTerminalLoginInput,
    ) -> Result<AgentTerminalLoginInputAccepted, RpcError> {
        let next_sequence = self
            .login(&request.login_id)?
            .input(request.sequence, request.data)
            .map_err(Error::Conflict)?;
        Ok(AgentTerminalLoginInputAccepted { next_sequence })
    }

    pub(crate) fn output(
        &mut self,
        request: AgentTerminalLoginOutputParams,
    ) -> Result<AgentTerminalLoginOutput, RpcError> {
        let login = self.login(&request.login_id)?;
        let page = login.output(request.after).map_err(Error::Invalid)?;
        let status = match page.status {
            Status::Starting | Status::Running if login.closing() => {
                AgentTerminalLoginStatus::Closing
            }
            Status::Starting => AgentTerminalLoginStatus::Starting,
            Status::Running => AgentTerminalLoginStatus::Running,
            Status::Closed => AgentTerminalLoginStatus::Closed,
            Status::Exited(exit_code) => AgentTerminalLoginStatus::Exited { exit_code },
            Status::Expired => AgentTerminalLoginStatus::Expired,
            Status::Failed => AgentTerminalLoginStatus::Failed,
            Status::Unknown => AgentTerminalLoginStatus::OutcomeUnknown,
        };
        Ok(AgentTerminalLoginOutput {
            status,
            next: page.next,
            truncated: page.truncated,
            data_base64: STANDARD.encode(page.bytes),
        })
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some((_, login)) = &self.active {
            login.wait_for_close(Duration::from_secs(1));
        }
    }
}

pub(crate) fn unconfigured() -> RpcError {
    Error::Conflict("local ACP terminal login is not configured".into()).into()
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(
    params: serde_json::Value,
) -> Result<T, RpcError> {
    // Keep even malformed credential input out of the JSON-RPC error path.
    serde_json::from_value(params).map_err(|_| RpcError {
        code: -32602,
        message: "invalid ACP terminal login parameters".into(),
    })
}
