use std::path::Path;

use anyhow::anyhow;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolCapability, ToolInvocationError, ToolUseContext};

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Recursively searches UTF-8 text files for a substring. Prefer this over shell grep/find pipelines when searching code."
    }

    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }

    async fn invoke(
        &self,
        input: Value,
        context: &ToolUseContext,
    ) -> Result<Value, ToolInvocationError> {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolInvocationError::Failed(anyhow!("grep requires a string `pattern`"))
            })?;
        let raw_root = input.get("root").and_then(Value::as_str).unwrap_or(".");
        let case_sensitive = input
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let max_results = input
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(50) as usize;

        let root = context
            .permissions()
            .resolve_read_path(context.cwd(), raw_root)?;
        let mut matches = Vec::new();
        collect_matches(
            context,
            &root,
            &root,
            pattern,
            case_sensitive,
            max_results,
            &mut matches,
        )
        .map_err(ToolInvocationError::Failed)?;

        Ok(json!({
            "pattern": pattern,
            "root": root,
            "case_sensitive": case_sensitive,
            "matches": matches,
        }))
    }
}

fn collect_matches(
    context: &ToolUseContext,
    walk_root: &Path,
    current: &Path,
    pattern: &str,
    case_sensitive: bool,
    max_results: usize,
    matches: &mut Vec<Value>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(current)? {
        if matches.len() >= max_results {
            break;
        }

        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if context.permissions().should_skip_walk_entry_for_read(&path) {
            continue;
        }

        if file_type.is_dir() {
            collect_matches(
                context,
                walk_root,
                &path,
                pattern,
                case_sensitive,
                max_results,
                matches,
            )?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };

        let needle = if case_sensitive {
            pattern.to_string()
        } else {
            pattern.to_lowercase()
        };

        for (line_number, line) in content.lines().enumerate() {
            if matches.len() >= max_results {
                break;
            }

            let haystack = if case_sensitive {
                line.to_string()
            } else {
                line.to_lowercase()
            };

            if haystack.contains(&needle) {
                matches.push(json!({
                    "path": path.strip_prefix(walk_root).unwrap_or(&path).to_string_lossy().replace('\\', "/"),
                    "line_number": line_number + 1,
                    "line": line,
                }));
            }
        }
    }

    Ok(())
}
