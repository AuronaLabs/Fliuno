//! Generate parameter and response schemas without a recursively nested JSON macro.
use super::*;

macro_rules! schemas {
    ($($name:literal => $schema:expr),+ $(,)?) => {
        Value::Object([$(($name.into(), $schema),)+].into_iter().collect())
    };
}

fn schema<T: JsonSchema>() -> Value {
    serde_json::json!(schemars::schema_for!(T))
}

/// Named parameter and domain schemas, generated from the actual wire types.
pub fn schema_bundle() -> Value {
    let methods = schemas! {
        "initialize" => schema::<InitializeParams>(),
        "workspace/open" => schema::<WorkspaceOpenParams>(),
        "workspace/get" => schema::<WorkspaceIdParams>(),
        "workspace/list" => schema::<ListParams>(),
        "session/create" => schema::<SessionCreateParams>(),
        "session/get" => schema::<SessionIdParams>(),
        "session/rename" => schema::<SessionRenameParams>(),
        "session/setArchived" => schema::<SessionSetArchivedParams>(),
        "session/list" => schema::<SessionListParams>(),
        "session/context" => schema::<SessionContextParams>(),
        "terminal/list" => schema::<TerminalListParams>(),
        "terminal/get" => schema::<TerminalGetParams>(),
        "terminal/output" => schema::<TerminalOutputParams>(),
        "terminal/tail" => schema::<TerminalTailParams>(),
        "terminal/subscribe" => schema::<TerminalSubscribeParams>(),
        "terminal/unsubscribe" => schema::<TerminalUnsubscribeParams>(),
        "terminal/archive" => schema::<TerminalArchiveParams>(),
        "terminal/archiveBatch" => schema::<TerminalArchiveBatchParams>(),
        "terminal/archives" => schema::<TerminalArchivesParams>(),
        "task/start" => schema::<TaskStartParams>(),
        "task/continueRemote" => schema::<TaskContinueRemoteParams>(),
        "task/continueAgentSession" => schema::<TaskContinueAgentSessionParams>(),
        "task/list" => schema::<TaskListParams>(),
        "task/listSummaries" => schema::<TaskSummaryListParams>(),
        "task/listWorkspaceSummaries" => schema::<WorkspaceTaskSummaryListParams>(),
        "task/get" => schema::<TaskIdParams>(),
        "task/getSummary" => schema::<TaskIdParams>(),
        "task/checkRemote" => schema::<TaskIdParams>(),
        "task/cancel" => schema::<TaskIdParams>(),
        "task/resume" => schema::<TaskResumeParams>(),
        "approval/listPending" => schema::<ApprovalListParams>(),
        "approval/get" => schema::<ApprovalGetParams>(),
        "approval/respond" => schema::<ApprovalRespondParams>(),
        "events/get" => schema::<EventGetParams>(),
        "events/list" => schema::<EventsListParams>(),
        "events/listAll" => schema::<EventsListAllParams>(),
        "events/listSession" => schema::<EventsListSessionParams>(),
        "events/listWorkspace" => schema::<EventsListWorkspaceParams>(),
        "events/setScope" => schema::<EventsSetScopeParams>(),
        "events/resubscribe" => serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        "modelProgress/resubscribe" => serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        "artifacts/list" => schema::<TaskIdParams>(),
        "artifacts/listSummaries" => schema::<ArtifactListParams>(),
        "artifacts/listScopedSummaries" => schema::<ArtifactCatalogParams>(),
        "artifacts/get" => schema::<ArtifactGetParams>(),
        "artifacts/getSummary" => schema::<ArtifactGetParams>(),
        "artifacts/read" => schema::<ArtifactReadParams>(),
        "tools/list" => serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        "agents/list" => serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
    };
    let types = schemas! {
        "Workspace" => schema::<Workspace>(),
        "Session" => schema::<Session>(),
        "Task" => schema::<Task>(),
        "TaskSummary" => schema::<TaskSummary>(),
        "TaskSummariesPage" => schema::<ObjectPage<TaskSummary>>(),
        "TaskContext" => schema::<TaskContext>(),
        "AgentSpec" => schema::<AgentSpec>(),
        "RemoteTaskCheckAccepted" => schema::<RemoteTaskCheckAccepted>(),
        "TerminalSnapshot" => schema::<TerminalSnapshot>(),
        "TerminalOutput" => schema::<TerminalOutput>(),
        "TerminalSubscription" => schema::<TerminalSubscription>(),
        "TerminalOutputNotification" => schema::<TerminalOutputNotification>(),
        "TerminalSubscriptionEnded" => schema::<TerminalSubscriptionEnded>(),
        "TerminalArchive" => schema::<TerminalArchive>(),
        "TerminalArchiveBatch" => schema::<TerminalArchiveBatch>(),
        "TerminalArchivesPage" => schema::<ObjectPage<TerminalArchive>>(),
        "PendingApprovalsPage" => schema::<ObjectPage<ApprovalSummary>>(),
        "ApprovalDetails" => schema::<ApprovalDetails>(),
        "Artifact" => schema::<Artifact>(),
        "ArtifactChunk" => schema::<ArtifactChunk>(),
        "ArtifactSummariesPage" => schema::<ObjectPage<ArtifactSummary>>(),
        "Event" => schema::<Event>(),
        "EventSubscription" => schema::<EventSubscription>(),
        "ModelProgress" => schema::<ModelProgress>(),
        "ModelBudget" => schema::<ModelBudget>(),
        "EventsPage" => schema::<EventsPage>()
    };
    serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "methods": methods,
        "types": types
    })
}
