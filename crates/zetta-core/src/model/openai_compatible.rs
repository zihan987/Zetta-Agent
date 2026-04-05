use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use zetta_protocol::{MessageRole, SessionSnapshot};

use super::{
    parse_tool_call_from_user_input, render_tool_result_for_model, ModelClient, ModelStreamSink,
    ParsedToolCall, PlannedTurn,
};
use crate::tool::ToolDefinition;

const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleConfig {
    pub api_key: String,
    pub model: String,
    pub api_base: String,
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolDefinition>,
    pub request_timeout: Duration,
    pub max_retries: usize,
    pub retry_backoff: Duration,
}

impl OpenAiCompatibleConfig {
    #[must_use]
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            api_base: DEFAULT_API_BASE.to_string(),
            system_prompt: None,
            tools: Vec::new(),
            request_timeout: Duration::from_secs(45),
            max_retries: 2,
            retry_backoff: Duration::from_millis(500),
        }
    }
}

pub struct OpenAiCompatibleModelClient {
    http: reqwest::Client,
    config: OpenAiCompatibleConfig,
}

impl OpenAiCompatibleModelClient {
    pub fn new(config: OpenAiCompatibleConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let auth_value = format!("Bearer {}", config.api_key);
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&auth_value)
                .context("invalid API key for authorization header")?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self { http, config })
    }

    fn chat_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.api_base.trim_end_matches('/')
        )
    }

    fn build_messages(&self, session: &SessionSnapshot) -> Vec<ChatMessageRequest> {
        let mut messages = Vec::new();

        if let Some(system_prompt) = &self.config.system_prompt {
            messages.push(ChatMessageRequest {
                role: "system".to_string(),
                content: system_prompt.clone(),
            });
        }

        for message in &session.messages {
            let content = match message.role {
                MessageRole::System | MessageRole::User | MessageRole::Assistant => {
                    message.content.clone()
                }
                MessageRole::Tool => render_tool_result_for_model(&message.content),
            };

            let role = match message.role {
                MessageRole::System => "system",
                MessageRole::User | MessageRole::Tool => "user",
                MessageRole::Assistant => "assistant",
            };

            messages.push(ChatMessageRequest {
                role: role.to_string(),
                content,
            });
        }

        messages
    }

    async fn send_chat_request(
        &self,
        request: &ChatCompletionRequest,
        operation: &str,
    ) -> Result<reqwest::Response> {
        for attempt in 0..=self.config.max_retries {
            match timeout(
                self.config.request_timeout,
                self.http.post(self.chat_url()).json(request).send(),
            )
            .await
            {
                Ok(Ok(response)) => {
                    if response.status().is_success() {
                        return Ok(response);
                    }

                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    let message = format_provider_status_error(status, &body);
                    if attempt < self.config.max_retries && is_retryable_status(status) {
                        sleep(backoff_for_attempt(self.config.retry_backoff, attempt)).await;
                        continue;
                    }

                    bail!("{operation} failed: {message}");
                }
                Ok(Err(error)) => {
                    if attempt < self.config.max_retries && is_retryable_transport_error(&error) {
                        sleep(backoff_for_attempt(self.config.retry_backoff, attempt)).await;
                        continue;
                    }

                    return Err(error).with_context(|| operation.to_string());
                }
                Err(_) => {
                    if attempt < self.config.max_retries {
                        sleep(backoff_for_attempt(self.config.retry_backoff, attempt)).await;
                        continue;
                    }

                    bail!(
                        "{operation} timed out after {}s",
                        self.config.request_timeout.as_secs()
                    );
                }
            }
        }

        unreachable!("retry loop should always return or continue");
    }

    async fn request_planned_turn(&self, session: &SessionSnapshot) -> Result<PlannedTurn> {
        let request = ChatCompletionRequest {
            model: self.config.model.clone(),
            messages: self.build_messages(session),
            tools: self.build_tool_definitions(),
            tool_choice: self.default_tool_choice(),
            stream: None,
        };

        let response = self
            .send_chat_request(&request, "model request")
            .await
            .context("model request failed")?;
        let body: ChatCompletionResponse = timeout(self.config.request_timeout, response.json())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "model response timed out after {}s while decoding",
                    self.config.request_timeout.as_secs()
                )
            })?
            .context("failed to decode model response")?;

        extract_planned_turn(body)
    }

    async fn request_planned_turn_streaming(
        &self,
        session: &SessionSnapshot,
    ) -> Result<PlannedTurn> {
        let request = ChatCompletionRequest {
            model: self.config.model.clone(),
            messages: self.build_messages(session),
            tools: self.build_tool_definitions(),
            tool_choice: self.default_tool_choice(),
            stream: Some(true),
        };
        let mut response = self
            .send_chat_request(&request, "streaming model request")
            .await
            .context("streaming model request failed")?;
        let mut pending = String::new();
        let mut aggregate = StreamingResponseAggregate::default();

        while let Some(chunk) = timeout(self.config.request_timeout, response.chunk())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "streaming model response stalled for {}s",
                    self.config.request_timeout.as_secs()
                )
            })?
            .context("failed to read streaming response chunk")?
        {
            pending.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline_index) = pending.find('\n') {
                let line = pending[..newline_index].trim().to_string();
                pending.drain(..=newline_index);

                aggregate.apply(parse_sse_data_line(&line)?);
            }
        }

        let trailing = pending.trim();
        aggregate.apply(parse_sse_data_line(trailing)?);

        planned_turn_from_streaming_aggregate(aggregate)
    }

    fn build_tool_definitions(&self) -> Option<Vec<ChatToolDefinitionRequest>> {
        if self.config.tools.is_empty() {
            return None;
        }

        Some(
            self.config
                .tools
                .iter()
                .map(|tool| ChatToolDefinitionRequest {
                    kind: "function".to_string(),
                    function: ChatFunctionDefinitionRequest {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        parameters: tool_parameters_schema(&tool.name),
                    },
                })
                .collect(),
        )
    }

    fn default_tool_choice(&self) -> Option<Value> {
        if self.config.tools.is_empty() {
            None
        } else {
            Some(Value::String("auto".to_string()))
        }
    }
}

#[async_trait]
impl ModelClient for OpenAiCompatibleModelClient {
    async fn plan_turn(&self, session: &SessionSnapshot) -> Result<PlannedTurn> {
        self.request_planned_turn(session).await
    }

    async fn plan_turn_with_sink(
        &self,
        session: &SessionSnapshot,
        mut sink: Option<&mut dyn ModelStreamSink>,
    ) -> Result<PlannedTurn> {
        let planned = match sink {
            Some(_) => self.request_planned_turn_streaming(session).await?,
            None => self.request_planned_turn(session).await?,
        };
        if let (Some(sink), Some(content)) = (&mut sink, streamed_assistant_output(&planned)) {
            sink.on_text_delta(content)?;
            sink.on_message_end()?;
        }
        Ok(planned)
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessageRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ChatToolDefinitionRequest>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ChatMessageRequest {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: Option<String>,
    tool_calls: Option<Vec<ChatToolCallResponse>>,
}

#[derive(Debug, Serialize)]
struct ChatToolDefinitionRequest {
    #[serde(rename = "type")]
    kind: String,
    function: ChatFunctionDefinitionRequest,
}

#[derive(Debug, Serialize)]
struct ChatFunctionDefinitionRequest {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatToolCallResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    function: ChatFunctionCallResponse,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatFunctionCallResponse {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct StreamingChatCompletionChunk {
    choices: Vec<StreamingChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamingChoice {
    delta: StreamingDelta,
}

#[derive(Debug, Deserialize)]
struct StreamingDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamingToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct StreamingToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    function: Option<StreamingFunctionCallDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamingFunctionCallDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct StreamingResponseAggregate {
    content: String,
    tool_calls: BTreeMap<usize, StreamingToolCallAggregate>,
}

#[derive(Default)]
struct StreamingToolCallAggregate {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl StreamingResponseAggregate {
    fn apply(&mut self, delta: StreamingChunkDelta) {
        self.content.push_str(&delta.content);
        for tool_call in delta.tool_calls {
            let entry = self.tool_calls.entry(tool_call.index).or_default();
            if let Some(id) = tool_call.id {
                entry.id = Some(id);
            }
            if let Some(name) = tool_call.name {
                entry.name.push_str(&name);
            }
            if let Some(arguments) = tool_call.arguments {
                entry.arguments.push_str(&arguments);
            }
        }
    }
}

#[derive(Default)]
struct StreamingChunkDelta {
    content: String,
    tool_calls: Vec<StreamingToolCallFragment>,
}

struct StreamingToolCallFragment {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

fn extract_planned_turn(response: ChatCompletionResponse) -> Result<PlannedTurn> {
    let Some(choice) = response.choices.into_iter().next() else {
        bail!("model response did not contain any choices");
    };

    planned_turn_from_chat_message(choice.message)
}

fn planned_turn_from_text(text: String) -> PlannedTurn {
    match parse_tool_call_from_user_input(&text) {
        ParsedToolCall::Valid(call) => PlannedTurn::ToolCall(call),
        ParsedToolCall::Invalid { error } => PlannedTurn::InvalidToolCall { raw: text, error },
        ParsedToolCall::NotAToolCall => PlannedTurn::AssistantMessage(text),
    }
}

fn planned_turn_from_chat_message(message: ChatMessageResponse) -> Result<PlannedTurn> {
    if let Some(tool_calls) = message.tool_calls {
        return planned_turn_from_native_tool_calls(tool_calls);
    }

    let content = message.content.unwrap_or_default().trim().to_string();
    Ok(planned_turn_from_text(content))
}

fn planned_turn_from_native_tool_calls(
    tool_calls: Vec<ChatToolCallResponse>,
) -> Result<PlannedTurn> {
    if tool_calls.is_empty() {
        return Ok(PlannedTurn::AssistantMessage(String::new()));
    }
    if tool_calls.len() > 1 {
        let raw = serde_json::to_string_pretty(&tool_calls)
            .unwrap_or_else(|_| "<multiple tool calls>".to_string());
        return Ok(PlannedTurn::InvalidToolCall {
            raw,
            error: "native tool-calling returned multiple tool calls in one turn; only one is supported".to_string(),
        });
    }

    let call = &tool_calls[0];
    if call.kind != "function" {
        let raw = serde_json::to_string_pretty(call)
            .unwrap_or_else(|_| "<invalid tool call>".to_string());
        return Ok(PlannedTurn::InvalidToolCall {
            raw,
            error: format!("unsupported tool call type `{}`", call.kind),
        });
    }

    let parsed_arguments =
        serde_json::from_str::<Value>(&call.function.arguments).map_err(|error| {
            anyhow::anyhow!(
                "native tool-calling returned invalid JSON arguments for `{}`: {error}",
                call.function.name
            )
        });

    match parsed_arguments {
        Ok(input) => Ok(PlannedTurn::ToolCall(zetta_protocol::ToolCall {
            name: call.function.name.clone(),
            input,
        })),
        Err(error) => Ok(PlannedTurn::InvalidToolCall {
            raw: serde_json::to_string_pretty(call)
                .unwrap_or_else(|_| call.function.arguments.clone()),
            error: error.to_string(),
        }),
    }
}

fn planned_turn_from_streaming_aggregate(
    aggregate: StreamingResponseAggregate,
) -> Result<PlannedTurn> {
    if !aggregate.tool_calls.is_empty() {
        let tool_calls = aggregate
            .tool_calls
            .into_values()
            .map(|tool_call| ChatToolCallResponse {
                id: tool_call.id,
                kind: "function".to_string(),
                function: ChatFunctionCallResponse {
                    name: tool_call.name,
                    arguments: tool_call.arguments,
                },
            })
            .collect::<Vec<_>>();
        return planned_turn_from_native_tool_calls(tool_calls);
    }

    Ok(planned_turn_from_text(aggregate.content.trim().to_string()))
}

fn streamed_assistant_output(planned: &PlannedTurn) -> Option<&str> {
    match planned {
        PlannedTurn::AssistantMessage(content) => Some(content.as_str()),
        PlannedTurn::ToolCall(_) | PlannedTurn::InvalidToolCall { .. } => None,
    }
}

fn is_retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout()
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
            | StatusCode::INTERNAL_SERVER_ERROR
    )
}

fn backoff_for_attempt(base: Duration, attempt: usize) -> Duration {
    base.saturating_mul(2_u32.saturating_pow(attempt as u32))
}

fn format_provider_status_error(status: StatusCode, body: &str) -> String {
    let detail = summarize_provider_error_body(body);
    format!("provider returned HTTP {}: {}", status.as_u16(), detail)
}

fn summarize_provider_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "empty error body".to_string();
    }

    if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
        if let Some(message) = parsed
            .get("error")
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
            .or_else(|| parsed.get("message").and_then(Value::as_str))
        {
            return message.to_string();
        }
    }

    const MAX_LEN: usize = 240;
    if trimmed.len() > MAX_LEN {
        format!("{}...", &trimmed[..MAX_LEN])
    } else {
        trimmed.to_string()
    }
}

fn parse_sse_data_line(line: &str) -> Result<StreamingChunkDelta> {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.starts_with("data:") {
        return Ok(StreamingChunkDelta::default());
    }

    let payload = trimmed.trim_start_matches("data:").trim();
    if payload == "[DONE]" || payload.is_empty() {
        return Ok(StreamingChunkDelta::default());
    }

    let chunk: StreamingChatCompletionChunk =
        serde_json::from_str(payload).context("failed to decode streaming response chunk")?;
    let mut delta = StreamingChunkDelta::default();
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content {
            delta.content.push_str(&content);
        }
        if let Some(tool_calls) = choice.delta.tool_calls {
            for tool_call in tool_calls {
                delta.tool_calls.push(StreamingToolCallFragment {
                    index: tool_call.index,
                    id: tool_call.id,
                    name: tool_call
                        .function
                        .as_ref()
                        .and_then(|function| function.name.clone()),
                    arguments: tool_call
                        .function
                        .as_ref()
                        .and_then(|function| function.arguments.clone()),
                });
            }
        }
    }

    Ok(delta)
}

fn tool_parameters_schema(tool_name: &str) -> Value {
    match tool_name {
        "echo" => json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to echo back." }
            },
            "required": ["text"],
            "additionalProperties": false,
        }),
        "bash" => json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to run with zsh -lc." }
            },
            "required": ["command"],
            "additionalProperties": false,
        }),
        "file_read" => json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to read." }
            },
            "required": ["path"],
            "additionalProperties": false,
        }),
        "file_read_lines" => json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to read." },
                "start_line": { "type": "integer", "description": "1-based start line." },
                "end_line": { "type": "integer", "description": "1-based inclusive end line." }
            },
            "required": ["path", "start_line", "end_line"],
            "additionalProperties": false,
        }),
        "file_edit" => json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to edit." },
                "old_text": { "type": "string", "description": "Exact text to replace." },
                "new_text": { "type": "string", "description": "Replacement text." },
                "replace_all": { "type": "boolean", "description": "Replace every match instead of exactly one." }
            },
            "required": ["path", "old_text", "new_text"],
            "additionalProperties": false,
        }),
        "file_edit_lines" => json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to edit." },
                "start_line": { "type": "integer", "description": "1-based start line." },
                "end_line": { "type": "integer", "description": "1-based inclusive end line." },
                "new_text": { "type": "string", "description": "Replacement text for the selected line range." }
            },
            "required": ["path", "start_line", "end_line", "new_text"],
            "additionalProperties": false,
        }),
        "file_write" => json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to write." },
                "content": { "type": "string", "description": "Full UTF-8 file contents." },
                "overwrite": { "type": "boolean", "description": "Whether an existing file may be overwritten." }
            },
            "required": ["path", "content"],
            "additionalProperties": false,
        }),
        "glob" => json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Wildcard pattern using * and ?." },
                "root": { "type": "string", "description": "Optional root path to search from." },
                "max_results": { "type": "integer", "description": "Maximum number of matching paths to return." }
            },
            "required": ["pattern"],
            "additionalProperties": false,
        }),
        "grep" => json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Substring to search for." },
                "root": { "type": "string", "description": "Optional root path to search from." },
                "case_sensitive": { "type": "boolean", "description": "Whether matching should be case sensitive." },
                "max_results": { "type": "integer", "description": "Maximum number of matches to return." }
            },
            "required": ["pattern"],
            "additionalProperties": false,
        }),
        _ => json!({
            "type": "object",
            "additionalProperties": true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zetta_protocol::{Message, MessageRole, SessionId, SessionSnapshot};

    use super::{
        backoff_for_attempt, extract_planned_turn, format_provider_status_error,
        is_retryable_status, parse_sse_data_line, planned_turn_from_streaming_aggregate,
        planned_turn_from_text, streamed_assistant_output, summarize_provider_error_body,
        ChatChoice, ChatCompletionResponse, ChatFunctionCallResponse, ChatMessageResponse,
        ChatToolCallResponse, OpenAiCompatibleConfig, OpenAiCompatibleModelClient,
        StreamingResponseAggregate,
    };
    use crate::model::encode_tool_result_message;
    use crate::tool::{ToolCapability, ToolDefinition};
    use reqwest::StatusCode;
    use std::time::Duration;

    #[test]
    fn planned_turn_parses_tool_call_text() {
        let planned = planned_turn_from_text("/tool echo hello".to_string());
        match planned {
            super::PlannedTurn::ToolCall(call) => {
                assert_eq!(call.name, "echo");
                assert_eq!(call.input, json!({"text":"hello"}));
            }
            super::PlannedTurn::AssistantMessage(_)
            | super::PlannedTurn::InvalidToolCall { .. } => panic!("expected tool call"),
        }
    }

    #[test]
    fn planned_turn_accepts_trailing_tool_call_after_explanatory_text() {
        let planned = planned_turn_from_text(
            "I will search the workspace first.\n\n/tool grep {\"pattern\":\"mamba\"}".to_string(),
        );
        match planned {
            super::PlannedTurn::ToolCall(call) => {
                assert_eq!(call.name, "grep");
                assert_eq!(call.input, json!({"pattern":"mamba"}));
            }
            super::PlannedTurn::AssistantMessage(_)
            | super::PlannedTurn::InvalidToolCall { .. } => panic!("expected tool call"),
        }
    }

    #[test]
    fn planned_turn_preserves_invalid_tool_call_attempts() {
        let planned = planned_turn_from_text("/tool file_read_lines src/main.rs:oops".to_string());
        match planned {
            super::PlannedTurn::InvalidToolCall { raw, error } => {
                assert!(raw.contains("file_read_lines"));
                assert!(error.contains("path:start-end") || error.contains("start-end"));
            }
            super::PlannedTurn::AssistantMessage(_) | super::PlannedTurn::ToolCall(_) => {
                panic!("expected invalid tool call")
            }
        }
    }

    #[test]
    fn extract_planned_turn_returns_first_choice() {
        let planned = extract_planned_turn(ChatCompletionResponse {
            choices: vec![ChatChoice {
                message: ChatMessageResponse {
                    content: Some("hello remote".to_string()),
                    tool_calls: None,
                },
            }],
        })
        .expect("assistant text");

        match planned {
            super::PlannedTurn::AssistantMessage(text) => assert_eq!(text, "hello remote"),
            super::PlannedTurn::ToolCall(_) | super::PlannedTurn::InvalidToolCall { .. } => {
                panic!("expected assistant message")
            }
        }
    }

    #[test]
    fn extract_planned_turn_prefers_native_tool_calls() {
        let planned = extract_planned_turn(ChatCompletionResponse {
            choices: vec![ChatChoice {
                message: ChatMessageResponse {
                    content: Some("I will use a tool".to_string()),
                    tool_calls: Some(vec![ChatToolCallResponse {
                        id: Some("call_1".to_string()),
                        kind: "function".to_string(),
                        function: ChatFunctionCallResponse {
                            name: "grep".to_string(),
                            arguments: "{\"pattern\":\"auth\"}".to_string(),
                        },
                    }]),
                },
            }],
        })
        .expect("planned turn");

        match planned {
            super::PlannedTurn::ToolCall(call) => {
                assert_eq!(call.name, "grep");
                assert_eq!(call.input, json!({"pattern":"auth"}));
            }
            super::PlannedTurn::AssistantMessage(_)
            | super::PlannedTurn::InvalidToolCall { .. } => panic!("expected tool call"),
        }
    }

    #[test]
    fn extract_planned_turn_rejects_multiple_native_tool_calls() {
        let planned = extract_planned_turn(ChatCompletionResponse {
            choices: vec![ChatChoice {
                message: ChatMessageResponse {
                    content: None,
                    tool_calls: Some(vec![
                        ChatToolCallResponse {
                            id: Some("call_1".to_string()),
                            kind: "function".to_string(),
                            function: ChatFunctionCallResponse {
                                name: "grep".to_string(),
                                arguments: "{\"pattern\":\"auth\"}".to_string(),
                            },
                        },
                        ChatToolCallResponse {
                            id: Some("call_2".to_string()),
                            kind: "function".to_string(),
                            function: ChatFunctionCallResponse {
                                name: "glob".to_string(),
                                arguments: "{\"pattern\":\"*.rs\"}".to_string(),
                            },
                        },
                    ]),
                },
            }],
        })
        .expect("planned turn");

        match planned {
            super::PlannedTurn::InvalidToolCall { error, .. } => {
                assert!(error.contains("multiple tool calls"));
            }
            super::PlannedTurn::AssistantMessage(_) | super::PlannedTurn::ToolCall(_) => {
                panic!("expected invalid tool call")
            }
        }
    }

    #[test]
    fn tool_messages_are_rendered_as_user_context() {
        let mut session = SessionSnapshot::new(SessionId::new());
        session
            .messages
            .push(Message::new(MessageRole::User, "search this repo"));
        session.messages.push(Message::new(
            MessageRole::Tool,
            encode_tool_result_message("glob", &json!({"files": 1})).expect("encode"),
        ));

        let client =
            OpenAiCompatibleModelClient::new(OpenAiCompatibleConfig::new("test-key", "test-model"))
                .expect("client");
        let messages = client.build_messages(&session);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "user");
        assert!(messages[1].content.contains("Tool `glob` completed"));
        assert!(messages[1].content.contains("\"files\": 1"));
    }

    #[test]
    fn parse_sse_data_line_extracts_content_delta() {
        let delta = parse_sse_data_line(r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#)
            .expect("parse chunk");
        assert_eq!(delta.content, "hello");
    }

    #[test]
    fn parse_sse_data_line_ignores_done_marker() {
        let delta = parse_sse_data_line("data: [DONE]").expect("parse done");
        assert!(delta.content.is_empty());
        assert!(delta.tool_calls.is_empty());
    }

    #[test]
    fn parse_sse_data_line_extracts_tool_call_fragments() {
        let delta = parse_sse_data_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"grep","arguments":"{\"pattern\":\"auth\"}"}}]}}]}"#,
        )
        .expect("parse tool call chunk");

        assert_eq!(delta.tool_calls.len(), 1);
        assert_eq!(delta.tool_calls[0].index, 0);
        assert_eq!(delta.tool_calls[0].name.as_deref(), Some("grep"));
    }

    #[test]
    fn only_assistant_messages_are_forwarded_to_stream_output() {
        let assistant = planned_turn_from_text("hello remote".to_string());
        let tool_call = planned_turn_from_text(
            "I will search first.\n\n/tool grep {\"pattern\":\"mamba\"}".to_string(),
        );

        assert_eq!(streamed_assistant_output(&assistant), Some("hello remote"));
        assert_eq!(streamed_assistant_output(&tool_call), None);
    }

    #[test]
    fn streaming_aggregate_yields_native_tool_call_turn() {
        let mut aggregate = StreamingResponseAggregate::default();
        aggregate.apply(
            parse_sse_data_line(
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"grep","arguments":"{\"pattern\":\"auth\"}"}}]}}]}"#,
            )
            .expect("parse tool call chunk"),
        );

        let planned = planned_turn_from_streaming_aggregate(aggregate).expect("planned");
        match planned {
            super::PlannedTurn::ToolCall(call) => {
                assert_eq!(call.name, "grep");
                assert_eq!(call.input, json!({"pattern":"auth"}));
            }
            super::PlannedTurn::AssistantMessage(_)
            | super::PlannedTurn::InvalidToolCall { .. } => panic!("expected tool call"),
        }
    }

    #[test]
    fn build_tool_definitions_emits_function_schema() {
        let mut config = OpenAiCompatibleConfig::new("test-key", "test-model");
        config.tools = vec![ToolDefinition {
            name: "file_read_lines".to_string(),
            description: "Reads an inclusive line range.".to_string(),
            capability: ToolCapability::Read,
        }];
        let client = OpenAiCompatibleModelClient::new(config).expect("client");

        let tools = client.build_tool_definitions().expect("tool definitions");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].kind, "function");
        assert_eq!(tools[0].function.name, "file_read_lines");
        assert_eq!(
            tools[0].function.parameters.get("required"),
            Some(&json!(["path", "start_line", "end_line"]))
        );
    }

    #[test]
    fn provider_error_summary_prefers_message_fields() {
        let body = r#"{"error":{"message":"quota exceeded"}}"#;
        assert_eq!(summarize_provider_error_body(body), "quota exceeded");
    }

    #[test]
    fn retry_policy_marks_common_provider_failures_retryable() {
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn provider_status_errors_include_status_and_summary() {
        let message = format_provider_status_error(
            StatusCode::BAD_GATEWAY,
            r#"{"message":"temporary upstream failure"}"#,
        );
        assert!(message.contains("502"));
        assert!(message.contains("temporary upstream failure"));
    }

    #[test]
    fn retry_backoff_grows_exponentially() {
        assert_eq!(
            backoff_for_attempt(Duration::from_millis(250), 0),
            Duration::from_millis(250)
        );
        assert_eq!(
            backoff_for_attempt(Duration::from_millis(250), 1),
            Duration::from_millis(500)
        );
        assert_eq!(
            backoff_for_attempt(Duration::from_millis(250), 2),
            Duration::from_millis(1000)
        );
    }
}
