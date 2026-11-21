//! SDK process fixture for peers that expose resources/prompts without tools.
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CompleteRequestParams, CompleteResult, CompletionInfo, GetPromptRequestParams,
        GetPromptResponse, GetPromptResult, ListPromptsResult, ListResourceTemplatesResult,
        ListResourcesResult, PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams,
        ReadResourceResponse, ReadResourceResult, ResourceUpdatedNotification,
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerConfig, ServerNotification,
        SubscribeRequestParams, SubscriptionFilter, UnsubscribeRequestParams,
    },
    service::{RequestContext, SubscriptionContext},
};
use serde_json::json;

struct Fixture {
    legacy: bool,
    subscription: std::sync::Mutex<Option<String>>,
}
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
        .with_protocol_version(if self.legacy {
            ProtocolVersion::V_2025_11_25
        } else {
            ProtocolVersion::V_2026_07_28
        })
    }
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        assert!(self.legacy);
        if request.uri == "fixture:declined" {
            return Err(ErrorData::invalid_params("not available", None));
        }
        assert!(
            self.subscription
                .lock()
                .unwrap()
                .replace(request.uri.clone())
                .is_none()
        );
        for uri in [
            "fixture:unrelated",
            "fixture:environment",
            "fixture:environment",
        ] {
            if uri == "fixture:unrelated" || request.uri == uri {
                context
                    .peer
                    .notify_resource_updated(ResourceUpdatedNotificationParam::new(uri))
                    .await
                    .map_err(|_| ErrorData::internal_error("watch delivery failed", None))?;
            }
        }
        Ok(())
    }
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        assert_eq!(self.subscription.lock().unwrap().take(), Some(request.uri));
        Ok(())
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
pub async fn run(legacy: bool) -> Result<(), Box<dyn std::error::Error>> {
    Fixture {
        legacy,
        subscription: Default::default(),
    }
    .serve(rmcp::transport::stdio())
    .await?
    .waiting()
    .await?;
    Ok(())
}
