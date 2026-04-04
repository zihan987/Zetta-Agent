use anyhow::anyhow;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

use super::{Tool, ToolCapability, ToolInvocationError, ToolUseContext};

pub struct FileWriteTool;

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &'static str {
        "file_write"
    }

    fn description(&self) -> &'static str {
        "Writes UTF-8 text to a file inside a writable root."
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
            ToolInvocationError::Failed(anyhow!("file_write requires a string `path`"))
        })?;
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolInvocationError::Failed(anyhow!("file_write requires a string `content`"))
            })?;
        let overwrite = input
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let resolved = context
            .permissions()
            .resolve_write_path(context.cwd(), raw_path)?;

        if !overwrite
            && fs::try_exists(&resolved)
                .await
                .map_err(anyhow::Error::from)
                .map_err(ToolInvocationError::Failed)?
        {
            return Err(ToolInvocationError::Failed(anyhow!(
                "target file already exists and overwrite=false"
            )));
        }

        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(anyhow::Error::from)
                .map_err(ToolInvocationError::Failed)?;
        }

        fs::write(&resolved, content.as_bytes())
            .await
            .map_err(anyhow::Error::from)
            .map_err(ToolInvocationError::Failed)?;

        Ok(json!({
            "path": resolved,
            "byte_count": content.len(),
            "overwrote": overwrite,
        }))
    }
}
