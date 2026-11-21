use crate::schema::*;

pub const OUTPUT_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum TerminalRequest {
    Create(CreateTerminalRequest),
    Output(TerminalOutputRequest),
    Wait(WaitForTerminalExitRequest),
    Kill(KillTerminalRequest),
    Release(ReleaseTerminalRequest),
}

impl TerminalRequest {
    pub(crate) fn session_id(&self) -> &SessionId {
        match self {
            Self::Create(r) => &r.session_id,
            Self::Output(r) => &r.session_id,
            Self::Wait(r) => &r.session_id,
            Self::Kill(r) => &r.session_id,
            Self::Release(r) => &r.session_id,
        }
    }

    pub(crate) fn valid(&self) -> bool {
        let id = match self {
            Self::Create(r) => return !r.command.is_empty() && r.command.len() <= 4096,
            Self::Output(r) => &r.terminal_id,
            Self::Wait(r) => &r.terminal_id,
            Self::Kill(r) => &r.terminal_id,
            Self::Release(r) => &r.terminal_id,
        };
        !id.0.is_empty() && id.0.len() <= 128
    }

    pub(crate) fn kind(&self) -> TerminalKind {
        match self {
            Self::Create(_) => TerminalKind::Create,
            Self::Output(_) => TerminalKind::Output,
            Self::Wait(_) => TerminalKind::Wait,
            Self::Kill(_) => TerminalKind::Kill,
            Self::Release(_) => TerminalKind::Release,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalKind {
    Create,
    Output,
    Wait,
    Kill,
    Release,
}

#[derive(Debug)]
pub enum TerminalResponse {
    Created(CreateTerminalResponse),
    Output(TerminalOutputResponse),
    Exited(WaitForTerminalExitResponse),
    Killed(KillTerminalResponse),
    Released(ReleaseTerminalResponse),
}

impl TerminalResponse {
    pub(crate) fn matches(&self, kind: TerminalKind) -> bool {
        match (self, kind) {
            (Self::Created(r), TerminalKind::Create) => {
                !r.terminal_id.0.is_empty() && r.terminal_id.0.len() <= 128
            }
            (Self::Output(r), TerminalKind::Output) => r.output.len() <= OUTPUT_MAX_BYTES,
            (Self::Exited(_), TerminalKind::Wait)
            | (Self::Killed(_), TerminalKind::Kill)
            | (Self::Released(_), TerminalKind::Release) => true,
            _ => false,
        }
    }
}
