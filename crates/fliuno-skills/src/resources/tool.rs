use super::*;
use fliuno_core::{
    CancellationToken, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use fliuno_protocol::ToolCall;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn tools(catalog: Arc<SkillCatalog>) -> Vec<Box<dyn Tool>> {
    [false, true]
        .into_iter()
        .map(|read| {
            Box::new(ResourceTool {
                catalog: Arc::clone(&catalog),
                read,
            }) as Box<dyn Tool>
        })
        .collect()
}
struct ResourceTool {
    catalog: Arc<SkillCatalog>,
    read: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    name: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    encoding: Encoding,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Encoding {
    #[default]
    Utf8,
    Base64,
}
impl ResourceTool {
    fn arguments(&self, value: &Value) -> Result<Arguments, String> {
        let args: Arguments = serde_json::from_value(value.clone())
            .map_err(|_| "invalid skill resource arguments")?;
        if !self.catalog.contains(&args.name) {
            return Err("unknown skill".into());
        }
        if !self.read && value.get("encoding").is_some() {
            return Err("skill resource listing does not accept encoding".into());
        }
        validate_path(&args.path, !self.read)?;
        Ok(args)
    }
}
impl Tool for ResourceTool {
    fn authority(&self) -> String {
        crate::tools::authority(&self.catalog, self.spec())
    }
    fn spec(&self) -> ToolSpec {
        let mut properties =
            json!({"name":{"type":"string"},"path":{"type":"string","maxLength":512}});
        if self.read {
            properties["encoding"] =
                json!({"type":"string","enum":["utf8","base64"],"default":"utf8"});
        }
        ToolSpec {
            name: if self.read { "skills_read_resource" } else { "skills_list_resources" }.into(),
            description: if self.read {
                "Read up to 256 KiB from one configured skill's relative resource path. Default encoding utf8 returns text; explicitly choose base64 for opaque binary assets. Returns raw-byte length and SHA-256. No decoding, execution, permissions or artifact creation. Links and paths outside this skill are rejected."
            } else {
                "List one configured skill's resource directory on demand, without loading contents or following links. Omit path for the skill root; at most 256 directory entries are scanned. Does not execute scripts or grant permissions."
            }.into(),
            effect: ToolEffect::SkillRead,
            input_schema: json!({"type":"object","additionalProperties":false,
                "required":if self.read { vec!["name","path"] } else { vec!["name"] },
                "properties":properties}),
        }
    }
    fn validate(&self, args: &Value) -> Result<(), String> {
        self.arguments(args).map(|_| ())
    }
    fn execute(
        &self,
        _: &ToolContext<'_>,
        call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancellation.is_cancelled() {
            return Err("skill resource read cancelled before execution".into());
        }
        let args = self.arguments(&call.arguments)?;
        let mut output = if self.read {
            match args.encoding {
                Encoding::Utf8 => {
                    serde_json::to_value(self.catalog.read_resource(&args.name, &args.path)?)
                }
                Encoding::Base64 => {
                    serde_json::to_value(self.catalog.read_asset(&args.name, &args.path)?)
                }
            }
            .map_err(|_| "cannot encode skill resource")?
        } else {
            json!({"path":args.path,"entries":self.catalog.list_resources(&args.name, &args.path)?})
        };
        output["name"] = json!(args.name);
        output["grantsPermissions"] = json!(false);
        Ok(ToolOutput {
            output,
            artifacts: vec![],
        })
    }
}
