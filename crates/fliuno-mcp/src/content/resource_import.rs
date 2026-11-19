//! Convert one approved resources/read response to staged local artifacts.
use super::{arguments::valid_uri, authority};
use crate::{Host, Tool, ToolContext, ToolError, ToolOutput, ToolSpec, host::is_complete};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use fliuno_core::{CancellationToken, ToolEffect};
use fliuno_protocol::{Artifact, ArtifactBody, MAX_BINARY_ARTIFACT_BYTES, ToolCall};
use rmcp::model::{ReadResourceRequest, ReadResourceRequestParams, ResourceContents, ServerResult};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

const METHOD: &str = "resources/read";
const MAX_TITLE_BYTES: usize = 240;
const MAX_CONTENTS: usize = 16;
const MAX_TOTAL_BYTES: usize = 512 * 1024;
const MAX_ENCODED_BYTES: usize = MAX_BINARY_ARTIFACT_BYTES.div_ceil(3) * 4;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args<'a> {
    uri: &'a str,
    title: &'a str,
}

fn parse(value: &Value) -> Result<Args<'_>, String> {
    let invalid = || "invalid MCP resource import URI or title".to_owned();
    let args = Args::deserialize(value).map_err(|_| invalid())?;
    if !valid_uri(args.uri)
        || args.title.trim().is_empty()
        || args.title.len() > MAX_TITLE_BYTES
        || args.title.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    Ok(args)
}

pub(super) fn tool(host: Arc<Host>, server: &str) -> Box<dyn Tool> {
    Box::new(ResourceImport {
        host,
        server: server.into(),
    })
}

struct ResourceImport {
    host: Arc<Host>,
    server: String,
}

impl Tool for ResourceImport {
    fn authority(&self) -> String {
        authority(&self.host, &self.spec(), METHOD)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: format!("mcp_{}_resources_import", self.server),
            description: "Read one resource from the configured MCP server and save the returned text and binary content as local task artifacts. Returns artifact references and metadata, not the body.".into(),
            effect: ToolEffect::External,
            input_schema: json!({"type":"object","additionalProperties":false,"required":["uri","title"],
                "properties":{"uri":{"type":"string","minLength":1,"maxLength":4096},
                    "title":{"type":"string","minLength":1,"maxLength":MAX_TITLE_BYTES}}}),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        parse(arguments).map(|_| ())
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args = parse(&call.arguments)?;
        let request = ReadResourceRequest::new(ReadResourceRequestParams::new(args.uri));
        let contents = self.host.request_read(cancellation, move |service| {
            Box::pin(async move {
                match service.send_request(request.into()).await? {
                    ServerResult::ReadResourceResult(response)
                        if is_complete(&response.result_type) =>
                    {
                        Ok(Some(response.contents))
                    }
                    _ => Ok(None),
                }
            })
        })?;
        project(context.task_id, &self.server, args, contents)
    }
}

fn project(
    task_id: &str,
    server: &str,
    args: Args<'_>,
    contents: Vec<ResourceContents>,
) -> Result<ToolOutput, ToolError> {
    let count = contents.len();
    if !(1..=MAX_CONTENTS).contains(&count) {
        return Err("MCP resource import requires 1 to 16 contents".into());
    }
    let mut artifacts = Vec::with_capacity(count);
    let mut references = Vec::with_capacity(count);
    let mut total_bytes = 0;
    for (index, part) in contents.into_iter().enumerate() {
        let (uri, media_type, body, binary) = match part {
            ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                ..
            } => (
                uri,
                media_type_or(mime_type, "text/plain"),
                ArtifactBody::Text { content: text },
                false,
            ),
            ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                ..
            } => {
                if blob.len() > MAX_ENCODED_BYTES {
                    return Err("MCP resource import exceeds the per-content byte limit".into());
                }
                let bytes = STANDARD
                    .decode(blob)
                    .map_err(|_| "MCP resource blob is not standard base64")?;
                (
                    uri,
                    media_type_or(mime_type, "application/octet-stream"),
                    ArtifactBody::Binary { bytes },
                    true,
                )
            }
            _ => return Err("unsupported MCP resource content".into()),
        };
        if !valid_uri(&uri) {
            return Err("MCP resource import returned an invalid URI".into());
        }
        let content_bytes = body.as_bytes().len();
        total_bytes += content_bytes;
        if content_bytes > MAX_BINARY_ARTIFACT_BYTES || total_bytes > MAX_TOTAL_BYTES {
            return Err("MCP resource import exceeds its byte budget".into());
        }
        let id = format!("artifact_{}", Uuid::new_v4());
        let title = if count == 1 {
            args.title.to_owned()
        } else {
            format!("{} ({}/{count})", args.title, index + 1)
        };
        references.push(json!({"uri":uri,"artifactId":id,"mediaType":media_type,"contentBytes":content_bytes,"binary":binary}));
        artifacts.push(Artifact {
            id,
            task_id: task_id.into(),
            title,
            media_type,
            body,
        });
    }
    Ok(ToolOutput {
        output: json!({"serverId":server,"method":METHOD,"result":{"uri":args.uri,"contents":references},"grantsPermissions":false}),
        artifacts,
    })
}

fn media_type_or(media_type: Option<String>, fallback: &str) -> String {
    media_type
        .filter(|value| {
            value.len() <= 128
                && !value.chars().any(char::is_control)
                && value
                    .parse::<mime::Mime>()
                    .is_ok_and(|kind| kind.type_() != mime::STAR && kind.subtype() != mime::STAR)
        })
        .unwrap_or_else(|| fallback.into())
}
