//! SDK process fixture for peers that expose resources/prompts without tools.
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CompleteRequestParams, CompleteResult, CompletionInfo, GetPromptRequestParams,
        GetPromptResponse, GetPromptResult, ListPromptsResult, ListResourceTemplatesResult,
        ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResponse, ReadResourceResult, ResourceUpdatedNotification,
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerConfig, ServerNotification,
        SubscriptionFilter,
    },
    service::{RequestContext, SubscriptionContext},
};
use serde_json::json;

struct Fixture;
impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_prompts()
                .enable_completions()
                .build(),
        )
    }
    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }
    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        let uris = context.accepted().resource_subscriptions.as_deref();
        if uris.is_some_and(|uris| uris.iter().any(|uri| uri == "fixture:environment")) {
            context
                .sink()
                .send(ServerNotification::ResourceUpdatedNotification(
                    ResourceUpdatedNotification::new(ResourceUpdatedNotificationParam::new(
                        "fixture:environment",
                    )),
                ))
                .await
                .map_err(|_| ErrorData::internal_error("watch delivery failed", None))?;
        }
        context.cancelled().await;
        Ok(())
    }
    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(serde_json::from_value(
            json!({"resources":[{"name":"environment","uri":"fixture:environment"}]}),
        )
        .unwrap())
    }
    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(serde_json::from_value(
            json!({"resourceTemplates":[{"name":"report","uriTemplate":"fixture:report/{id}"}]}),
        )
        .unwrap())
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let report = json!({"pid":std::process::id(),"cwd":std::env::current_dir().unwrap(),
            "container":fliuno_sandbox::current_process_is_app_container().unwrap_or(false)});
        Ok(
            serde_json::from_value::<ReadResourceResult>(json!({"contents":[{
                "uri":request.uri,"mimeType":"application/json","text":report.to_string()
            }]}))
            .unwrap()
            .into(),
        )
    }
    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(serde_json::from_value(json!({"prompts":[{"name":"draft"}]})).unwrap())
    }
    async fn get_prompt(
        &self,
        _: GetPromptRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        Ok(
            serde_json::from_value::<GetPromptResult>(json!({"messages":[{
                "role":"user","content":{"type":"text","text":"PROCESS_PROMPT_DATA"}
            }]}))
            .unwrap()
            .into(),
        )
    }
    async fn complete(
        &self,
        request: CompleteRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        assert_eq!(
            request.r#ref,
            rmcp::model::Reference::for_resource("fixture:report/{id}")
        );
        assert_eq!(request.argument.name, "id");
        Ok(CompleteResult::new(
            CompletionInfo::with_all_values(vec!["report-1".into()]).unwrap(),
        ))
    }
}
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    Fixture
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
