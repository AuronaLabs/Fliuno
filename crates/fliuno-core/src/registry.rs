//! Trusted-host tool registration commits only a fully validated batch. Keep
//! adapter setup and connection IO outside this in-memory registry boundary.
use crate::{Error, Kernel, Result, Tool, ToolSpec, authority};
use std::{collections::BTreeMap, sync::Arc};

impl Kernel {
    /// Single-tool registration uses the same checks as a complete adapter.
    /// Registration never executes a tool or grants model/client permissions.
    pub fn register_tool(&mut self, tool: Box<dyn Tool>) -> Result<()> {
        self.register_tools(vec![tool])
    }

    /// Add every tool or leave the registry unchanged. An invalid name,
    /// authority, existing name or duplicate within the batch rejects all of
    /// it. Empty batches are harmless. Implementations remain trusted host code
    /// and must expose an immutable specification and registration identity.
    pub fn register_tools(&mut self, tools: Vec<Box<dyn Tool>>) -> Result<()> {
        let mut staged: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
        for tool in tools {
            let name = tool.spec().name;
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                || self.tools.contains_key(&name)
                || staged.contains_key(&name)
            {
                return Err(Error::Conflict(
                    "invalid or duplicate tool name (use 1..64 ASCII letters, digits, _ or -)"
                        .into(),
                ));
            }
            if !authority::valid(&tool.authority()) {
                return Err(Error::Conflict(
                    "invalid tool registration authority".into(),
                ));
            }
            staged.insert(name, Arc::from(tool));
        }
        // No fallible validation remains after the first registry mutation.
        self.tools.append(&mut staged);
        Ok(())
    }

    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }
}

#[cfg(test)]
mod tests;
