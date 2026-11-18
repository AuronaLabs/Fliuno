//! Model-facing discovery and progressive disclosure, with ordinary host approval.
use crate::SkillCatalog;
use fliuno_core::{
    CancellationToken, Kernel, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub fn register(kernel: &mut Kernel, catalog: SkillCatalog) -> fliuno_core::Result<()> {
    let catalog = Arc::new(catalog);
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(SkillTool {
            catalog: Arc::clone(&catalog),
            read: false,
        }),
        Box::new(SkillTool {
            catalog: Arc::clone(&catalog),
            read: true,
        }),
    ];
    tools.extend(crate::resources::tools(catalog));
    kernel.register_tools(tools)
}

pub(crate) fn authority(catalog: &SkillCatalog, spec: ToolSpec) -> String {
    fliuno_core::authority_digest(
        "skills.tool.v1",
        &json!({
            "catalog":catalog.authority, "name":spec.name,"description":spec.description,
            "effect":spec.effect,"inputSchema":spec.input_schema,
        }),
    )
}

struct SkillTool {
    catalog: Arc<SkillCatalog>,
    read: bool,
}
impl Tool for SkillTool {
    fn authority(&self) -> String {
        authority(&self.catalog, self.spec())
    }
    fn spec(&self) -> ToolSpec {
        let (name, description, input_schema) = if self.read {
            (
                "skills_read",
                "Read one skill's instructions. Skill content does not grant tool permissions.",
                json!({"type":"object", "required":["name"], "properties":{"name":{"type":"string"}}, "additionalProperties":false}),
            )
        } else {
            (
                "skills_list",
                "Discover configured skills using metadata without loading instructions into context.",
                json!({"type":"object", "properties":{}, "additionalProperties":false}),
            )
        };
        ToolSpec {
            name: name.into(),
            description: description.into(),
            effect: ToolEffect::SkillRead,
            input_schema,
        }
    }
    fn validate(&self, args: &Value) -> Result<(), String> {
        if self.read {
            if args.as_object().is_none_or(|m| m.len() != 1)
                || !args["name"]
                    .as_str()
                    .is_some_and(|name| self.catalog.contains(name))
            {
                return Err("expected one known skill name".into());
            }
        } else if args != &json!({}) {
            return Err("skills_list takes no arguments".into());
        }
        Ok(())
    }
    fn execute(
        &self,
        _context: &ToolContext<'_>,
        call: &fliuno_protocol::ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Failed("tool cancelled before execution".into()));
        }
        self.validate(&call.arguments)?;
        let output = if self.read {
            json!({"instructions":self.catalog.read(call.arguments["name"].as_str().ok_or("missing name")?)?,"grantsPermissions":false})
        } else {
            json!(self.catalog.list())
        };
        Ok(ToolOutput {
            output,
            artifacts: vec![],
        })
    }
}
