mod openai_compatible;
mod rule_based;
mod tool_transcript;

use anyhow::Result;
use async_trait::async_trait;
use zetta_protocol::{SessionSnapshot, ToolCall};

pub use openai_compatible::{OpenAiCompatibleConfig, OpenAiCompatibleModelClient};
pub use rule_based::{
    parse_tool_call_from_user_input, tool_call_from_user_input, ParsedToolCall,
    RuleBasedModelClient,
};
pub use tool_transcript::{
    encode_tool_denied_message, encode_tool_failed_message, encode_tool_invalid_call_message,
    encode_tool_result_message, render_tool_result_for_model, summarize_tool_result,
};

pub enum PlannedTurn {
    AssistantMessage(String),
    ToolCall(ToolCall),
    InvalidToolCall { raw: String, error: String },
}

pub trait ModelStreamSink: Send {
    fn on_text_delta(&mut self, delta: &str) -> Result<()>;

    fn on_message_end(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
pub trait ModelClient: Send + Sync {
    async fn plan_turn(&self, session: &SessionSnapshot) -> Result<PlannedTurn>;

    async fn plan_turn_with_sink(
        &self,
        session: &SessionSnapshot,
        mut sink: Option<&mut dyn ModelStreamSink>,
    ) -> Result<PlannedTurn> {
        let planned = self.plan_turn(session).await?;
        if let (Some(sink), PlannedTurn::AssistantMessage(content)) = (&mut sink, &planned) {
            sink.on_text_delta(content)?;
            sink.on_message_end()?;
        }
        Ok(planned)
    }
}
