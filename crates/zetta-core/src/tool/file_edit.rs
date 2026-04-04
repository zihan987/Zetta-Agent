use anyhow::anyhow;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use super::{Tool, ToolCapability, ToolInvocationError, ToolUseContext};

const PREVIEW_CHAR_LIMIT: usize = 160;

pub struct FileEditTool;

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &'static str {
        "file_edit"
    }

    fn description(&self) -> &'static str {
        "Edits a UTF-8 file by replacing exact text matches inside a writable root."
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
            ToolInvocationError::Failed(anyhow!("file_edit requires a string `path`"))
        })?;
        let old_text = input
            .get("old_text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolInvocationError::Failed(anyhow!("file_edit requires a string `old_text`"))
            })?;
        let new_text = input
            .get("new_text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolInvocationError::Failed(anyhow!("file_edit requires a string `new_text`"))
            })?;
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if old_text.is_empty() {
            return Err(ToolInvocationError::Failed(anyhow!(
                "file_edit requires `old_text` to be non-empty"
            )));
        }

        let resolved = context
            .permissions()
            .resolve_write_path(context.cwd(), raw_path)?;
        let content = fs::read_to_string(&resolved)
            .await
            .map_err(anyhow::Error::from)
            .map_err(ToolInvocationError::Failed)?;

        let replacement_count = content.match_indices(old_text).count();
        if replacement_count == 0 {
            return Err(ToolInvocationError::Failed(anyhow!(
                "file_edit could not find `old_text` in target file"
            )));
        }
        if !replace_all && replacement_count != 1 {
            return Err(ToolInvocationError::Failed(anyhow!(
                "file_edit found {replacement_count} matches; pass `replace_all=true` or make `old_text` more specific"
            )));
        }

        let first_offset = content
            .find(old_text)
            .expect("replacement_count > 0 guarantees a first match");
        let last_offset = content
            .rfind(old_text)
            .expect("replacement_count > 0 guarantees a last match");
        let first_start_line = line_number_for_offset(&content, first_offset);
        let first_end_line = ending_line_for_span(&content, first_offset, old_text.len());
        let last_start_line = line_number_for_offset(&content, last_offset);
        let last_end_line = ending_line_for_span(&content, last_offset, old_text.len());
        let before_preview = surrounding_snippet(
            &content,
            first_offset,
            first_offset + old_text.len(),
            PREVIEW_CHAR_LIMIT,
        );

        let updated = if replace_all {
            content.replace(old_text, new_text)
        } else {
            content.replacen(old_text, new_text, 1)
        };
        let after_span_end = first_offset + new_text.len();
        let after_preview =
            surrounding_snippet(&updated, first_offset, after_span_end, PREVIEW_CHAR_LIMIT);

        fs::write(&resolved, updated.as_bytes())
            .await
            .map_err(anyhow::Error::from)
            .map_err(ToolInvocationError::Failed)?;

        Ok(json!({
            "path": resolved,
            "operation": if replace_all { "replace_all" } else { "replace" },
            "replacement_count": if replace_all { replacement_count } else { 1 },
            "byte_count": updated.len(),
            "first_replacement": {
                "start_line": first_start_line,
                "end_line": first_end_line,
                "before_preview": before_preview,
                "after_preview": after_preview,
            },
            "last_replacement": if replace_all && replacement_count > 1 {
                json!({
                    "start_line": last_start_line,
                    "end_line": last_end_line,
                })
            } else {
                Value::Null
            },
        }))
    }
}

fn line_number_for_offset(content: &str, offset: usize) -> usize {
    1 + content[..offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
}

fn ending_line_for_span(content: &str, start_offset: usize, span_len: usize) -> usize {
    let end_offset = start_offset + span_len;
    line_number_for_offset(content, end_offset)
}

fn surrounding_snippet(content: &str, start: usize, end: usize, limit: usize) -> String {
    let snippet_start = start.saturating_sub(limit / 2);
    let snippet_end = (end + (limit / 2)).min(content.len());
    let mut snippet = String::new();
    if snippet_start > 0 {
        snippet.push_str("...");
    }
    snippet.push_str(&content[snippet_start..snippet_end]);
    if snippet_end < content.len() {
        snippet.push_str("...");
    }
    snippet
}
