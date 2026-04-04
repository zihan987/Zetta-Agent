use anyhow::anyhow;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use super::{Tool, ToolCapability, ToolInvocationError, ToolUseContext};

pub struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &'static str {
        "file_read"
    }

    fn description(&self) -> &'static str {
        "Reads a file inside the workspace root."
    }

    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }

    async fn invoke(
        &self,
        input: Value,
        context: &ToolUseContext,
    ) -> Result<Value, ToolInvocationError> {
        let raw_path = input.get("path").and_then(Value::as_str).ok_or_else(|| {
            ToolInvocationError::Failed(anyhow!("file_read requires a string `path`"))
        })?;

        let resolved = context
            .permissions()
            .resolve_read_path(context.cwd(), raw_path)?;
        let bytes = fs::read(&resolved)
            .await
            .map_err(anyhow::Error::from)
            .map_err(ToolInvocationError::Failed)?;
        let content = String::from_utf8_lossy(&bytes).into_owned();

        Ok(json!({
            "path": resolved,
            "byte_count": bytes.len(),
            "content": content,
        }))
    }
}
