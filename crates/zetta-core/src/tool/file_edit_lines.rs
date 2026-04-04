use anyhow::anyhow;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use super::{Tool, ToolCapability, ToolInvocationError, ToolUseContext};

const PREVIEW_LINE_LIMIT: usize = 8;

pub struct FileEditLinesTool;

#[async_trait]
impl Tool for FileEditLinesTool {
    fn name(&self) -> &'static str {
        "file_edit_lines"
    }

    fn description(&self) -> &'static str {
        "Replaces an inclusive line range in a UTF-8 file inside a writable root."
    }

    fn capability(&self) -> ToolCapability {
        ToolCapability::Write
    }

    async fn invoke(
        &self,
        input: Value,
        context: &ToolUseContext,
    ) -> Result<Value, ToolInvocationError> {
        let raw_path = input.get("path").and_then(Value::as_str).ok_or_else(|| {
            ToolInvocationError::Failed(anyhow!("file_edit_lines requires a string `path`"))
        })?;
        let start_line = input
            .get("start_line")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ToolInvocationError::Failed(anyhow!(
                    "file_edit_lines requires an integer `start_line`"
                ))
            })? as usize;
        let end_line = input
            .get("end_line")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ToolInvocationError::Failed(anyhow!(
                    "file_edit_lines requires an integer `end_line`"
                ))
            })? as usize;
        let new_text = input
            .get("new_text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolInvocationError::Failed(anyhow!("file_edit_lines requires a string `new_text`"))
            })?;

        if start_line == 0 || end_line == 0 {
            return Err(ToolInvocationError::Failed(anyhow!(
                "file_edit_lines expects 1-based line numbers"
            )));
        }
        if end_line < start_line {
            return Err(ToolInvocationError::Failed(anyhow!(
                "file_edit_lines requires `end_line >= start_line`"
            )));
        }

        let resolved = context
            .permissions()
            .resolve_write_path(context.cwd(), raw_path)?;
        let content = fs::read_to_string(&resolved)
            .await
            .map_err(anyhow::Error::from)
            .map_err(ToolInvocationError::Failed)?;
        let had_trailing_newline = content.ends_with('\n');
        let mut lines = content.lines().map(ToString::to_string).collect::<Vec<_>>();
        let total_lines = lines.len();

        if total_lines == 0 {
            return Err(ToolInvocationError::Failed(anyhow!(
                "file_edit_lines cannot edit an empty file"
            )));
        }
        if start_line > total_lines {
            return Err(ToolInvocationError::Failed(anyhow!(
                "file_edit_lines start_line {start_line} is past end of file ({total_lines} lines)"
            )));
        }

        let clamped_end = end_line.min(total_lines);
        let replacement_lines = if new_text.is_empty() {
            Vec::new()
        } else {
            new_text
                .lines()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        let replaced_lines = lines[(start_line - 1)..clamped_end].to_vec();
        let inserted_lines = replacement_lines.len();
        let operation = if replacement_lines.is_empty() {
            "delete"
        } else if replaced_lines.is_empty() {
            "insert"
        } else {
            "replace"
        };

        lines.splice((start_line - 1)..clamped_end, replacement_lines.clone());

        let mut updated = lines.join("\n");
        if had_trailing_newline && !updated.is_empty() {
            updated.push('\n');
        }

        fs::write(&resolved, updated.as_bytes())
            .await
            .map_err(anyhow::Error::from)
            .map_err(ToolInvocationError::Failed)?;

        Ok(json!({
            "path": resolved,
            "start_line": start_line,
            "end_line": clamped_end,
            "replaced_line_count": clamped_end - start_line + 1,
            "inserted_line_count": inserted_lines,
            "byte_count": updated.len(),
            "operation": operation,
            "before_preview": preview_lines(&replaced_lines, PREVIEW_LINE_LIMIT),
            "after_preview": preview_lines(&replacement_lines, PREVIEW_LINE_LIMIT),
        }))
    }
}

fn preview_lines(lines: &[String], limit: usize) -> String {
    if lines.is_empty() {
        return String::new();
    }

    let mut selected = lines.iter().take(limit).cloned().collect::<Vec<_>>();
    if lines.len() > limit {
        selected.push("...".to_string());
    }
    selected.join("\n")
}
