//! SDK process fixture for peers that expose resources/prompts without tools.
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        GetPromptRequestParams, GetPromptResponse, GetPromptResult, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, ServerCapabilities,
        ServerConfig,
    },
    service::RequestContext,
};
use serde_json::json;

struct Fixture;
impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
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
}
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    Fixture
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
