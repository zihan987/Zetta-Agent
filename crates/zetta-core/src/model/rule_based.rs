use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use zetta_protocol::{SessionSnapshot, ToolCall};

use super::{summarize_tool_result, ModelClient, PlannedTurn};

pub struct RuleBasedModelClient;

pub enum ParsedToolCall {
    NotAToolCall,
    Valid(ToolCall),
    Invalid { error: String },
}

pub fn parse_tool_call_from_user_input(input: &str) -> ParsedToolCall {
    let command = match extract_tool_call_line(input) {
        Ok(Some(command)) => command,
        Ok(None) => return ParsedToolCall::NotAToolCall,
        Err(error) => return ParsedToolCall::Invalid { error },
    };
    parse_tool_call_line(command)
}

fn extract_tool_call_line(input: &str) -> Result<Option<&str>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if !trimmed.contains('\n') {
        return Ok(trimmed.starts_with("/tool").then_some(trimmed));
    }

    let non_empty_lines = trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let tool_line_indexes = non_empty_lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("/tool").then_some(index))
        .collect::<Vec<_>>();

    match tool_line_indexes.as_slice() {
        [] => Ok(None),
        [index] => {
            if *index != non_empty_lines.len() - 1 {
                Err("tool call line must be the last non-empty line in the response".to_string())
            } else {
                Ok(Some(non_empty_lines[*index]))
            }
        }
        _ => Err("multiple tool call lines found; emit only one `/tool ...` line".to_string()),
    }
}

fn parse_tool_call_line(command: &str) -> ParsedToolCall {
    let Some(command) = command.trim().strip_prefix("/tool") else {
        return ParsedToolCall::NotAToolCall;
    };
    let command = command.trim_start();
    if command.is_empty() {
        return ParsedToolCall::Invalid {
            error: "missing tool name after `/tool`".to_string(),
        };
    }

    let mut segments = command.splitn(2, char::is_whitespace);
    let name = segments.next().unwrap_or_default().trim();
    let raw_payload = segments.next().unwrap_or_default().trim();
    if name.is_empty() {
        return ParsedToolCall::Invalid {
            error: "missing tool name after `/tool`".to_string(),
        };
    }

    let tool_input = if raw_payload.is_empty() {
        Ok(json!({}))
    } else if raw_payload.starts_with('{') {
        serde_json::from_str::<Value>(raw_payload)
            .map_err(|error| format!("invalid JSON payload for `{name}`: {error}"))
    } else {
        parse_raw_tool_payload(name, raw_payload)
    };

    match tool_input {
        Ok(input) => ParsedToolCall::Valid(ToolCall {
            name: name.to_string(),
            input,
        }),
        Err(error) => ParsedToolCall::Invalid { error },
    }
}

pub fn tool_call_from_user_input(input: &str) -> Option<ToolCall> {
    match parse_tool_call_from_user_input(input) {
        ParsedToolCall::Valid(call) => Some(call),
        ParsedToolCall::NotAToolCall | ParsedToolCall::Invalid { .. } => None,
    }
}

fn parse_raw_tool_payload(name: &str, raw_payload: &str) -> Result<Value, String> {
    match name {
        "echo" => Ok(json!({ "text": raw_payload })),
        "bash" => Ok(json!({ "command": raw_payload })),
        "file_read" => Ok(json!({ "path": raw_payload })),
        "file_read_lines" => {
            let (path, range) = raw_payload
                .rsplit_once(':')
                .ok_or_else(|| "expected `path:start-end` for `file_read_lines`".to_string())?;
            let (start_line, end_line) = range
                .split_once('-')
                .ok_or_else(|| "expected `start-end` line range".to_string())?;
            Ok(json!({
                "path": path.trim(),
                "start_line": start_line
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| "invalid `start_line` in range".to_string())?,
                "end_line": end_line
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| "invalid `end_line` in range".to_string())?,
            }))
        }
        "file_edit_lines" => {
            let (locator, new_text) = raw_payload.split_once(' ').ok_or_else(|| {
                "expected `path:start-end replacement text` for `file_edit_lines`".to_string()
            })?;
            let (path, range) = locator.rsplit_once(':').ok_or_else(|| {
                "expected `path:start-end replacement text` for `file_edit_lines`".to_string()
            })?;
            let (start_line, end_line) = range
                .split_once('-')
                .ok_or_else(|| "expected `start-end` line range".to_string())?;
            Ok(json!({
                "path": path.trim(),
                "start_line": start_line
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| "invalid `start_line` in range".to_string())?,
                "end_line": end_line
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| "invalid `end_line` in range".to_string())?,
                "new_text": new_text,
            }))
        }
        "glob" => Ok(json!({ "pattern": raw_payload })),
        "grep" => Ok(json!({ "pattern": raw_payload })),
        _ => Ok(json!({ "raw": raw_payload })),
    }
}

#[async_trait]
impl ModelClient for RuleBasedModelClient {
    async fn plan_turn(&self, session: &SessionSnapshot) -> Result<PlannedTurn> {
        let Some(last_message) = session.messages.last() else {
            return Ok(PlannedTurn::AssistantMessage(
                "Phase 2 placeholder response:".to_string(),
            ));
        };

        if matches!(last_message.role, zetta_protocol::MessageRole::Tool) {
            return Ok(PlannedTurn::AssistantMessage(summarize_tool_result(
                &last_message.content,
            )));
        }

        let last_user = session
            .messages
            .iter()
            .rev()
            .find(|message| matches!(message.role, zetta_protocol::MessageRole::User))
            .map(|message| message.content.as_str())
            .unwrap_or_default();

        match parse_tool_call_from_user_input(last_user) {
            ParsedToolCall::Valid(call) => return Ok(PlannedTurn::ToolCall(call)),
            ParsedToolCall::Invalid { error } => {
                return Ok(PlannedTurn::InvalidToolCall {
                    raw: last_user.to_string(),
                    error,
                });
            }
            ParsedToolCall::NotAToolCall => {}
        }

        Ok(PlannedTurn::AssistantMessage(format!(
            "Phase 2 placeholder response: {last_user}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_tool_call_from_user_input, ParsedToolCall};

    #[test]
    fn parser_returns_detailed_error_for_invalid_line_range() {
        let parsed = parse_tool_call_from_user_input("/tool file_read_lines src/main.rs:oops");
        match parsed {
            ParsedToolCall::Invalid { error } => {
                assert!(error.contains("path:start-end") || error.contains("start-end"));
            }
            ParsedToolCall::NotAToolCall | ParsedToolCall::Valid(_) => {
                panic!("expected invalid tool call")
            }
        }
    }

    #[test]
    fn parser_keeps_valid_raw_tool_shorthand() {
        let parsed = parse_tool_call_from_user_input("/tool echo hello rust");
        match parsed {
            ParsedToolCall::Valid(call) => {
                assert_eq!(call.name, "echo");
                assert_eq!(call.input, json!({"text": "hello rust"}));
            }
            ParsedToolCall::NotAToolCall | ParsedToolCall::Invalid { .. } => {
                panic!("expected valid tool call")
            }
        }
    }

    #[test]
    fn parser_accepts_trailing_tool_call_after_explanatory_text() {
        let parsed = parse_tool_call_from_user_input(
            "I will inspect the workspace first.\n\n/tool grep {\"pattern\":\"mamba\"}",
        );
        match parsed {
            ParsedToolCall::Valid(call) => {
                assert_eq!(call.name, "grep");
                assert_eq!(call.input, json!({"pattern": "mamba"}));
            }
            ParsedToolCall::NotAToolCall | ParsedToolCall::Invalid { .. } => {
                panic!("expected valid trailing tool call")
            }
        }
    }

    #[test]
    fn parser_rejects_multiple_tool_call_lines() {
        let parsed = parse_tool_call_from_user_input(
            "/tool grep {\"pattern\":\"a\"}\n/tool grep {\"pattern\":\"b\"}",
        );
        match parsed {
            ParsedToolCall::Invalid { error } => {
                assert!(error.contains("multiple tool call lines"));
            }
            ParsedToolCall::NotAToolCall | ParsedToolCall::Valid(_) => {
                panic!("expected invalid tool call")
            }
        }
    }

    #[test]
    fn parser_rejects_tool_call_followed_by_extra_text() {
        let parsed = parse_tool_call_from_user_input(
            "/tool grep {\"pattern\":\"mamba\"}\nI found several matches.",
        );
        match parsed {
            ParsedToolCall::Invalid { error } => {
                assert!(error.contains("last non-empty line"));
            }
            ParsedToolCall::NotAToolCall | ParsedToolCall::Valid(_) => {
                panic!("expected invalid tool call")
            }
        }
    }
}
