use std::process::Command;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolCapability, ToolInvocationError, ToolUseContext};

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Runs a shell command with zsh -lc in the current working directory."
    }

    fn capability(&self) -> ToolCapability {
        ToolCapability::Execute
    }

    async fn invoke(
        &self,
        input: Value,
        context: &ToolUseContext,
    ) -> Result<Value, ToolInvocationError> {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolInvocationError::Failed(anyhow!("bash tool requires a string `command`"))
            })?;

        context.permissions().check_shell_command(command)?;

        let output = Command::new("zsh")
            .args(["-lc", command])
            .current_dir(context.cwd())
            .output()
            .map_err(anyhow::Error::from)
            .map_err(ToolInvocationError::Failed)?;

        Ok(json!({
            "command": command,
            "success": output.status.success(),
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }))
    }
}
