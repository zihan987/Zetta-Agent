use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use zetta_protocol::{MessageRole, SessionSnapshot};

use super::{
    parse_tool_call_from_user_input, render_tool_result_for_model, ModelClient, ModelStreamSink,
    ParsedToolCall, PlannedTurn,
};

const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleConfig {
    pub api_key: String,
    pub model: String,
    pub api_base: String,
    pub system_prompt: Option<String>,
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

    async fn request_assistant_text(&self, session: &SessionSnapshot) -> Result<String> {
        let request = ChatCompletionRequest {
            model: self.config.model.clone(),
            messages: self.build_messages(session),
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

        extract_assistant_text(body)
    }

    async fn request_assistant_text_streaming(&self, session: &SessionSnapshot) -> Result<String> {
        let request = ChatCompletionRequest {
            model: self.config.model.clone(),
            messages: self.build_messages(session),
            stream: Some(true),
        };
        let mut response = self
            .send_chat_request(&request, "streaming model request")
            .await
            .context("streaming model request failed")?;
        let mut pending = String::new();
        let mut output = String::new();

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

                if let Some(delta) = parse_sse_data_line(&line)? {
                    output.push_str(&delta);
                }
            }
        }

        let trailing = pending.trim();
        if let Some(delta) = parse_sse_data_line(trailing)? {
            output.push_str(&delta);
        }

        Ok(output.trim().to_string())
    }
}

#[async_trait]
impl ModelClient for OpenAiCompatibleModelClient {
    async fn plan_turn(&self, session: &SessionSnapshot) -> Result<PlannedTurn> {
        let text = self.request_assistant_text(session).await?;
        Ok(planned_turn_from_text(text))
    }

    async fn plan_turn_with_sink(
        &self,
        session: &SessionSnapshot,
        mut sink: Option<&mut dyn ModelStreamSink>,
    ) -> Result<PlannedTurn> {
        let text = match sink {
            Some(_) => self.request_assistant_text_streaming(session).await?,
            None => self.request_assistant_text(session).await?,
        };
        let planned = planned_turn_from_text(text);
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
}

fn extract_assistant_text(response: ChatCompletionResponse) -> Result<String> {
    let Some(choice) = response.choices.into_iter().next() else {
        bail!("model response did not contain any choices");
    };

    let Some(content) = choice.message.content else {
        bail!("model response did not contain assistant text");
    };

    Ok(content.trim().to_string())
}

fn planned_turn_from_text(text: String) -> PlannedTurn {
    match parse_tool_call_from_user_input(&text) {
        ParsedToolCall::Valid(call) => PlannedTurn::ToolCall(call),
        ParsedToolCall::Invalid { error } => PlannedTurn::InvalidToolCall { raw: text, error },
        ParsedToolCall::NotAToolCall => PlannedTurn::AssistantMessage(text),
    }
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

fn parse_sse_data_line(line: &str) -> Result<Option<String>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.starts_with("data:") {
        return Ok(None);
    }

    let payload = trimmed.trim_start_matches("data:").trim();
    if payload == "[DONE]" || payload.is_empty() {
        return Ok(None);
    }

    let chunk: StreamingChatCompletionChunk =
        serde_json::from_str(payload).context("failed to decode streaming response chunk")?;
    let delta = chunk
        .choices
        .into_iter()
        .filter_map(|choice| choice.delta.content)
        .collect::<Vec<_>>()
        .join("");

    if delta.is_empty() {
        Ok(None)
    } else {
        Ok(Some(delta))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zetta_protocol::{Message, MessageRole, SessionId, SessionSnapshot};

    use super::{
        backoff_for_attempt, extract_assistant_text, format_provider_status_error,
        is_retryable_status, parse_sse_data_line, planned_turn_from_text,
        streamed_assistant_output, summarize_provider_error_body, ChatChoice,
        ChatCompletionResponse, ChatMessageResponse, OpenAiCompatibleConfig,
        OpenAiCompatibleModelClient,
    };
    use crate::model::encode_tool_result_message;
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
    fn extract_assistant_text_returns_first_choice() {
        let text = extract_assistant_text(ChatCompletionResponse {
            choices: vec![ChatChoice {
                message: ChatMessageResponse {
                    content: Some("hello remote".to_string()),
                },
            }],
        })
        .expect("assistant text");

        assert_eq!(text, "hello remote");
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
        assert_eq!(delta.as_deref(), Some("hello"));
    }

    #[test]
    fn parse_sse_data_line_ignores_done_marker() {
        let delta = parse_sse_data_line("data: [DONE]").expect("parse done");
        assert!(delta.is_none());
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
