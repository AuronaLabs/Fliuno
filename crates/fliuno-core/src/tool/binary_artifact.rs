//! Approval-gated storage of uninterpreted bytes in the task's artifact store.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use fliuno_protocol::MAX_BINARY_ARTIFACT_BYTES;
use serde::Deserialize;

const MAX_ENCODED_BYTES: usize = MAX_BINARY_ARTIFACT_BYTES.div_ceil(3) * 4;

pub(crate) struct CreateBinaryArtifact;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Args<'a> {
    title: &'a str,
    media_type: &'a str,
    content_base64: &'a str,
}

fn parse(value: &Value) -> Result<(Args<'_>, Vec<u8>), String> {
    let args = Args::deserialize(value).map_err(|_| "invalid binary artifact arguments")?;
    if args.title.trim().is_empty()
        || args.title.len() > 256
        || args.title.chars().any(char::is_control)
    {
        return Err("binary artifact title must be 1..256 bytes without control characters".into());
    }
    if args.media_type.len() > 128
        || args.media_type.chars().any(char::is_control)
        || !args.media_type.parse::<mime::Mime>().is_ok_and(|kind| {
            kind.params().next().is_none()
                && kind.type_() != mime::STAR
                && kind.subtype() != mime::STAR
        })
    {
        return Err(
            "binary artifact mediaType must be a concrete type/subtype without parameters".into(),
        );
    }
    if args.content_base64.len() > MAX_ENCODED_BYTES {
        return Err("binary artifact exceeds the 256 KiB byte limit".into());
    }
    let bytes = STANDARD
        .decode(args.content_base64)
        .map_err(|_| "binary artifact requires standard padded base64")?;
    if bytes.len() > MAX_BINARY_ARTIFACT_BYTES {
        return Err("binary artifact exceeds the 256 KiB byte limit".into());
    }
    Ok((args, bytes))
}

impl Tool for CreateBinaryArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "artifact_create_binary".into(),
            description: "Store a binary artifact after approval. Use standard base64 for up to 256 KiB of raw bytes.".into(),
            effect: ToolEffect::ArtifactWrite,
            input_schema: json!({"type":"object", "additionalProperties":false,
                "required":["title","mediaType","contentBase64"], "properties":{
                    "title":{"type":"string","minLength":1,"maxLength":256},
                    "mediaType":{"type":"string","minLength":3,"maxLength":128},
                    "contentBase64":{"type":"string","maxLength":MAX_ENCODED_BYTES}
                }}),
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
        if cancellation.is_cancelled() {
            return Err(ToolError::Failed("tool cancelled before execution".into()));
        }
        let (args, bytes) = parse(&call.arguments)?;
        let id = new_id("artifact");
        Ok(ToolOutput {
            output: json!({"artifactId":id,"contentBytes":bytes.len(),"mediaType":args.media_type}),
            artifacts: vec![Artifact {
                id,
                task_id: context.task_id.into(),
                title: args.title.into(),
                media_type: args.media_type.into(),
                body: ArtifactBody::Binary { bytes },
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_decoded_byte_boundary_and_rejects_ambiguous_inputs() {
        let args = |media: &str, content: String| json!({"title":"result","mediaType":media,"contentBase64":content});
        let maximum = args(
            "application/octet-stream",
            STANDARD.encode(vec![0xff; MAX_BINARY_ARTIFACT_BYTES]),
        );
        assert!(CreateBinaryArtifact.validate(&maximum).is_ok());
        let over = args(
            "application/octet-stream",
            STANDARD.encode(vec![0xff; MAX_BINARY_ARTIFACT_BYTES + 1]),
        );
        assert_eq!(
            over["contentBase64"].as_str().unwrap().len(),
            MAX_ENCODED_BYTES
        );
        assert!(CreateBinaryArtifact.validate(&over).is_err());
        for invalid in [
            args("application/octet-stream", "//".into()),
            args("application/octet-stream", "_w==".into()),
            args("image/*", "/w==".into()),
            args("text/plain; charset=utf-8", "/w==".into()),
            json!({"title":"result","mediaType":"image/png","contentBase64":"","path":"host.txt"}),
        ] {
            assert!(CreateBinaryArtifact.validate(&invalid).is_err());
        }
    }
}
