//! Bound discovery while fetching pages, rather than after SDK accumulation.
use crate::host::McpService;
use rmcp::model::PaginatedRequestParams;
use std::collections::BTreeSet;

pub(crate) async fn tools(service: &McpService) -> Result<Vec<rmcp::model::Tool>, String> {
    let mut tools = Vec::new();
    let mut cursor = None;
    let mut seen = BTreeSet::new();
    let mut bytes = 0;
    for _ in 0..32 {
        let page = service
            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await
            .map_err(|_| "MCP tool discovery failed")?;
        if !crate::host::is_complete(&page.result_type) {
            return Err("MCP discovery returned an unsupported result type".into());
        }
        bytes += fliuno_core::json_size(&page).map_err(|_| "cannot encode MCP discovery page")?;
        if tools.len() + page.tools.len() > 256 || bytes > 4 * 1024 * 1024 {
            return Err("MCP discovery exceeds 256 tools or 4 MiB".into());
        }
        tools.extend(page.tools);
        let Some(next) = page.next_cursor else {
            return Ok(tools);
        };
        if next.is_empty() || next.len() > 4096 || !seen.insert(next.clone()) {
            return Err("MCP discovery returned an invalid or repeated cursor".into());
        }
        cursor = Some(next);
    }
    Err("MCP discovery exceeds 32 pages".into())
}
