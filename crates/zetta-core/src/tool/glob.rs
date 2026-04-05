use std::path::Path;

use anyhow::anyhow;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolCapability, ToolInvocationError, ToolUseContext};

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Recursively matches files under a root using simple * and ? wildcards. Prefer this for repository structure and file discovery."
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
                ToolInvocationError::Failed(anyhow!("glob requires a string `pattern`"))
            })?;
        let raw_root = input.get("root").and_then(Value::as_str).unwrap_or(".");
        let max_results = input
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(100) as usize;

        let root = context
            .permissions()
            .resolve_read_path(context.cwd(), raw_root)?;
        let mut matches = Vec::new();
        collect_matching_paths(context, &root, &root, pattern, max_results, &mut matches)
            .map_err(ToolInvocationError::Failed)?;

        Ok(json!({
            "pattern": pattern,
            "root": root,
            "max_results": max_results,
            "truncated": matches.len() >= max_results,
            "matches": matches,
        }))
    }
}

fn collect_matching_paths(
    context: &ToolUseContext,
    walk_root: &Path,
    current: &Path,
    pattern: &str,
    max_results: usize,
    matches: &mut Vec<String>,
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
            collect_matching_paths(context, walk_root, &path, pattern, max_results, matches)?;
            continue;
        }

        if file_type.is_file() {
            let relative = relative_string(walk_root, &path);
            if wildcard_match(pattern, &relative) {
                matches.push(relative);
            }
        }
    }

    matches.sort();
    Ok(())
}

fn relative_string(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn wildcard_match(pattern: &str, candidate: &str) -> bool {
    wildcard_match_bytes(pattern.as_bytes(), candidate.as_bytes())
}

fn wildcard_match_bytes(pattern: &[u8], candidate: &[u8]) -> bool {
    match (pattern.first(), candidate.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(b'*'), _) => {
            wildcard_match_bytes(&pattern[1..], candidate)
                || (!candidate.is_empty() && wildcard_match_bytes(pattern, &candidate[1..]))
        }
        (Some(b'?'), Some(_)) => wildcard_match_bytes(&pattern[1..], &candidate[1..]),
        (Some(expected), Some(actual)) if expected == actual => {
            wildcard_match_bytes(&pattern[1..], &candidate[1..])
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::wildcard_match;

    #[test]
    fn wildcard_matching_supports_star_and_question_mark() {
        assert!(wildcard_match("src/*.rs", "src/main.rs"));
        assert!(wildcard_match("foo?.txt", "foo1.txt"));
        assert!(!wildcard_match("src/*.rs", "src/main.ts"));
    }
}
