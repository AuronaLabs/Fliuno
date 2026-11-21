use crate::tasks::legacy_input::Bridge;
use rmcp::{
    ClientHandler, ErrorData, RoleClient,
    model::{
        ClientConfig, ElicitRequestParams, ElicitResult, ProtocolVersion,
        ResourceUpdatedNotificationParam,
    },
    service::{NotificationContext, RequestContext},
};
use tokio::sync::broadcast;
pub(crate) struct Client {
    pub config: ClientConfig,
    pub legacy_input: Bridge,
    pub legacy_resource_updates: broadcast::Sender<String>,
}

impl Client {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            legacy_input: Default::default(),
            legacy_resource_updates: broadcast::channel(8).0,
        }
    }

    pub fn with_protocol_version(mut self, version: ProtocolVersion) -> Self {
        self.config = self.config.with_protocol_version(version);
        self
    }
}

impl ClientHandler for Client {
    fn get_info(&self) -> ClientConfig {
        self.config.clone()
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _: NotificationContext<RoleClient>,
    ) {
        if params.uri.len() <= 4096 {
            let _ = self.legacy_resource_updates.send(params.uri);
        }
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        self.legacy_input.request_form(request, context).await
    }
}
