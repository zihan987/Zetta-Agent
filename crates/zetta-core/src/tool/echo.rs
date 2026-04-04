use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolCapability, ToolInvocationError, ToolUseContext};

pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "Returns the provided text unchanged."
    }

    fn capability(&self) -> ToolCapability {
        ToolCapability::Safe
    }

    async fn invoke(
        &self,
        input: Value,
        _context: &ToolUseContext,
    ) -> Result<Value, ToolInvocationError> {
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();

        Ok(json!({
            "echo": text,
        }))
    }
}
