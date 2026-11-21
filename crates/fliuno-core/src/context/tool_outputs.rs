use super::*;
use serde_json::Value;
use std::borrow::Cow;

const LARGE_OUTPUT_BYTES: usize = 4 * 1024;
const EXCERPT_BYTES: usize = 512;

fn outputs(turn: &ContextTurn) -> impl Iterator<Item = &Value> {
    turn.messages
        .iter()
        .filter_map(|message| match message {
            ContextMessage::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .chain(turn.host_tools.iter().filter_map(|tool| {
            (tool.status == AgentHostToolStatus::Succeeded)
                .then_some(tool.output.as_ref())
                .flatten()
        }))
}

pub(crate) fn count_large(turns: &[ContextTurn]) -> Result<u32> {
    turns.iter().flat_map(outputs).try_fold(0, |count, output| {
        Ok(count + u32::from(json_size(output)? > LARGE_OUTPUT_BYTES))
    })
}

pub(crate) fn project(turns: &[ContextTurn], enabled: bool) -> Result<Cow<'_, [ContextTurn]>> {
    if !enabled {
        return Ok(Cow::Borrowed(turns));
    }
    let mut projected = turns.to_vec();
    for turn in &mut projected {
        for message in &mut turn.messages {
            if let ContextMessage::ToolResult { call_id, output } = message {
                replace(output, &turn.task_id, call_id)?;
            }
        }
        for tool in &mut turn.host_tools {
            if tool.status == AgentHostToolStatus::Succeeded
                && let Some(output) = &mut tool.output
            {
                replace(output, &turn.task_id, &tool.call.id)?;
            }
        }
    }
    Ok(Cow::Owned(projected))
}

fn replace(output: &mut Value, task_id: &str, call_id: &str) -> Result<()> {
    let original_bytes = json_size(output)?;
    if original_bytes <= LARGE_OUTPUT_BYTES {
        return Ok(());
    }
    let original = serde_json::to_string(output)?;
    let mut end = EXCERPT_BYTES;
    while !original.is_char_boundary(end) {
        end -= 1;
    }
    let reduced = serde_json::json!({
        "$fliuno": "historical_tool_output_excerpt",
        "taskId": task_id,
        "callId": call_id,
        "originalJsonBytes": original_bytes,
        "jsonPrefix": &original[..end],
    });
    if json_size(&reduced)? < original_bytes {
        *output = reduced;
    }
    Ok(())
}
