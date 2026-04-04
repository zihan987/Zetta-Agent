use anyhow::Result;
use serde_json::{json, Value};

const TOOL_RESULT_MESSAGE_TYPE: &str = "tool_result";
const TOOL_STATUS_COMPLETED: &str = "completed";
const TOOL_STATUS_DENIED: &str = "denied";
const TOOL_STATUS_FAILED: &str = "failed";
const TOOL_STATUS_INVALID_CALL: &str = "invalid_call";

pub fn encode_tool_result_message(tool_name: &str, output: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(&json!({
        "type": TOOL_RESULT_MESSAGE_TYPE,
        "tool_name": tool_name,
        "status": TOOL_STATUS_COMPLETED,
        "output": output,
    }))?)
}

pub fn encode_tool_denied_message(tool_name: &str, reason: &str) -> Result<String> {
    Ok(serde_json::to_string_pretty(&json!({
        "type": TOOL_RESULT_MESSAGE_TYPE,
        "tool_name": tool_name,
        "status": TOOL_STATUS_DENIED,
        "reason": reason,
    }))?)
}

pub fn encode_tool_failed_message(tool_name: &str, error: &str) -> Result<String> {
    Ok(serde_json::to_string_pretty(&json!({
        "type": TOOL_RESULT_MESSAGE_TYPE,
        "tool_name": tool_name,
        "status": TOOL_STATUS_FAILED,
        "error": error,
    }))?)
}

pub fn encode_tool_invalid_call_message(raw: &str, error: &str) -> Result<String> {
    Ok(serde_json::to_string_pretty(&json!({
        "type": TOOL_RESULT_MESSAGE_TYPE,
        "tool_name": "tool_call",
        "status": TOOL_STATUS_INVALID_CALL,
        "raw": raw,
        "error": error,
    }))?)
}

pub fn render_tool_result_for_model(content: &str) -> String {
    if let Some(parsed) = decode_tool_result_message(content) {
        match parsed.status.as_str() {
            TOOL_STATUS_COMPLETED => {
                let pretty_output = serde_json::to_string_pretty(&parsed.output)
                    .unwrap_or_else(|_| parsed.output.to_string());
                format!(
                    "Tool `{}` completed with JSON output:\n{}",
                    parsed.tool_name, pretty_output
                )
            }
            TOOL_STATUS_DENIED => format!(
                "Tool `{}` was denied:\n{}",
                parsed.tool_name,
                parsed
                    .reason
                    .unwrap_or_else(|| "permission denied".to_string())
            ),
            TOOL_STATUS_FAILED => format!(
                "Tool `{}` failed:\n{}",
                parsed.tool_name,
                parsed
                    .error
                    .unwrap_or_else(|| "unknown tool error".to_string())
            ),
            TOOL_STATUS_INVALID_CALL => format!(
                "The previous tool call was invalid.\nRaw request:\n{}\nError:\n{}",
                parsed
                    .raw
                    .unwrap_or_else(|| "<missing raw tool call>".to_string()),
                parsed
                    .error
                    .unwrap_or_else(|| "invalid tool call".to_string())
            ),
            _ => format!("Tool output:\n{content}"),
        }
    } else {
        format!("Tool output:\n{content}")
    }
}

pub fn summarize_tool_result(content: &str) -> String {
    if let Some(parsed) = decode_tool_result_message(content) {
        match parsed.status.as_str() {
            TOOL_STATUS_COMPLETED => {
                let pretty_output = serde_json::to_string_pretty(&parsed.output)
                    .unwrap_or_else(|_| parsed.output.to_string());
                format!("Tool `{}` returned:\n{}", parsed.tool_name, pretty_output)
            }
            TOOL_STATUS_DENIED => format!(
                "Tool `{}` was denied:\n{}",
                parsed.tool_name,
                parsed
                    .reason
                    .unwrap_or_else(|| "permission denied".to_string())
            ),
            TOOL_STATUS_FAILED => format!(
                "Tool `{}` failed:\n{}",
                parsed.tool_name,
                parsed
                    .error
                    .unwrap_or_else(|| "unknown tool error".to_string())
            ),
            TOOL_STATUS_INVALID_CALL => format!(
                "The previous tool call was invalid.\nRaw request:\n{}\nError:\n{}",
                parsed
                    .raw
                    .unwrap_or_else(|| "<missing raw tool call>".to_string()),
                parsed
                    .error
                    .unwrap_or_else(|| "invalid tool call".to_string())
            ),
            _ => format!("Tool returned:\n{content}"),
        }
    } else {
        format!("Tool returned:\n{content}")
    }
}

struct DecodedToolResult {
    tool_name: String,
    status: String,
    output: Value,
    reason: Option<String>,
    error: Option<String>,
    raw: Option<String>,
}

fn decode_tool_result_message(content: &str) -> Option<DecodedToolResult> {
    let value = serde_json::from_str::<Value>(content).ok()?;
    let object = value.as_object()?;
    let message_type = object.get("type")?.as_str()?;
    if message_type != TOOL_RESULT_MESSAGE_TYPE {
        return None;
    }

    Some(DecodedToolResult {
        tool_name: object.get("tool_name")?.as_str()?.to_string(),
        status: object
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or(TOOL_STATUS_COMPLETED)
            .to_string(),
        output: object.get("output").cloned().unwrap_or(Value::Null),
        reason: object
            .get("reason")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        error: object
            .get("error")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        raw: object
            .get("raw")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        encode_tool_denied_message, encode_tool_failed_message, encode_tool_invalid_call_message,
        encode_tool_result_message, render_tool_result_for_model, summarize_tool_result,
    };

    #[test]
    fn structured_tool_result_round_trips_for_model_rendering() {
        let encoded = encode_tool_result_message("grep", &json!({"matches": 2})).expect("encode");
        let rendered = render_tool_result_for_model(&encoded);
        assert!(rendered.contains("Tool `grep` completed"));
        assert!(rendered.contains("\"matches\": 2"));
    }

    #[test]
    fn summarize_tool_result_falls_back_for_legacy_content() {
        let summary = summarize_tool_result("{\"legacy\":true}");
        assert!(summary.contains("Tool returned:"));
    }

    #[test]
    fn denied_and_failed_tool_results_render_distinctly() {
        let denied = encode_tool_denied_message("bash", "policy blocked").expect("encode denied");
        let failed = encode_tool_failed_message("grep", "spawn error").expect("encode failed");

        assert!(render_tool_result_for_model(&denied).contains("was denied"));
        assert!(summarize_tool_result(&failed).contains("failed"));
    }

    #[test]
    fn invalid_tool_calls_render_distinctly() {
        let invalid = encode_tool_invalid_call_message(
            "/tool file_read_lines src/main.rs:oops",
            "expected `path:start-end` for `file_read_lines`",
        )
        .expect("encode invalid");

        let rendered = render_tool_result_for_model(&invalid);
        assert!(rendered.contains("previous tool call was invalid"));
        assert!(rendered.contains("file_read_lines"));
    }
}
