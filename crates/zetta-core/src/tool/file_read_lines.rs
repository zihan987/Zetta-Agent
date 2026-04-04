use anyhow::anyhow;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use super::{Tool, ToolCapability, ToolInvocationError, ToolUseContext};

pub struct FileReadLinesTool;

#[async_trait]
impl Tool for FileReadLinesTool {
    fn name(&self) -> &'static str {
        "file_read_lines"
    }

    fn description(&self) -> &'static str {
        "Reads an inclusive line range from a UTF-8 file inside a readable root."
    }

    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }

    async fn invoke(
        &self,
        input: Value,
        context: &ToolUseContext,
    ) -> Result<Value, ToolInvocationError> {
        let raw_path = input.get("path").and_then(Value::as_str).ok_or_else(|| {
            ToolInvocationError::Failed(anyhow!("file_read_lines requires a string `path`"))
        })?;
        let start_line = input
            .get("start_line")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ToolInvocationError::Failed(anyhow!(
                    "file_read_lines requires an integer `start_line`"
                ))
            })? as usize;
        let end_line = input
            .get("end_line")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ToolInvocationError::Failed(anyhow!(
                    "file_read_lines requires an integer `end_line`"
                ))
            })? as usize;

        if start_line == 0 || end_line == 0 {
            return Err(ToolInvocationError::Failed(anyhow!(
                "file_read_lines expects 1-based line numbers"
            )));
        }
        if end_line < start_line {
            return Err(ToolInvocationError::Failed(anyhow!(
                "file_read_lines requires `end_line >= start_line`"
            )));
        }

        let resolved = context
            .permissions()
            .resolve_read_path(context.cwd(), raw_path)?;
        let content = fs::read_to_string(&resolved)
            .await
            .map_err(anyhow::Error::from)
            .map_err(ToolInvocationError::Failed)?;

        let lines = content.lines().map(ToString::to_string).collect::<Vec<_>>();
        let total_lines = lines.len();
        if total_lines == 0 {
            return Ok(json!({
                "path": resolved,
                "start_line": start_line,
                "end_line": end_line,
                "total_lines": 0,
                "content": "",
                "lines": [],
            }));
        }
        if start_line > total_lines {
            return Err(ToolInvocationError::Failed(anyhow!(
                "file_read_lines start_line {start_line} is past end of file ({total_lines} lines)"
            )));
        }

        let clamped_end = end_line.min(total_lines);
        let selected = lines[(start_line - 1)..clamped_end]
            .iter()
            .enumerate()
            .map(|(offset, text)| {
                json!({
                    "line_number": start_line + offset,
                    "text": text,
                })
            })
            .collect::<Vec<_>>();
        let joined = lines[(start_line - 1)..clamped_end].join("\n");

        Ok(json!({
            "path": resolved,
            "start_line": start_line,
            "end_line": clamped_end,
            "total_lines": total_lines,
            "content": joined,
            "lines": selected,
        }))
    }
}
